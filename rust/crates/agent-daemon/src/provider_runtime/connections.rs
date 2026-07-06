use std::{collections::HashMap, sync::Arc};

use agent_provider::{
    anthropic::{AnthropicModelCache, AnthropicProvider},
    openai::{
        OpenAiCodexHttpClient, OpenAiCodexSessionState, OpenAiModelCatalogCache, OpenAiProvider,
    },
};
use agent_vocab::ProviderKind;
use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use crate::auth::Credentials;

use super::provider::ProviderHandle;

#[derive(Clone)]
pub(crate) struct ProviderConnectionRegistry {
    codex_client: OpenAiCodexHttpClient,
    anthropic_client: reqwest::Client,
    anthropic_model_cache: AnthropicModelCache,
    openai_model_catalog_cache: OpenAiModelCatalogCache,
    connections: Arc<Mutex<HashMap<ProviderConnectionKey, Arc<ProviderConnection>>>>,
    #[cfg(test)]
    test_openai_base_url: Option<String>,
    #[cfg(test)]
    test_credentials: Option<Credentials>,
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
    client: OpenAiCodexHttpClient,
    model_catalog_cache: OpenAiModelCatalogCache,
}

struct AnthropicConnection {
    client: reqwest::Client,
    model_cache: AnthropicModelCache,
}

impl ProviderConnectionRegistry {
    pub(crate) fn new() -> Self {
        Self {
            codex_client: OpenAiCodexHttpClient::new(),
            anthropic_client: reqwest::Client::new(),
            anthropic_model_cache: AnthropicModelCache::default(),
            openai_model_catalog_cache: OpenAiModelCatalogCache::default(),
            connections: Arc::new(Mutex::new(HashMap::new())),
            #[cfg(test)]
            test_openai_base_url: None,
            #[cfg(test)]
            test_credentials: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_openai(base_url: String) -> Self {
        Self {
            test_openai_base_url: Some(base_url),
            test_credentials: Some(Credentials {
                codex_access_token: Some("test-access-token".to_string()),
                codex_account_id: Some("test-account".to_string()),
                codex_installation_id: Some("test-installation".to_string()),
                anthropic_api_key: None,
            }),
            ..Self::new()
        }
    }

    pub(super) async fn provider_for_config(
        &self,
        provider: ProviderKind,
        credentials: &Credentials,
        session_id: &str,
    ) -> Result<ProviderHandle> {
        let connection = self.get_or_create(session_id, provider).await;
        #[cfg(test)]
        let credentials = self.test_credentials.as_ref().unwrap_or(credentials);
        connection.provider_handle(
            credentials,
            #[cfg(test)]
            self.test_openai_base_url.as_deref(),
        )
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
                        client: self.codex_client.clone(),
                        model_catalog_cache: self.openai_model_catalog_cache.clone(),
                    }),
                    ProviderKind::Claude => ProviderConnection::Anthropic(AnthropicConnection {
                        client: self.anthropic_client.clone(),
                        model_cache: self.anthropic_model_cache.clone(),
                    }),
                })
            })
            .clone()
    }
}

impl ProviderConnection {
    fn provider_handle(
        &self,
        credentials: &Credentials,
        #[cfg(test)] openai_base_url: Option<&str>,
    ) -> Result<ProviderHandle> {
        match self {
            ProviderConnection::OpenAi(connection) => connection.provider_handle(
                credentials,
                #[cfg(test)]
                openai_base_url,
            ),
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
    fn provider_handle(
        &self,
        credentials: &Credentials,
        #[cfg(test)] base_url: Option<&str>,
    ) -> Result<ProviderHandle> {
        let provider = OpenAiProvider::codex_with_client_session_and_cache(
            self.client.clone(),
            self.state.clone(),
            credentials.codex_access_token.clone().ok_or_else(|| {
                anyhow!("~/.codex ChatGPT token not found for OpenAI subscription transport")
            })?,
            credentials.codex_account_id.clone(),
            credentials.codex_installation_id.clone(),
            self.model_catalog_cache.clone(),
        );
        #[cfg(test)]
        let mut provider = provider;
        #[cfg(test)]
        if let Some(base_url) = base_url {
            provider.set_base_url_for_test(base_url.to_string());
        }
        Ok(ProviderHandle {
            provider: Box::new(provider),
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
