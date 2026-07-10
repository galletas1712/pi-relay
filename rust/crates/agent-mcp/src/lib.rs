#![forbid(unsafe_code)]

mod catalog;
mod client;
mod config;
mod http_transport;
mod manager;
mod oauth_callback;
mod oauth_discovery;
mod oauth_http;
mod oauth_login;
mod result;

pub use catalog::{
    canonical_json, fingerprint_json, McpManifestTool, McpSessionManifest, McpSessionSnapshot,
};
pub use config::{
    McpConfig, McpHttpAuthConfig, McpServerConfig, McpStdioTransportConfig,
    McpStreamableHttpTransportConfig, McpTransportConfig,
};
pub use manager::{
    McpCallError, McpCallOutput, McpHealth, McpInventory, McpInventoryServer, McpInventoryTool,
    McpManager, McpManagerError, McpServerSelection, McpSessionSelection, McpToolView,
};
pub use oauth_login::{McpOAuthLoginError, McpOAuthLoginStart};
