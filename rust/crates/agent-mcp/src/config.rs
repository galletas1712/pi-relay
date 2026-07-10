use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};

#[path = "oauth_config.rs"]
mod oauth_config;

pub use oauth_config::McpHttpAuthConfig;

const DEFAULT_STARTUP_TIMEOUT_MS: u64 = 10_000;
const DEFAULT_CALL_TIMEOUT_MS: u64 = 30_000;
const MAX_TIMEOUT_MS: u64 = 10 * 60 * 1_000;
const MAX_SERVERS: usize = 64;
const MAX_PARALLEL_CALLS: usize = 32;
const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_COMMAND_BYTES: usize = 4 * 1024;
const MAX_CWD_BYTES: usize = 16 * 1024;
const MAX_URL_BYTES: usize = 16 * 1024;
const MAX_ARGS: usize = 256;
const MAX_ARG_BYTES: usize = 16 * 1024;
const MAX_TOTAL_ARG_BYTES: usize = 128 * 1024;
const MAX_ENV_ENTRIES: usize = 128;
const MAX_ENV_VALUE_BYTES: usize = 16 * 1024;
const MAX_INHERITED_ENV: usize = 128;
const MAX_ENABLED_TOOLS: usize = 512;
const MAX_TOOL_NAME_BYTES: usize = 256;

#[derive(Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpConfig {
    #[serde(default)]
    pub servers: BTreeMap<String, McpServerConfig>,
}

fn secret_like_env_name(name: &str) -> bool {
    let uppercase = name.to_ascii_uppercase();
    let components = uppercase.split('_').collect::<BTreeSet<_>>();
    [
        "TOKEN",
        "SECRET",
        "PASSWORD",
        "CREDENTIAL",
        "API_KEY",
        "ACCESS_KEY",
        "PRIVATE_KEY",
        "AUTH",
        "COOKIE",
        "SESSION",
    ]
    .iter()
    .any(|fragment| uppercase.contains(fragment))
        || ["PAT", "BEARER", "SSH"]
            .iter()
            .any(|component| components.contains(component))
        || [
            "DATABASE_URL",
            "DB_URL",
            "POSTGRES_URL",
            "POSTGRESQL_URL",
            "MYSQL_URL",
            "MARIADB_URL",
            "MONGODB_URI",
            "REDIS_URL",
        ]
        .contains(&uppercase.as_str())
}

impl fmt::Debug for McpConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpConfig")
            .field("servers", &self.servers)
            .finish()
    }
}

impl McpTransportConfig {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Stdio(config) => config.validate(),
            Self::StreamableHttp(config) => config.validate(),
        }
    }
}

impl McpStdioTransportConfig {
    fn validate(&self) -> Result<()> {
        if self.command.trim().is_empty() || self.command.len() > MAX_COMMAND_BYTES {
            bail!("command must contain between 1 and {MAX_COMMAND_BYTES} bytes");
        }
        if self
            .cwd
            .as_ref()
            .is_some_and(|cwd| cwd.len() > MAX_CWD_BYTES)
        {
            bail!("cwd exceeds {MAX_CWD_BYTES} bytes");
        }
        if self.args.len() > MAX_ARGS
            || self.args.iter().any(|arg| arg.len() > MAX_ARG_BYTES)
            || self.args.iter().map(String::len).sum::<usize>() > MAX_TOTAL_ARG_BYTES
        {
            bail!("MCP command arguments exceed configured bounds");
        }
        if self.env.len() > MAX_ENV_ENTRIES
            || self
                .env
                .values()
                .any(|value| value.len() > MAX_ENV_VALUE_BYTES)
        {
            bail!("literal environment exceeds configured bounds");
        }
        if self.inherit_env.len() > MAX_INHERITED_ENV {
            bail!("inherit_env has more than {MAX_INHERITED_ENV} entries");
        }
        for name in self.env.keys().chain(self.inherit_env.iter()) {
            validate_env_name(name)?;
        }
        if let Some(name) = self.env.keys().find(|name| secret_like_env_name(name)) {
            bail!(
                "secret-like environment variable {name} must use inherit_env instead of literal env"
            );
        }
        if self.env.keys().any(|name| self.inherit_env.contains(name)) {
            bail!("an environment name cannot appear in both env and inherit_env");
        }
        Ok(())
    }
}

impl McpStreamableHttpTransportConfig {
    fn validate(&self) -> Result<()> {
        if self.url.len() > MAX_URL_BYTES {
            bail!("Streamable HTTP URL exceeds {MAX_URL_BYTES} bytes");
        }
        let url = reqwest::Url::parse(&self.url).context("parse Streamable HTTP URL")?;
        if !url.username().is_empty() || url.password().is_some() {
            bail!("Streamable HTTP URL must not contain credentials");
        }
        let host = url
            .host_str()
            .ok_or_else(|| anyhow::anyhow!("Streamable HTTP URL must have a host"))?;
        let loopback = host.eq_ignore_ascii_case("localhost")
            || host
                .trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .is_ok_and(|address| address.is_loopback());
        if url.scheme() != "https" && !(url.scheme() == "http" && loopback) {
            bail!("Streamable HTTP URL must use HTTPS (HTTP is limited to loopback hosts)");
        }
        if url.fragment().is_some() {
            bail!("Streamable HTTP URL must not contain a fragment");
        }
        if let Some(auth) = &self.auth {
            auth.validate()?;
        }
        Ok(())
    }

    pub(crate) fn canonical_url(&self) -> Result<reqwest::Url> {
        reqwest::Url::parse(&self.url).context("parse Streamable HTTP URL")
    }
}

impl McpConfig {
    pub fn from_path(path: &Path) -> Result<Self> {
        let file =
            File::open(path).with_context(|| format!("open MCP config {}", path.display()))?;
        let metadata = file
            .metadata()
            .with_context(|| format!("inspect MCP config {}", path.display()))?;
        if metadata.len() > MAX_CONFIG_BYTES {
            bail!("MCP config exceeds {MAX_CONFIG_BYTES} bytes");
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_CONFIG_BYTES + 1)
            .read_to_end(&mut bytes)
            .with_context(|| format!("read MCP config {}", path.display()))?;
        if bytes.len() as u64 > MAX_CONFIG_BYTES {
            bail!("MCP config exceeds {MAX_CONFIG_BYTES} bytes");
        }
        let config: Self = serde_json::from_slice(&bytes)
            .with_context(|| format!("parse MCP config {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        if self.servers.len() > MAX_SERVERS {
            bail!("MCP config has more than {MAX_SERVERS} servers");
        }
        for (server_id, server) in &self.servers {
            validate_server_id(server_id)?;
            server
                .validate()
                .with_context(|| format!("invalid MCP server {server_id}"))?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn semantic_fingerprint_input(&self) -> serde_json::Value {
        let servers = self
            .servers
            .iter()
            .map(|(server_id, server)| (server_id.clone(), server.semantic_fingerprint_input()))
            .collect::<serde_json::Map<_, _>>();
        serde_json::json!({ "servers": servers })
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct McpServerConfig {
    pub transport: McpTransportConfig,
    pub startup_timeout_ms: u64,
    pub call_timeout_ms: u64,
    pub parallel_calls: usize,
    pub allow_all_tools: bool,
    pub enabled_tools: BTreeSet<String>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum McpTransportConfig {
    Stdio(McpStdioTransportConfig),
    StreamableHttp(McpStreamableHttpTransportConfig),
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpStdioTransportConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: Option<String>,
    /// Literal, non-secret environment values. Secrets must use
    /// `inherit_env`, so their values never enter configuration-derived data.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub inherit_env: BTreeSet<String>,
}

#[derive(Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpStreamableHttpTransportConfig {
    pub url: String,
    #[serde(default)]
    pub auth: Option<McpHttpAuthConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TaggedMcpServerConfig {
    transport: McpTransportConfig,
    #[serde(default = "default_startup_timeout_ms")]
    startup_timeout_ms: u64,
    #[serde(default = "default_call_timeout_ms")]
    call_timeout_ms: u64,
    #[serde(default = "default_parallel_calls")]
    parallel_calls: usize,
    #[serde(default)]
    allow_all_tools: bool,
    #[serde(default)]
    enabled_tools: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyMcpServerConfig {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    inherit_env: BTreeSet<String>,
    #[serde(default = "default_startup_timeout_ms")]
    startup_timeout_ms: u64,
    #[serde(default = "default_call_timeout_ms")]
    call_timeout_ms: u64,
    #[serde(default = "default_parallel_calls")]
    parallel_calls: usize,
    #[serde(default)]
    allow_all_tools: bool,
    #[serde(default)]
    enabled_tools: BTreeSet<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum McpServerConfigWire {
    Tagged(TaggedMcpServerConfig),
    Legacy(LegacyMcpServerConfig),
}

impl<'de> Deserialize<'de> for McpServerConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(match McpServerConfigWire::deserialize(deserializer)? {
            McpServerConfigWire::Tagged(config) => Self {
                transport: config.transport,
                startup_timeout_ms: config.startup_timeout_ms,
                call_timeout_ms: config.call_timeout_ms,
                parallel_calls: config.parallel_calls,
                allow_all_tools: config.allow_all_tools,
                enabled_tools: config.enabled_tools,
            },
            McpServerConfigWire::Legacy(config) => Self {
                transport: McpTransportConfig::Stdio(McpStdioTransportConfig {
                    command: config.command,
                    args: config.args,
                    cwd: config.cwd,
                    env: config.env,
                    inherit_env: config.inherit_env,
                }),
                startup_timeout_ms: config.startup_timeout_ms,
                call_timeout_ms: config.call_timeout_ms,
                parallel_calls: config.parallel_calls,
                allow_all_tools: config.allow_all_tools,
                enabled_tools: config.enabled_tools,
            },
        })
    }
}

impl fmt::Debug for McpServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpServerConfig")
            .field("transport", &self.transport)
            .field("startup_timeout_ms", &self.startup_timeout_ms)
            .field("call_timeout_ms", &self.call_timeout_ms)
            .field("parallel_calls", &self.parallel_calls)
            .field("allow_all_tools", &self.allow_all_tools)
            .field("enabled_tools", &self.enabled_tools)
            .finish()
    }
}

impl fmt::Debug for McpTransportConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stdio(config) => formatter
                .debug_struct("Stdio")
                .field("command", &"<redacted>")
                .field("args_count", &config.args.len())
                .field("cwd", &config.cwd.as_ref().map(|_| "<redacted>"))
                .field("env_names", &config.env.keys().collect::<Vec<_>>())
                .field("inherit_env", &config.inherit_env)
                .finish(),
            Self::StreamableHttp(config) => formatter
                .debug_struct("StreamableHttp")
                .field("url", &"<redacted>")
                .field("auth", &config.auth)
                .finish(),
        }
    }
}

impl McpServerConfig {
    pub(crate) fn semantic_fingerprint(&self) -> String {
        crate::fingerprint_json(&self.semantic_fingerprint_input())
    }

    fn semantic_fingerprint_input(&self) -> serde_json::Value {
        match &self.transport {
            McpTransportConfig::Stdio(config) => {
                let env_hashes = config
                    .env
                    .iter()
                    .map(|(name, value)| {
                        let digest = Sha256::digest(value.as_bytes());
                        (name.clone(), format!("{digest:x}"))
                    })
                    .collect::<BTreeMap<_, _>>();
                serde_json::json!({
                    "command": config.command,
                    "args": config.args,
                    "cwd": config.cwd,
                    "env_hashes": env_hashes,
                    "inherit_env": config.inherit_env,
                    "parallel_calls": self.parallel_calls,
                    "allow_all_tools": self.allow_all_tools,
                    "enabled_tools": self.enabled_tools,
                })
            }
            McpTransportConfig::StreamableHttp(config) => {
                let transport = if let Some(McpHttpAuthConfig::Oauth {
                    client_id,
                    scopes,
                    resource,
                    ..
                }) = &config.auth
                {
                    let client_id = client_id
                        .as_deref()
                        .map(str::trim)
                        .filter(|client_id| !client_id.is_empty());
                    serde_json::json!({
                        "type": "streamable_http",
                        "url": config
                            .canonical_url()
                            .expect("validated Streamable HTTP URL")
                            .as_str(),
                        "auth": {
                            "type": "oauth",
                            "client_id": client_id,
                            "scopes": scopes,
                            "resource": resource,
                        },
                    })
                } else {
                    // Preserve the pre-OAuth Streamable HTTP fingerprint bytes.
                    serde_json::json!({
                        "type": "streamable_http",
                        "url": config.url,
                        "auth": config.auth,
                    })
                };
                serde_json::json!({
                    "transport": transport,
                    "parallel_calls": self.parallel_calls,
                    "allow_all_tools": self.allow_all_tools,
                    "enabled_tools": self.enabled_tools,
                })
            }
        }
    }

    fn validate(&self) -> Result<()> {
        self.transport.validate()?;
        if !self.allow_all_tools && self.enabled_tools.is_empty() {
            bail!("enabled_tools is required unless allow_all_tools is true");
        }
        if self.parallel_calls == 0 || self.parallel_calls > MAX_PARALLEL_CALLS {
            bail!("parallel_calls must be between 1 and {MAX_PARALLEL_CALLS}");
        }
        for (name, value) in [
            ("startup_timeout_ms", self.startup_timeout_ms),
            ("call_timeout_ms", self.call_timeout_ms),
        ] {
            if value == 0 || value > MAX_TIMEOUT_MS {
                bail!("{name} must be between 1 and {MAX_TIMEOUT_MS}");
            }
        }
        if self.enabled_tools.len() > MAX_ENABLED_TOOLS
            || self
                .enabled_tools
                .iter()
                .any(|name| name.is_empty() || name.len() > MAX_TOOL_NAME_BYTES)
        {
            bail!("enabled_tools exceeds configured bounds");
        }
        Ok(())
    }

    pub(crate) fn tool_enabled(&self, name: &str) -> bool {
        self.allow_all_tools || self.enabled_tools.contains(name)
    }

    pub(crate) fn startup_timeout(&self) -> Duration {
        Duration::from_millis(self.startup_timeout_ms)
    }

    pub(crate) fn call_timeout(&self) -> Duration {
        Duration::from_millis(self.call_timeout_ms)
    }
}

fn default_startup_timeout_ms() -> u64 {
    DEFAULT_STARTUP_TIMEOUT_MS
}

fn default_call_timeout_ms() -> u64 {
    DEFAULT_CALL_TIMEOUT_MS
}

fn default_parallel_calls() -> usize {
    1
}

fn validate_server_id(server_id: &str) -> Result<()> {
    if server_id.is_empty() || server_id.len() > 128 {
        bail!("server id must contain between 1 and 128 bytes");
    }
    if server_id.chars().any(char::is_control) {
        bail!("server id must not contain control characters");
    }
    Ok(())
}

pub(super) fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    if !chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        || chars.any(|ch| ch != '_' && !ch.is_ascii_alphanumeric())
    {
        bail!("invalid environment variable name {name:?}");
    }
    Ok(())
}

#[cfg(test)]
#[path = "config_tests.rs"]
mod tests;
