use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpHealth {
    Healthy,
    Unavailable,
    Revoked,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthStatus {
    NonOauth,
    Unsupported,
    Unknown,
    Bearer,
    LoginRequired,
    ReauthenticationRequired,
    OauthReady,
    AuthorizationPending,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthKind {
    None,
    Bearer,
    Oauth,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum McpAuthFailure {
    CredentialStoreUnavailable,
    DiscoveryFailed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpAuthServerStatus {
    pub server: String,
    pub auth_kind: McpAuthKind,
    pub auth_state: McpAuthStatus,
    pub can_login: bool,
    pub can_logout: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<McpAuthFailure>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum McpLogoutResult {
    Removed,
    NotFound,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpInventoryTool {
    pub raw_name: String,
    pub description: String,
    pub context_token_estimate: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpInventoryServer {
    pub server: String,
    pub revision: String,
    pub health: McpHealth,
    pub tools: Vec<McpInventoryTool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpInventory {
    pub revision: String,
    pub servers: Vec<McpInventoryServer>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpSessionSelection {
    pub inventory_revision: String,
    pub servers: Vec<McpServerSelection>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct McpServerSelection {
    pub server: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct McpToolView {
    pub server: String,
    pub raw_name: String,
    pub exposed_name: String,
    pub contract_fingerprint: String,
    pub health: McpHealth,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpCallOutput {
    pub output: String,
    pub is_error: bool,
}

#[derive(Debug, Error)]
pub enum McpManagerError {
    #[error("mcp_inventory_changed: MCP inventory changed")]
    InventoryChanged { current_revision: String },
    #[error("mcp_selection_invalid: {message}")]
    SelectionInvalid { message: String },
    #[error("mcp_unavailable: selected MCP server {server} is unavailable")]
    Unavailable { server: String },
    #[error("mcp_oauth_credential_store_failed")]
    CredentialStore(#[from] crate::OAuthCredentialStoreError),
    #[error("invalid MCP catalog: {0}")]
    Catalog(#[from] anyhow::Error),
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum McpCallError {
    #[error("mcp_tool_revoked: MCP tool {tool} is no longer allowed")]
    Revoked { tool: String },
    #[error("mcp_server_unavailable: MCP server {server} is unavailable")]
    ServerUnavailable { server: String },
    #[error("mcp_tool_contract_changed: MCP tool {tool} no longer has the advertised contract")]
    ContractChanged { tool: String },
    #[error("mcp_call_timeout: MCP tool {tool} exceeded its total call deadline")]
    Timeout { tool: String },
    #[error("mcp_protocol_error: {message}")]
    Protocol { message: String },
}
