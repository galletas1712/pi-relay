use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use agent_vocab::{ProviderConfig, ProviderKind, ReasoningEffort};
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use serde_json::Value;

const DAEMON_CONFIG_FILE: &str = "config.toml";
const PRODUCT_CONFIG_DIR: &str = "pi-relay";
const DAEMON_CONFIG_DIR: &str = "agentd";
const DEFAULT_BIND: &str = "127.0.0.1:8787";
const DEFAULT_RUNTIME_BIND: &str = "127.0.0.1:8786";

#[derive(Debug, Clone)]
pub(crate) struct Config {
    pub(crate) database_url: String,
    pub(crate) bind: String,
    pub(crate) runtime_bind: String,
    pub(crate) config_root: PathBuf,
    pub(crate) daemon_config: DaemonConfig,
}

/// General daemon settings stored in
/// `$XDG_CONFIG_HOME/pi-relay/agentd/config.toml` (or
/// `$HOME/.config/pi-relay/agentd/config.toml`).
#[derive(Debug, Clone)]
pub(crate) struct DaemonConfig {
    pub(crate) default_parent_model: ProviderConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            default_parent_model: stable_default_provider(),
        }
    }
}

pub(crate) fn stable_default_provider() -> ProviderConfig {
    ProviderConfig {
        kind: ProviderKind::OpenAi,
        model: "gpt-5.6-sol".to_string(),
        reasoning_effort: ReasoningEffort::High,
        max_tokens: None,
        prompt_cache: None,
    }
}

impl Config {
    pub(crate) fn from_env_and_args() -> Result<Self> {
        Self::from_values(
            env::var_os("XDG_CONFIG_HOME"),
            env::var_os("HOME"),
            env::args().skip(1).collect(),
        )
    }

    fn from_values(
        xdg_config_home: Option<std::ffi::OsString>,
        home: Option<std::ffi::OsString>,
        args: Vec<String>,
    ) -> Result<Self> {
        if let Some(argument) = args.first() {
            return Err(anyhow!(
                "pi-agentd accepts no arguments; configure it in {DAEMON_CONFIG_FILE} (unknown argument: {argument})"
            ));
        }

        let config_root = config_root_from_env(xdg_config_home.as_deref(), home.as_deref())?;
        let policy = load_daemon_config(&config_root.join(DAEMON_CONFIG_FILE))?;

        Ok(Self {
            database_url: policy.database_url,
            bind: policy.bind,
            runtime_bind: policy.runtime_bind,
            config_root,
            daemon_config: policy.daemon_config,
        })
    }
}

fn config_root_from_env(
    xdg_config_home: Option<&std::ffi::OsStr>,
    home: Option<&std::ffi::OsStr>,
) -> Result<PathBuf> {
    if let Some(xdg_config_home) = xdg_config_home.filter(|value| !value.is_empty()) {
        let config_home = PathBuf::from(xdg_config_home);
        if !config_home.is_absolute() {
            return Err(anyhow!("XDG_CONFIG_HOME must be an absolute path"));
        }
        return Ok(config_home.join(PRODUCT_CONFIG_DIR).join(DAEMON_CONFIG_DIR));
    }
    let home = home
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow!("HOME is required when XDG_CONFIG_HOME is unset"))?;
    let home = PathBuf::from(home);
    if !home.is_absolute() {
        return Err(anyhow!("HOME must be an absolute path"));
    }
    Ok(home
        .join(".config")
        .join(PRODUCT_CONFIG_DIR)
        .join(DAEMON_CONFIG_DIR))
}

fn load_daemon_config(path: &Path) -> Result<DaemonStartupPolicy> {
    let bytes = fs::read(path)
        .with_context(|| format!("read required daemon config {}", path.display()))?;
    let parsed: DaemonConfigFile = toml::from_str(
        std::str::from_utf8(&bytes)
            .with_context(|| format!("parse daemon config {}", path.display()))?,
    )
    .with_context(|| format!("parse daemon config {}", path.display()))?;
    parsed
        .try_into()
        .with_context(|| format!("validate daemon config {}", path.display()))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DaemonConfigFile {
    database_url: Option<String>,
    #[serde(default)]
    bind: Option<String>,
    #[serde(default)]
    runtime_bind: Option<String>,
    #[serde(default)]
    default_parent_model: Option<StrictProviderConfig>,
}

#[derive(Debug)]
struct DaemonStartupPolicy {
    database_url: String,
    bind: String,
    runtime_bind: String,
    daemon_config: DaemonConfig,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictProviderConfig {
    kind: ProviderKind,
    model: String,
    #[serde(default)]
    reasoning_effort: ReasoningEffort,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    prompt_cache: Option<Value>,
}

impl TryFrom<DaemonConfigFile> for DaemonStartupPolicy {
    type Error = anyhow::Error;

    fn try_from(value: DaemonConfigFile) -> Result<Self> {
        let database_url = value
            .database_url
            .ok_or_else(|| anyhow!("database_url is required"))?;
        if database_url.trim().is_empty() {
            return Err(anyhow!("database_url must not be blank"));
        }

        let bind = value.bind.unwrap_or_else(|| DEFAULT_BIND.to_string());
        if bind.trim().is_empty() {
            return Err(anyhow!("bind must not be blank"));
        }
        let runtime_bind = value
            .runtime_bind
            .unwrap_or_else(|| DEFAULT_RUNTIME_BIND.to_string());
        if runtime_bind.trim().is_empty() {
            return Err(anyhow!("runtime_bind must not be blank"));
        }

        let default_parent_model = match value.default_parent_model {
            Some(provider) => provider_from_strict("default_parent_model", provider)?,
            None => stable_default_provider(),
        };
        Ok(Self {
            database_url,
            bind,
            runtime_bind,
            daemon_config: DaemonConfig {
                default_parent_model,
            },
        })
    }
}

fn provider_from_strict(field: &str, provider: StrictProviderConfig) -> Result<ProviderConfig> {
    if provider.model.trim().is_empty() {
        return Err(anyhow!("{field}.model must not be blank"));
    }
    Ok(ProviderConfig {
        kind: provider.kind,
        model: provider.model,
        reasoning_effort: provider.reasoning_effort,
        max_tokens: provider.max_tokens,
        prompt_cache: provider.prompt_cache,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn resolves_component_scoped_xdg_config_root() {
        assert_eq!(
            config_root_from_env(Some("/tmp/xdg".as_ref()), Some("/home/test".as_ref()))
                .expect("config root"),
            PathBuf::from("/tmp/xdg/pi-relay/agentd")
        );
        assert_eq!(
            config_root_from_env(None, Some("/home/test".as_ref())).expect("config root"),
            PathBuf::from("/home/test/.config/pi-relay/agentd")
        );
        assert!(config_root_from_env(Some("relative".as_ref()), None).is_err());
        assert!(config_root_from_env(None, Some("relative".as_ref())).is_err());
    }

    #[test]
    fn config_is_strict_and_preserves_parent_provider() {
        let root = make_temp_dir("strict-config");
        let config = root.join(DAEMON_CONFIG_FILE);
        fs::write(
            &config,
            r#"
database_url = "postgres://example"
bind = "127.0.0.1:9999"
runtime_bind = "127.0.0.1:9998"

[default_parent_model]
kind = "claude"
model = "parent"
reasoning_effort = "high"
max_tokens = 123
prompt_cache = { key = "parent-cache" }
"#,
        )
        .expect("write config");

        let loaded = load_daemon_config(&config).expect("parse config");
        assert_eq!(loaded.database_url, "postgres://example");
        assert_eq!(loaded.bind, "127.0.0.1:9999");
        assert_eq!(loaded.runtime_bind, "127.0.0.1:9998");
        assert_eq!(
            loaded.daemon_config.default_parent_model.kind,
            ProviderKind::Claude
        );
        assert_eq!(
            loaded.daemon_config.default_parent_model.max_tokens,
            Some(123)
        );
        assert_eq!(
            loaded.daemon_config.default_parent_model.prompt_cache,
            Some(serde_json::json!({"key": "parent-cache"}))
        );

        fs::write(
            &config,
            "database_url = \"postgres://example\"\n[subagent_models.reviewer]\nkind = \"claude\"\nmodel = \"reviewer\"\n",
        )
        .expect("write removed field");
        let error = load_daemon_config(&config).expect_err("subagent model map is removed");
        assert!(format!("{error:#}").contains("unknown field"));
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn config_requires_database_url_and_defaults_policy() {
        let root = make_temp_dir("default-config");
        let config = root.join(DAEMON_CONFIG_FILE);
        let error = load_daemon_config(&config).expect_err("missing config is rejected");
        assert!(format!("{error:#}").contains("read required daemon config"));

        fs::write(&config, "").expect("empty config");
        let error = load_daemon_config(&config).expect_err("database URL is required");
        assert!(format!("{error:#}").contains("database_url is required"));

        fs::write(&config, "database_url = \"postgres://example\"\n").expect("default config");
        let loaded = load_daemon_config(&config).expect("default policy");
        assert_eq!(loaded.bind, DEFAULT_BIND);
        assert_eq!(loaded.runtime_bind, DEFAULT_RUNTIME_BIND);
        let default = loaded.daemon_config.default_parent_model;
        assert_eq!(default.kind, ProviderKind::OpenAi);
        assert_eq!(default.model, "gpt-5.6-sol");
        assert_eq!(default.reasoning_effort, ReasoningEffort::High);
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn daemon_accepts_no_arguments() {
        let error = Config::from_values(
            None,
            Some("/home/test".into()),
            vec!["old-config.toml".to_string()],
        )
        .expect_err("daemon rejects arguments");
        assert!(format!("{error:#}").contains("pi-agentd accepts no arguments"));
    }

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "pi-relay-agentd-config-{prefix}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&path).expect("temp directory");
        path
    }
}
