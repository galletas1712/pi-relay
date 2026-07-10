use agent_mcp::{
    McpAuthFailure, McpAuthStatus, McpLogoutResult, McpOAuthLoginError, OAuthCredentialStoreError,
};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::state::AppState;
use crate::types::RpcError;

const MAX_SERVER_ID_BYTES: usize = 128;
const LOGIN_ID_BYTES: usize = 16;
const MAX_CALLBACK_URL_BYTES: usize = 16 * 1024;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyParams {}

fn decode_params<T: for<'de> Deserialize<'de>>(
    params: Value,
    message: &'static str,
) -> std::result::Result<T, RpcError> {
    serde_json::from_value(params).map_err(|_| RpcError::new("invalid_params", message))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServerParams {
    server: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginParams {
    server: String,
    login_id: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CompleteParams {
    server: String,
    login_id: String,
    callback_url: String,
}

pub(crate) async fn status(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let EmptyParams {} = decode_params(params, "Invalid parameters for mcp.status")?;
    let servers = state
        .mcp
        .auth_statuses()
        .await
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
    let ServerParams { server } = decode_params(params, "Invalid parameters for mcp.login")?;
    validate_server_id(&server)?;
    let start = state
        .mcp
        .begin_oauth_login(&server)
        .await
        .map_err(map_login_error)?;
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
        .mcp
        .complete_oauth_login(&server, &login_id, &callback_url)
        .await
        .map_err(map_login_error)?;
    Ok(json!({ "completed": true }))
}

pub(crate) async fn cancel(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let LoginParams { server, login_id } =
        decode_params(params, "Invalid parameters for mcp.cancel")?;
    validate_server_id(&server)?;
    validate_login_id(&login_id)?;
    state
        .mcp
        .cancel_oauth_login(&server, &login_id)
        .await
        .map_err(map_login_error)?;
    Ok(json!({ "cancelled": true }))
}

pub(crate) async fn logout(
    state: &AppState,
    params: Value,
) -> std::result::Result<Value, RpcError> {
    let ServerParams { server } = decode_params(params, "Invalid parameters for mcp.logout")?;
    validate_server_id(&server)?;
    let result = state
        .mcp
        .logout_oauth(&server)
        .await
        .map_err(map_store_error)?;
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

fn map_store_error(_error: OAuthCredentialStoreError) -> RpcError {
    RpcError::new(
        "mcp_oauth_credential_store_failed",
        "MCP OAuth credential storage is unavailable",
    )
}

fn map_login_error(error: McpOAuthLoginError) -> RpcError {
    match error {
        McpOAuthLoginError::NotConfigured => RpcError::new(
            "mcp_oauth_not_configured",
            "OAuth login is not configured for this MCP server",
        ),
        McpOAuthLoginError::AlreadyPending => RpcError::new(
            "mcp_oauth_login_already_pending",
            "An OAuth login is already pending for this MCP server",
        ),
        McpOAuthLoginError::NotFound => RpcError::new(
            "mcp_oauth_login_not_found",
            "The MCP OAuth login was not found",
        ),
        McpOAuthLoginError::AlreadyCompleted => RpcError::new(
            "mcp_oauth_login_finished",
            "The MCP OAuth login is no longer pending",
        ),
        McpOAuthLoginError::Cancelled => RpcError::new(
            "mcp_oauth_login_cancelled",
            "The MCP OAuth login was cancelled",
        ),
        McpOAuthLoginError::Expired => {
            RpcError::new("mcp_oauth_login_expired", "The MCP OAuth login expired")
        }
        McpOAuthLoginError::CallbackBind => RpcError::new(
            "mcp_oauth_callback_unavailable",
            "The daemon could not start the loopback OAuth callback listener",
        ),
        McpOAuthLoginError::InvalidCallback => RpcError::new(
            "mcp_oauth_callback_invalid",
            "The OAuth callback URL is invalid for this login",
        ),
        McpOAuthLoginError::Provider => RpcError::new(
            "mcp_oauth_provider_error",
            "The authorization server rejected the OAuth login",
        ),
        McpOAuthLoginError::Persistence => map_store_error(OAuthCredentialStoreError::Io),
        McpOAuthLoginError::Discovery
        | McpOAuthLoginError::Registration
        | McpOAuthLoginError::TokenEndpoint
        | McpOAuthLoginError::Network
        | McpOAuthLoginError::Unavailable
        | McpOAuthLoginError::AuthorizationUrlTooLong => RpcError::new(
            "mcp_oauth_login_failed",
            "The MCP OAuth login could not be completed",
        ),
    }
}

#[cfg(test)]
#[path = "mcp_auth_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "mcp_auth_integration_tests.rs"]
mod integration_tests;
