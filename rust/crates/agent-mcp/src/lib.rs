#![forbid(unsafe_code)]

mod client;
mod config;
mod http_transport;
mod manager;
mod oauth_callback;
mod oauth_credentials;
mod oauth_discovery;
mod oauth_http;
mod oauth_login;
mod oauth_runtime;
mod result;

pub use agent_mcp_types::{
    canonical_json, fingerprint_json, McpAuthFailure, McpAuthKind, McpAuthServerStatus,
    McpAuthStatus, McpCallError, McpCallOutput, McpHealth, McpInventory, McpInventoryServer,
    McpInventoryTool, McpLogoutResult, McpManagerError, McpManifestTool, McpOAuthLoginError,
    McpOAuthLoginStart, McpServerSelection, McpSessionManifest, McpSessionSelection,
    McpSessionSnapshot, McpToolView, OAuthCredentialStoreError,
};
pub use config::{
    McpConfig, McpHttpAuthConfig, McpServerConfig, McpStdioTransportConfig,
    McpStreamableHttpTransportConfig, McpTransportConfig,
};
pub use manager::McpManager;
