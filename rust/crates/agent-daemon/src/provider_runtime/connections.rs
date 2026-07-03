use std::{collections::HashMap, sync::Arc};

use agent_provider::{
    anthropic::{AnthropicModelCache, AnthropicProvider},
    openai::{OpenAiCodexSessionState, OpenAiProvider},
};
use agent_vocab::ProviderKind;
use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use crate::auth::Credentials;

use super::provider::ProviderHandle;

#[derive(Clone)]
pub(crate) struct ProviderConnectionRegistry {
    client: reqwest::Client,
    anthropic_model_cache: AnthropicModelCache,
    connections: Arc<Mutex<HashMap<ProviderConnectionKey, Arc<ProviderConnection>>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderConnectionKey {
    session_id: String,
    provider: ProviderKind,
}

enum ProviderConnection {
    OpenAi(OpenAiCodexConnection),
    Anthropic(AnthropicConnection),
}

struct OpenAiCodexConnection {
    state: Arc<OpenAiCodexSessionState>,
    client: reqwest::Client,
}

struct AnthropicConnection {
    client: reqwest::Client,
    model_cache: AnthropicModelCache,
}

impl ProviderConnectionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            anthropic_model_cache: AnthropicModelCache::default(),
            connections: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) async fn provider_for_config(
        &self,
        provider: ProviderKind,
        credentials: &Credentials,
        session_id: &str,
    ) -> Result<ProviderHandle> {
        let connection = self.get_or_create(session_id, provider).await;
        connection.provider_handle(credentials)
    }

    pub(crate) async fn mark_compacted(
        &self,
        session_id: &str,
        provider: ProviderKind,
        generation: u64,
    ) {
        let key = ProviderConnectionKey {
            session_id: session_id.to_string(),
            provider,
        };
        let connection = {
            let guard = self.connections.lock().await;
            guard.get(&key).cloned()
        };
        if let Some(connection) = connection {
            connection.mark_compacted(generation);
        }
    }

    pub(crate) async fn remove_session(&self, session_id: &str) {
        let mut guard = self.connections.lock().await;
        guard.retain(|key, _| key.session_id != session_id);
    }

    async fn get_or_create(
        &self,
        session_id: &str,
        provider: ProviderKind,
    ) -> Arc<ProviderConnection> {
        let key = ProviderConnectionKey {
            session_id: session_id.to_string(),
            provider,
        };
        let mut guard = self.connections.lock().await;
        guard
            .entry(key)
            .or_insert_with(|| {
                Arc::new(match provider {
                    ProviderKind::OpenAi => ProviderConnection::OpenAi(OpenAiCodexConnection {
                        state: Arc::new(OpenAiCodexSessionState::new(session_id)),
                        client: self.client.clone(),
                    }),
                    ProviderKind::Claude => ProviderConnection::Anthropic(AnthropicConnection {
                        client: self.client.clone(),
                        model_cache: self.anthropic_model_cache.clone(),
                    }),
                })
            })
            .clone()
    }
}

impl ProviderConnection {
    fn provider_handle(&self, credentials: &Credentials) -> Result<ProviderHandle> {
        match self {
            ProviderConnection::OpenAi(connection) => connection.provider_handle(credentials),
            ProviderConnection::Anthropic(connection) => connection.provider_handle(credentials),
        }
    }

    fn mark_compacted(&self, generation: u64) {
        match self {
            ProviderConnection::OpenAi(connection) => {
                connection.state.set_window_generation(generation);
            }
            ProviderConnection::Anthropic(_) => {}
        }
    }
}

impl OpenAiCodexConnection {
    fn provider_handle(&self, credentials: &Credentials) -> Result<ProviderHandle> {
        Ok(ProviderHandle {
            provider: Box::new(OpenAiProvider::codex_with_client_and_session(
                self.client.clone(),
                self.state.clone(),
                credentials.codex_access_token.clone().ok_or_else(|| {
                    anyhow!("~/.codex ChatGPT token not found for OpenAI subscription transport")
                })?,
                credentials.codex_account_id.clone(),
                credentials.codex_installation_id.clone(),
            )),
            uses_codex_auth: true,
        })
    }
}

impl AnthropicConnection {
    fn provider_handle(&self, credentials: &Credentials) -> Result<ProviderHandle> {
        Ok(ProviderHandle {
            provider: Box::new(AnthropicProvider::new_with_client_and_cache(
                self.client.clone(),
                credentials.anthropic_api_key.clone().ok_or_else(|| {
                    anyhow!("ANTHROPIC_API_KEY not found in env or Claude Code config")
                })?,
                self.model_cache.clone(),
            )),
            uses_codex_auth: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn openai_compaction_updates_session_generation() {
        let registry = ProviderConnectionRegistry::new();
        let connection = registry
            .get_or_create("session-1", ProviderKind::OpenAi)
            .await;
        let ProviderConnection::OpenAi(openai) = connection.as_ref() else {
            panic!("expected OpenAI connection");
        };
        assert_eq!(openai.state.window_generation(), 0);

        registry
            .mark_compacted("session-1", ProviderKind::OpenAi, 42)
            .await;

        assert_eq!(openai.state.window_generation(), 42);
    }

    #[tokio::test]
    async fn remove_session_drops_all_provider_connections_for_session() {
        let registry = ProviderConnectionRegistry::new();
        registry
            .get_or_create("session-1", ProviderKind::OpenAi)
            .await;
        registry
            .get_or_create("session-1", ProviderKind::Claude)
            .await;
        registry
            .get_or_create("session-2", ProviderKind::OpenAi)
            .await;

        registry.remove_session("session-1").await;

        let guard = registry.connections.lock().await;
        assert_eq!(guard.len(), 1);
        assert!(guard.keys().all(|key| key.session_id == "session-2"));
    }
}
