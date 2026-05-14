use std::{
    env,
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const CODEX_REFRESH_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

#[derive(Debug, Clone, Default)]
pub(crate) struct Credentials {
    pub(crate) codex_access_token: Option<String>,
    pub(crate) codex_account_id: Option<String>,
    pub(crate) codex_installation_id: Option<String>,
    pub(crate) anthropic_api_key: Option<String>,
}

impl Credentials {
    pub(crate) fn load() -> Self {
        let codex = read_codex_auth();
        Self {
            codex_access_token: env::var("CODEX_ACCESS_TOKEN")
                .ok()
                .or_else(|| codex.access_token.clone()),
            codex_account_id: codex.account_id,
            codex_installation_id: read_codex_installation_id(),
            anthropic_api_key: env::var("ANTHROPIC_API_KEY")
                .ok()
                .or_else(read_claude_code_keychain_api_key),
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
    let path = env::var("HOME").ok().map(PathBuf::from)?.join(".codex/installation_id");
    let contents = std::fs::read_to_string(&path).ok()?;
    let trimmed = contents.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn read_claude_code_keychain_api_key() -> Option<String> {
    if !cfg!(target_os = "macos") {
        return None;
    }

    let username = env::var("USER").ok().filter(|value| !value.is_empty())?;
    let output = Command::new("security")
        .args([
            "find-generic-password",
            "-a",
            &username,
            "-w",
            "-s",
            "Claude Code",
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }

    String::from_utf8(output.stdout)
        .ok()
        .map(|key| key.trim().to_string())
        .filter(|key| key.starts_with("sk-ant-"))
}

#[derive(Debug, Default)]
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

pub(crate) async fn refresh_codex_credentials() -> Result<Credentials> {
    let snapshot = read_codex_auth();
    let refresh_token = env::var("CODEX_REFRESH_TOKEN")
        .ok()
        .or_else(|| snapshot.refresh_token.clone())
        .ok_or_else(|| {
            anyhow!("Codex provider returned 401 and no ChatGPT refresh token was found")
        })?;

    let response = reqwest::Client::new()
        .post(CODEX_REFRESH_TOKEN_URL)
        .header("Content-Type", "application/json")
        .json(&CodexRefreshRequest {
            client_id: CODEX_CLIENT_ID,
            grant_type: "refresh_token",
            refresh_token,
        })
        .send()
        .await?;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(anyhow!(
            "Codex token refresh failed with HTTP {}: {}",
            status.as_u16(),
            refresh_error_message(&body)
        ));
    }

    let refreshed = response.json::<CodexRefreshResponse>().await?;
    let mut refreshed_snapshot = snapshot;
    if let Some(path) = refreshed_snapshot.path.clone() {
        refreshed_snapshot = persist_codex_refresh(&path, &refreshed)?;
    } else {
        refreshed_snapshot.access_token = refreshed.access_token;
        refreshed_snapshot.refresh_token = refreshed.refresh_token;
    }

    let mut credentials = Credentials::load();
    credentials.codex_access_token = refreshed_snapshot.access_token;
    credentials.codex_account_id = refreshed_snapshot.account_id;
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

fn refresh_error_message(body: &str) -> String {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        if let Some(message) = value
            .pointer("/error/message")
            .or_else(|| value.get("message"))
            .and_then(Value::as_str)
        {
            return message.to_string();
        }
        if let Some(code) = value
            .pointer("/error/code")
            .or_else(|| value.get("code"))
            .and_then(Value::as_str)
        {
            return code.to_string();
        }
    }
    let trimmed = body.trim();
    if trimmed.is_empty() {
        "empty response body".to_string()
    } else {
        trimmed.chars().take(240).collect()
    }
}
