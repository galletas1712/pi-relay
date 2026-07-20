use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OAuthCredentialStoreError {
    #[error("oauth_credential_store_empty")]
    Empty,
    #[error("oauth_credential_store_oversized")]
    Oversized,
    #[error("oauth_credential_store_corrupt")]
    Corrupt,
    #[error("oauth_credential_store_version_unsupported")]
    UnsupportedVersion,
    #[error("oauth_credential_store_bounds_exceeded")]
    Bounds,
    #[error("oauth_credential_store_io_failed")]
    Io,
}

#[derive(PartialEq, Eq)]
pub struct McpOAuthLoginStart {
    pub login_id: String,
    pub authorization_url: String,
    pub expires_at_unix_seconds: u64,
}

impl fmt::Debug for McpOAuthLoginStart {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("McpOAuthLoginStart")
            .field("login_id", &self.login_id)
            .field("authorization_url", &"<redacted>")
            .field("expires_at_unix_seconds", &self.expires_at_unix_seconds)
            .finish()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum McpOAuthLoginError {
    #[error("oauth_login_not_configured")]
    NotConfigured,
    #[error("oauth_login_already_pending")]
    AlreadyPending,
    #[error("oauth_login_not_found")]
    NotFound,
    #[error("oauth_login_already_completed")]
    AlreadyCompleted,
    #[error("oauth_login_cancelled")]
    Cancelled,
    #[error("oauth_login_expired")]
    Expired,
    #[error("oauth_discovery_failed")]
    Discovery,
    #[error("oauth_registration_failed")]
    Registration,
    #[error("oauth_callback_bind_failed")]
    CallbackBind,
    #[error("oauth_callback_invalid")]
    InvalidCallback,
    #[error("oauth_provider_error")]
    Provider,
    #[error("oauth_token_endpoint_error")]
    TokenEndpoint,
    #[error("oauth_credential_store_failed")]
    Persistence,
    #[error("oauth_network_failed")]
    Network,
    #[error("oauth_login_unavailable")]
    Unavailable,
    #[error("oauth_authorization_url_too_long")]
    AuthorizationUrlTooLong,
}
