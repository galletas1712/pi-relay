#![forbid(unsafe_code)]

mod catalog;
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

pub use catalog::{
    canonical_json, fingerprint_json, McpManifestTool, McpSessionManifest, McpSessionSnapshot,
};
pub use config::{
    McpConfig, McpHttpAuthConfig, McpServerConfig, McpStdioTransportConfig,
    McpStreamableHttpTransportConfig, McpTransportConfig,
};
pub use manager::{
    McpAuthFailure, McpAuthKind, McpAuthServerStatus, McpAuthStatus, McpCallError, McpCallOutput,
    McpHealth, McpInventory, McpInventoryServer, McpInventoryTool, McpLogoutResult, McpManager,
    McpManagerError, McpServerSelection, McpSessionSelection, McpToolView,
};
pub use oauth_credentials::OAuthCredentialStoreError;
pub use oauth_login::{McpOAuthLoginError, McpOAuthLoginStart};
