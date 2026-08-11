use std::sync::Arc;

use agent_provider::pi_ai::PiAiSidecarProvider;
use anyhow::Result;
use tokio::sync::Mutex;

use crate::auth::Credentials;

use super::provider::ProviderHandle;

/// Registry that produces a `PiAiSidecarProvider` for every session.
///
/// All provider-specific logic lives in the pi-ai sidecar; the daemon only
/// needs a shared HTTP client and the sidecar's base URL.
#[derive(Clone)]
pub(crate) struct ProviderConnectionRegistry {
    sidecar_url: String,
    http_client: reqwest::Client,
    connections: Arc<Mutex<Vec<String>>>,
}

impl ProviderConnectionRegistry {
    pub(crate) fn new() -> Self {
        let sidecar_url = std::env::var("SIDECAR_URL")
            .unwrap_or_else(|_| agent_provider::pi_ai::SIDECAR_DEFAULT_URL.to_string());
        Self {
            sidecar_url,
            http_client: reqwest::Client::new(),
            connections: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(super) async fn provider_for_config(
        &self,
        provider: String,
        _credentials: &Credentials,
        _session_id: &str,
    ) -> Result<ProviderHandle> {
        let pi_ai = PiAiSidecarProvider::with_client(
            &self.sidecar_url,
            self.http_client.clone(),
            provider.as_str(),
        );
        Ok(ProviderHandle {
            provider: Box::new(pi_ai),
            uses_codex_auth: false,
        })
    }

    pub(crate) async fn mark_compacted(
        &self,
        session_id: &str,
        _provider: String,
        _generation: u64,
    ) {
        // The pi-ai sidecar manages cache state internally; no-op.
        let _ = session_id;
    }

    pub(crate) async fn remove_session(&self, session_id: &str) {
        let mut guard = self.connections.lock().await;
        guard.retain(|s| s != session_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remove_session_drops_connection() {
        let registry = ProviderConnectionRegistry::new();
        registry.connections.lock().await.push("session-1".to_string());
        registry.connections.lock().await.push("session-2".to_string());

        registry.remove_session("session-1").await;

        let guard = registry.connections.lock().await;
        assert_eq!(guard.len(), 1);
        assert_eq!(guard[0], "session-2");
    }

    #[tokio::test]
    async fn provider_for_config_returns_pi_ai_provider() {
        let registry = ProviderConnectionRegistry::new();
        let credentials = Credentials::default();
        let handle = registry
            .provider_for_config("openai".to_string(), &credentials, "session-1")
            .await
            .expect("provider handle");
        assert!(!handle.uses_codex_auth);
    }
}
