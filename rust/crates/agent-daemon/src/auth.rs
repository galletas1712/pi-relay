use std::{
    env, fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as StdMutex},
    time::Duration,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const CODEX_REFRESH_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone, Default)]
pub(crate) struct Credentials {
    pub(crate) codex_access_token: Option<String>,
    pub(crate) codex_account_id: Option<String>,
    pub(crate) codex_installation_id: Option<String>,
    pub(crate) anthropic_api_key: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Credentials")
            .field(
                "codex_access_token",
                &self.codex_access_token.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "codex_account_id",
                &self.codex_account_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "codex_installation_id",
                &self.codex_installation_id.as_ref().map(|_| "[REDACTED]"),
            )
            .field(
                "anthropic_api_key",
                &self.anthropic_api_key.as_ref().map(|_| "[REDACTED]"),
            )
            .finish()
    }
}

#[derive(Clone)]
pub(crate) struct CredentialSnapshot {
    generation: u64,
    codex_generation: u64,
    anthropic_generation: u64,
    credentials: Arc<Credentials>,
}

impl CredentialSnapshot {
    #[cfg(test)]
    pub(crate) fn for_tests(credentials: Credentials) -> Self {
        Self {
            generation: 1,
            codex_generation: 1,
            anthropic_generation: 1,
            credentials: Arc::new(credentials),
        }
    }

    #[cfg(test)]
    pub(crate) fn generation(&self) -> u64 {
        self.generation
    }

    pub(crate) fn codex_generation(&self) -> u64 {
        self.codex_generation
    }

    pub(crate) fn anthropic_generation(&self) -> u64 {
        self.anthropic_generation
    }

    pub(crate) fn credentials(&self) -> &Credentials {
        &self.credentials
    }
}

impl fmt::Debug for CredentialSnapshot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialSnapshot")
            .field("generation", &self.generation)
            .field("credentials", &self.credentials)
            .finish()
    }
}

type CredentialLoader = dyn Fn() -> Credentials + Send + Sync;

#[async_trait]
pub(crate) trait CodexCredentialRefresher: Send + Sync {
    async fn refresh(&self, prior: &Credentials) -> Result<Credentials>;
}

struct SystemCodexCredentialRefresher;

#[async_trait]
impl CodexCredentialRefresher for SystemCodexCredentialRefresher {
    async fn refresh(&self, prior: &Credentials) -> Result<Credentials> {
        refresh_codex_credentials(prior).await
    }
}

#[derive(Clone)]
pub(crate) struct CredentialManager {
    current: Arc<StdMutex<CredentialSnapshot>>,
    codex_recovery_lock: Arc<Mutex<()>>,
    anthropic_recovery_lock: Arc<Mutex<()>>,
    loader: Arc<CredentialLoader>,
    codex_refresher: Arc<dyn CodexCredentialRefresher>,
}

impl fmt::Debug for CredentialManager {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialManager")
            .field("generation", &self.snapshot().generation)
            .finish_non_exhaustive()
    }
}

impl CredentialManager {
    pub(crate) fn from_system() -> Self {
        Self::from_dependencies(
            Arc::new(Credentials::load),
            Arc::new(SystemCodexCredentialRefresher),
        )
    }

    fn from_dependencies(
        loader: Arc<CredentialLoader>,
        codex_refresher: Arc<dyn CodexCredentialRefresher>,
    ) -> Self {
        let credentials = Arc::new(loader());
        Self {
            current: Arc::new(StdMutex::new(CredentialSnapshot {
                generation: 1,
                codex_generation: 1,
                anthropic_generation: 1,
                credentials,
            })),
            codex_recovery_lock: Arc::new(Mutex::new(())),
            anthropic_recovery_lock: Arc::new(Mutex::new(())),
            loader,
            codex_refresher,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_tests(credentials: Credentials) -> Self {
        let loader_credentials = credentials.clone();
        Self::from_dependencies(
            Arc::new(move || loader_credentials.clone()),
            Arc::new(SystemCodexCredentialRefresher),
        )
    }

    #[cfg(test)]
    pub(crate) fn for_tests_with(
        loader: Arc<CredentialLoader>,
        codex_refresher: Arc<dyn CodexCredentialRefresher>,
    ) -> Self {
        Self::from_dependencies(loader, codex_refresher)
    }

    pub(crate) fn snapshot(&self) -> CredentialSnapshot {
        self.current
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .clone()
    }

    pub(crate) async fn refresh_codex(
        &self,
        observed: &CredentialSnapshot,
    ) -> Result<CredentialSnapshot> {
        let _recovery = self.codex_recovery_lock.lock().await;
        let current = self.snapshot();
        if current.codex_generation > observed.codex_generation {
            return Ok(current);
        }

        let refreshed = self.codex_refresher.refresh(current.credentials()).await?;
        if refreshed.codex_access_token.is_none() {
            return Err(anyhow!(
                "Codex token refresh did not produce an access token"
            ));
        }
        self.publish_codex_after(current.codex_generation, refreshed)
    }

    pub(crate) async fn reload_anthropic(
        &self,
        observed: &CredentialSnapshot,
    ) -> Result<Option<CredentialSnapshot>> {
        let _recovery = self.anthropic_recovery_lock.lock().await;
        let current = self.snapshot();
        if current.anthropic_generation > observed.anthropic_generation {
            return Ok(Some(current));
        }

        let loaded = (self.loader)();
        let Some(loaded_api_key) = loaded.anthropic_api_key else {
            return Ok(None);
        };
        if current.credentials.anthropic_api_key.as_ref() == Some(&loaded_api_key) {
            return Ok(None);
        }
        Ok(Some(self.publish_anthropic_after(
            current.anthropic_generation,
            loaded_api_key,
        )?))
    }

    fn publish_codex_after(
        &self,
        expected_codex_generation: u64,
        refreshed: Credentials,
    ) -> Result<CredentialSnapshot> {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if current.codex_generation != expected_codex_generation {
            return Err(anyhow!(
                "Codex credential generation changed while publishing authentication recovery"
            ));
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("credential generation overflow"))?;
        let codex_generation = current
            .codex_generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Codex credential generation overflow"))?;
        let mut credentials = current.credentials.as_ref().clone();
        credentials.codex_access_token = refreshed.codex_access_token;
        credentials.codex_account_id = refreshed.codex_account_id;
        credentials.codex_installation_id = refreshed.codex_installation_id;
        let replacement = CredentialSnapshot {
            generation,
            codex_generation,
            anthropic_generation: current.anthropic_generation,
            credentials: Arc::new(credentials),
        };
        *current = replacement.clone();
        Ok(replacement)
    }

    fn publish_anthropic_after(
        &self,
        expected_anthropic_generation: u64,
        api_key: String,
    ) -> Result<CredentialSnapshot> {
        let mut current = self
            .current
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if current.anthropic_generation != expected_anthropic_generation {
            return Err(anyhow!(
                "Anthropic credential generation changed while publishing authentication recovery"
            ));
        }
        let generation = current
            .generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("credential generation overflow"))?;
        let anthropic_generation = current
            .anthropic_generation
            .checked_add(1)
            .ok_or_else(|| anyhow!("Anthropic credential generation overflow"))?;
        let mut credentials = current.credentials.as_ref().clone();
        credentials.anthropic_api_key = Some(api_key);
        let replacement = CredentialSnapshot {
            generation,
            codex_generation: current.codex_generation,
            anthropic_generation,
            credentials: Arc::new(credentials),
        };
        *current = replacement.clone();
        Ok(replacement)
    }
}

impl Credentials {
    fn load() -> Self {
        let codex = read_codex_auth();
        Self {
            codex_access_token: env::var("CODEX_ACCESS_TOKEN")
                .ok()
                .or_else(|| codex.access_token.clone()),
            codex_account_id: codex.account_id,
            codex_installation_id: read_codex_installation_id(),
            anthropic_api_key: env::var("ANTHROPIC_API_KEY")
                .ok()
                .or_else(read_claude_code_config_api_key),
        }
    }
}

/// Read the persistent Codex installation id from `~/.codex/installation_id`,
/// matching the format the Codex CLI maintains. This file is a UUID v4 that
/// the CLI rotates with `codex login`, so pi-relay can piggy-back on it for
/// the `x-codex-installation-id` header without inventing a separate identity.
///
/// Returns `None` when the file is missing or unreadable; the caller can fall
/// back to omitting the header (Codex backend tolerates its absence).
fn read_codex_installation_id() -> Option<String> {
    let path = env::var("HOME")
        .ok()
        .map(PathBuf::from)?
        .join(".codex/installation_id");
    let contents = std::fs::read_to_string(&path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_claude_code_config_api_key() -> Option<String> {
    let home = env::var("HOME").ok().filter(|value| !value.is_empty())?;
    read_claude_code_config_api_key_from_home(Path::new(&home))
}

fn read_claude_code_config_api_key_from_home(home: &Path) -> Option<String> {
    let paths = [home.join(".claude/config.json"), home.join(".claude.json")];

    for path in paths {
        let Ok(contents) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<Value>(&contents) else {
            continue;
        };
        let Some(key) = value
            .get("primaryApiKey")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|key| key.starts_with("sk-ant-"))
        else {
            continue;
        };
        return Some(key.to_string());
    }

    None
}

#[derive(Default)]
struct CodexAuthSnapshot {
    path: Option<PathBuf>,
    access_token: Option<String>,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

fn read_codex_auth() -> CodexAuthSnapshot {
    let path = env::var("HOME")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".codex/auth.json");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return CodexAuthSnapshot::default();
    };
    let Ok(value) = serde_json::from_str::<Value>(&contents) else {
        return CodexAuthSnapshot::default();
    };
    CodexAuthSnapshot {
        path: Some(path),
        access_token: value
            .pointer("/tokens/access_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .map(ToOwned::to_owned),
        refresh_token: value
            .pointer("/tokens/refresh_token")
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty())
            .map(ToOwned::to_owned),
        account_id: value
            .pointer("/tokens/account_id")
            .and_then(Value::as_str)
            .filter(|account| !account.trim().is_empty())
            .map(ToOwned::to_owned),
    }
}

#[derive(Serialize)]
struct CodexRefreshRequest {
    client_id: &'static str,
    grant_type: &'static str,
    refresh_token: String,
}

#[derive(Deserialize)]
struct CodexRefreshResponse {
    id_token: Option<String>,
    access_token: Option<String>,
    refresh_token: Option<String>,
}

async fn refresh_codex_credentials(prior: &Credentials) -> Result<Credentials> {
    let snapshot = read_codex_auth();
    let refresh_token = env::var("CODEX_REFRESH_TOKEN")
        .ok()
        .or_else(|| snapshot.refresh_token.clone())
        .ok_or_else(|| {
            anyhow!("Codex provider returned 401 and no ChatGPT refresh token was found")
        })?;

    let response = reqwest::Client::new()
        .post(CODEX_REFRESH_TOKEN_URL)
        .timeout(CODEX_REFRESH_TIMEOUT)
        .header("Content-Type", "application/json")
        .json(&CodexRefreshRequest {
            client_id: CODEX_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .await
        .map_err(|_| anyhow!("Codex token refresh request failed"))?;

    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!(
            "Codex token refresh failed with HTTP {}",
            status.as_u16()
        ));
    }

    let refreshed = response
        .json::<CodexRefreshResponse>()
        .await
        .map_err(|_| anyhow!("Codex token refresh response was invalid"))?;
    let mut refreshed_snapshot = snapshot;
    if let Some(path) = refreshed_snapshot.path.clone() {
        refreshed_snapshot = persist_codex_refresh(&path, &refreshed)?;
    } else {
        refreshed_snapshot.access_token = refreshed.access_token;
        refreshed_snapshot.refresh_token = refreshed.refresh_token;
    }

    let mut credentials = prior.clone();
    credentials.codex_access_token = refreshed_snapshot.access_token;
    credentials.codex_account_id = refreshed_snapshot.account_id;
    credentials.codex_installation_id = read_codex_installation_id();
    if credentials.codex_access_token.is_none() {
        return Err(anyhow!(
            "Codex token refresh did not produce an access token"
        ));
    }
    Ok(credentials)
}

fn persist_codex_refresh(
    path: &Path,
    refreshed: &CodexRefreshResponse,
) -> Result<CodexAuthSnapshot> {
    let contents = std::fs::read_to_string(path)?;
    let mut value = serde_json::from_str::<Value>(&contents)?;
    if !value.get("tokens").is_some_and(Value::is_object) {
        value["tokens"] = json!({});
    }

    let tokens = value
        .get_mut("tokens")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow!("~/.codex/auth.json tokens field is not an object"))?;
    if let Some(id_token) = &refreshed.id_token {
        tokens.insert("id_token".to_string(), Value::String(id_token.clone()));
    }
    if let Some(access_token) = &refreshed.access_token {
        tokens.insert(
            "access_token".to_string(),
            Value::String(access_token.clone()),
        );
    }
    if let Some(refresh_token) = &refreshed.refresh_token {
        tokens.insert(
            "refresh_token".to_string(),
            Value::String(refresh_token.clone()),
        );
    }
    value["last_refresh"] = Value::String(
        time::OffsetDateTime::now_utc()
            .format(&time::format_description::well_known::Rfc3339)
            .map_err(|error| anyhow!("failed to format refresh timestamp: {error}"))?,
    );

    let tmp_path = path.with_file_name(format!(
        "{}.tmp",
        path.file_name()
            .and_then(|file_name| file_name.to_str())
            .unwrap_or("auth.json")
    ));
    let serialized = serde_json::to_vec_pretty(&value)?;
    let permissions = std::fs::metadata(path)
        .ok()
        .map(|metadata| metadata.permissions());
    std::fs::write(&tmp_path, serialized)?;
    if let Some(permissions) = permissions {
        std::fs::set_permissions(&tmp_path, permissions)?;
    }
    std::fs::rename(&tmp_path, path)?;
    Ok(read_codex_auth())
}

#[cfg(test)]
#[path = "auth_tests.rs"]
mod tests;
