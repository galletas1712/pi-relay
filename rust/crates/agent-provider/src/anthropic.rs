//! Minimal Anthropic auth type.
//!
//! The full `AnthropicProvider` implementation has been replaced by the
//! pi-ai sidecar (`pi_ai::PiAiSidecarProvider`).  This module retains only
//! the `AnthropicAuth` enum used by `agent-daemon/src/auth.rs` for
//! credential loading.

/// Authentication method for the Anthropic API.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnthropicAuth {
    ApiKey(String),
    Bearer(String),
}

impl AnthropicAuth {
    pub fn apply(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Self::ApiKey(key) => request.header("x-api-key", key),
            Self::Bearer(token) => request.header("Authorization", format!("Bearer {token}")),
        }
    }
}
