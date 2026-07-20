use agent_mcp_types::{McpAuthFailure, McpAuthStatus, McpLogoutResult};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;
use crate::types::RpcError;

const MAX_SERVER_ID_BYTES: usize = 128;
const LOGIN_ID_BYTES: usize = 16;
const MAX_CALLBACK_URL_BYTES: usize = 16 * 1024;

fn decode_params<T: for<'de> Deserialize<'de>>(
    params: Value,
    message: &'static str,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(params).map_err(|_| RpcError::new("invalid_params", message))
}

// MCP servers live on a runtime host, so every MCP RPC is scoped to a runtime.
// The new-session panel already resolves the runtime it will start the session
// on and threads its id in.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeParams {
    runtime_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerParams {
    runtime_id: String,
    server: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginParams {
    runtime_id: String,
    server: String,
    login_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteParams {
    runtime_id: String,
    server: String,
    login_id: String,
    callback_url: String,
}

pub(crate) async fn status(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let RuntimeParams { runtime_id } = decode_params(params, "Invalid parameters for mcp.status")?;
    let servers = state
        .runtime_hosts
        .mcp_auth_statuses(&runtime_id)
        .await
        .map_err(map_runtime_mcp_error)?
        .into_iter()
        .map(|status| {
            let auth_state = match status.auth_state {
                McpAuthStatus::NonOauth | McpAuthStatus::Bearer => "not_applicable",
                McpAuthStatus::Unsupported => "unsupported",
                McpAuthStatus::Unknown => "unknown",
                McpAuthStatus::LoginRequired => "login_required",
                McpAuthStatus::ReauthenticationRequired => "reauthentication_required",
                McpAuthStatus::OauthReady => "ready",
                McpAuthStatus::AuthorizationPending => "authorization_pending",
            };
            let failure = status.failure.map(|failure| match failure {
                McpAuthFailure::CredentialStoreUnavailable => "credential_store_unavailable",
                McpAuthFailure::DiscoveryFailed => "discovery_failed",
            });
            let mut value = json!({
                "server": status.server,
                "auth_kind": status.auth_kind,
                "auth_state": auth_state,
                "can_login": status.can_login,
                "can_logout": status.can_logout,
            });
            if let Some(failure) = failure {
                value["failure"] = Value::String(failure.to_string());
            }
            value
        })
        .collect::<Vec<_>>();
    Ok(json!({ "servers": servers }))
}

pub(crate) async fn login(state: &AppState, params: Value) -> std::result::Result<Value, RpcError> {
    let ServerParams { runtime_id, server } =
        decode_params(params, "Invalid parameters for mcp.login")?;
    validate_server_id(&server)?;
    let start = state
        .runtime_hosts
        .mcp_begin_login(&runtime_id, server)
        .await
        .map_err(map_runtime_mcp_error)?;
    Ok(json!({
        "login_id": start.login_id,
        "authorization_url": start.authorization_url,
        "expires_at_unix_seconds": start.expires_at_unix_seconds,
    }))
}

pub(crate) async fn complete(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let CompleteParams {
        runtime_id,
        server,
        login_id,
        callback_url,
    } = decode_params(params, "Invalid parameters for mcp.complete")?;
    validate_server_id(&server)?;
    validate_login_id(&login_id)?;
    if callback_url.is_empty() || callback_url.len() > MAX_CALLBACK_URL_BYTES {
        return Err(RpcError::new(
            "invalid_params",
            "callback_url must contain the entire bounded callback URL",
        ));
    }
    state
        .runtime_hosts
        .mcp_complete_login(&runtime_id, server, login_id, callback_url)
        .await
        .map_err(map_runtime_mcp_error)?;
    Ok(json!({ "completed": true }))
}

pub(crate) async fn cancel(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let LoginParams {
        runtime_id,
        server,
        login_id,
    } = decode_params(params, "Invalid parameters for mcp.cancel")?;
    validate_server_id(&server)?;
    validate_login_id(&login_id)?;
    state
        .runtime_hosts
        .mcp_cancel_login(&runtime_id, server, login_id)
        .await
        .map_err(map_runtime_mcp_error)?;
    Ok(json!({ "cancelled": true }))
}

pub(crate) async fn logout(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let ServerParams { runtime_id, server } =
        decode_params(params, "Invalid parameters for mcp.logout")?;
    validate_server_id(&server)?;
    let result = state
        .runtime_hosts
        .mcp_logout(&runtime_id, server)
        .await
        .map_err(map_runtime_mcp_error)?;
    Ok(json!({
        "result": match result {
            McpLogoutResult::Removed => "removed",
            McpLogoutResult::NotFound => "not_found",
        }
    }))
}

fn validate_server_id(server: &str) -> std::result::Result<(), RpcError> {
    if server.is_empty()
        || server.len() > MAX_SERVER_ID_BYTES
        || server.chars().any(char::is_control)
    {
        return Err(RpcError::new(
            "invalid_params",
            "server must be a configured MCP server ID of at most 128 bytes",
        ));
    }
    Ok(())
}

fn validate_login_id(login_id: &str) -> std::result::Result<(), RpcError> {
    if login_id.len() != LOGIN_ID_BYTES
        || !login_id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(RpcError::new(
            "invalid_params",
            "login_id must be a valid MCP OAuth login ID",
        ));
    }
    Ok(())
}

/// Map a runtime MCP error back to the frontend's stable RpcError codes. The
/// runtime puts those codes on RuntimeCommandError.code (preserved as
/// RuntimeHostError through the conduit); messages and data are already
/// secret-free.
pub(crate) fn map_runtime_mcp_error(error: anyhow::Error) -> RpcError {
    if let Some(host) = error.downcast_ref::<crate::runtime_hosts::RuntimeHostError>() {
        return match host.code.as_str() {
            "mcp_inventory_changed" => RpcError {
                code: "mcp_inventory_changed".to_string(),
                message: host.message.clone(),
                data: host.data.clone(),
            },
            "mcp_selection_invalid"
            | "mcp_unavailable"
            | "mcp_oauth_credential_store_failed"
            | "mcp_oauth_not_configured"
            | "mcp_oauth_login_already_pending"
            | "mcp_oauth_login_not_found"
            | "mcp_oauth_login_finished"
            | "mcp_oauth_login_cancelled"
            | "mcp_oauth_login_expired"
            | "mcp_oauth_callback_unavailable"
            | "mcp_oauth_callback_invalid"
            | "mcp_oauth_provider_error"
            | "mcp_oauth_login_failed" => RpcError::new(host.code.clone(), host.message.clone()),
            _ => RpcError::new("mcp_error", format!("{}: {}", host.code, host.message)),
        };
    }
    RpcError::new("mcp_error", format!("{error:#}"))
}

#[cfg(test)]
#[path = "mcp_auth_tests.rs"]
mod tests;
