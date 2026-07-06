use std::{collections::HashMap, sync::Arc};

#[cfg(test)]
use std::collections::VecDeque;
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

use agent_provider::{
    anthropic::{AnthropicModelCache, AnthropicProvider},
    openai::{
        OpenAiCodexHttpClient, OpenAiCodexSessionState, OpenAiModelCatalogCache, OpenAiProvider,
    },
};
use agent_vocab::ProviderKind;
use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use crate::auth::CredentialSnapshot;

use super::provider::ProviderHandle;

#[derive(Clone)]
pub(crate) struct ProviderConnectionRegistry {
    codex_client: OpenAiCodexHttpClient,
    anthropic_client: reqwest::Client,
    anthropic_model_cache: AnthropicModelCache,
    openai_model_catalog_cache: OpenAiModelCatalogCache,
    connections: Arc<Mutex<HashMap<ProviderConnectionKey, Arc<ProviderConnection>>>>,
    #[cfg(test)]
    injected_providers: Arc<Mutex<VecDeque<ProviderHandle>>>,
    #[cfg(test)]
    provider_constructions: Arc<AtomicUsize>,
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
    session_id: String,
    state: Mutex<Option<OpenAiCodexConnectionState>>,
    client: OpenAiCodexHttpClient,
    model_catalog_cache: OpenAiModelCatalogCache,
}

struct OpenAiCodexConnectionState {
    identity: OpenAiCodexIdentity,
    credential_generation: u64,
    state: Arc<OpenAiCodexSessionState>,
}

#[derive(PartialEq, Eq)]
struct OpenAiCodexIdentity {
    account_id: Option<String>,
    installation_id: Option<String>,
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
            injected_providers: Arc::default(),
            #[cfg(test)]
            provider_constructions: Arc::default(),
        }
    }

    pub(super) async fn provider_for_config(
        &self,
        provider: ProviderKind,
        credentials: &CredentialSnapshot,
        session_id: &str,
    ) -> Result<ProviderHandle> {
        #[cfg(test)]
        {
            self.provider_constructions.fetch_add(1, Ordering::Relaxed);
            if let Some(mut provider) = self.injected_providers.lock().await.pop_front() {
                provider.credentials = credentials.clone();
                return Ok(provider);
            }
        }
        let connection = self.get_or_create(session_id, provider).await;
        connection.provider_handle(credentials).await
    }

    #[cfg(test)]
    pub(crate) async fn install_test_provider(
        &self,
        provider: Box<dyn agent_provider::ModelProvider>,
        uses_codex_auth: bool,
        codex_account_id: Option<String>,
    ) {
        self.injected_providers
            .lock()
            .await
            .push_back(ProviderHandle {
                provider,
                uses_codex_auth,
                codex_account_id,
                credentials: CredentialSnapshot::for_tests(Default::default()),
            });
    }

    #[cfg(test)]
    pub(crate) fn provider_construction_count(&self) -> usize {
        self.provider_constructions.load(Ordering::Relaxed)
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
            connection.mark_compacted(generation).await;
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
                        session_id: session_id.to_string(),
                        state: Mutex::new(None),
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
    async fn provider_handle(&self, credentials: &CredentialSnapshot) -> Result<ProviderHandle> {
        match self {
            ProviderConnection::OpenAi(connection) => connection.provider_handle(credentials).await,
            ProviderConnection::Anthropic(connection) => connection.provider_handle(credentials),
        }
    }

    async fn mark_compacted(&self, generation: u64) {
        match self {
            ProviderConnection::OpenAi(connection) => {
                connection.mark_compacted(generation).await;
            }
            ProviderConnection::Anthropic(_) => {}
        }
    }
}

impl OpenAiCodexConnection {
    async fn provider_handle(&self, snapshot: &CredentialSnapshot) -> Result<ProviderHandle> {
        let credentials = snapshot.credentials();
        let access_token = credentials.codex_access_token.clone().ok_or_else(|| {
            anyhow!("~/.codex ChatGPT token not found for OpenAI subscription transport")
        })?;
        let session_state = self
            .session_state(
                credentials.codex_account_id.as_deref(),
                credentials.codex_installation_id.as_deref(),
                snapshot.codex_generation(),
            )
            .await;
        let mut provider = OpenAiProvider::codex_with_client_session_and_cache(
            self.client.clone(),
            session_state,
            access_token,
            credentials.codex_account_id.clone(),
            credentials.codex_installation_id.clone(),
            self.model_catalog_cache.clone(),
        );
        provider.set_credential_generation(snapshot.codex_generation());
        Ok(ProviderHandle {
            provider: Box::new(provider),
            uses_codex_auth: true,
            codex_account_id: credentials.codex_account_id.clone(),
            credentials: snapshot.clone(),
        })
    }

    async fn session_state(
        &self,
        account_id: Option<&str>,
        installation_id: Option<&str>,
        credential_generation: u64,
    ) -> Arc<OpenAiCodexSessionState> {
        let identity = OpenAiCodexIdentity {
            account_id: account_id.map(str::to_string),
            installation_id: installation_id.map(str::to_string),
        };
        let identity_is_complete =
            identity.account_id.is_some() && identity.installation_id.is_some();
        let mut current = self.state.lock().await;
        if let Some(current) = current.as_mut() {
            if credential_generation < current.credential_generation {
                return Arc::new(OpenAiCodexSessionState::new(&self.session_id));
            }
            if current.identity == identity
                && (identity_is_complete || credential_generation == current.credential_generation)
            {
                current.credential_generation = credential_generation;
                return current.state.clone();
            }
        }

        let state = Arc::new(OpenAiCodexSessionState::new(&self.session_id));
        *current = Some(OpenAiCodexConnectionState {
            identity,
            credential_generation,
            state: state.clone(),
        });
        state
    }

    async fn mark_compacted(&self, generation: u64) {
        let current = self.state.lock().await;
        if let Some(current) = current.as_ref() {
            current.state.set_window_generation(generation);
        }
    }
}

impl AnthropicConnection {
    fn provider_handle(&self, snapshot: &CredentialSnapshot) -> Result<ProviderHandle> {
        let credentials = snapshot.credentials();
        let mut provider = AnthropicProvider::new_with_client_and_cache(
            self.client.clone(),
            credentials.anthropic_api_key.clone().ok_or_else(|| {
                anyhow!("ANTHROPIC_API_KEY not found in env or Claude Code config")
            })?,
            self.model_cache.clone(),
        );
        provider.set_credential_generation(snapshot.anthropic_generation());
        Ok(ProviderHandle {
            provider: Box::new(provider),
            uses_codex_auth: false,
            codex_account_id: None,
            credentials: snapshot.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{CodexCredentialRefresher, CredentialManager, Credentials};
    use anyhow::Result;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct UnusedRefresher;

    #[async_trait]
    impl CodexCredentialRefresher for UnusedRefresher {
        async fn refresh(&self, _prior: &Credentials) -> Result<Credentials> {
            unreachable!("routine provider resolution must not refresh credentials")
        }
    }

    fn test_openai_connection() -> OpenAiCodexConnection {
        OpenAiCodexConnection {
            session_id: "session-1".to_string(),
            state: Mutex::new(None),
            client: OpenAiCodexHttpClient::new(),
            model_catalog_cache: OpenAiModelCatalogCache::default(),
        }
    }

    #[tokio::test]
    async fn routine_provider_resolutions_do_not_reload_credential_sources() {
        let loads = Arc::new(AtomicUsize::new(0));
        let loader_loads = Arc::clone(&loads);
        let credentials = CredentialManager::for_tests_with(
            Arc::new(move || {
                loader_loads.fetch_add(1, Ordering::Relaxed);
                Credentials {
                    codex_access_token: Some("codex-secret".to_string()),
                    codex_account_id: Some("account-id".to_string()),
                    codex_installation_id: Some("installation-id".to_string()),
                    anthropic_api_key: Some("anthropic-secret".to_string()),
                }
            }),
            Arc::new(UnusedRefresher),
        );
        let registry = ProviderConnectionRegistry::new();

        for (operation, provider) in [
            ("ordinary-generation", ProviderKind::OpenAi),
            ("claude-count", ProviderKind::Claude),
            ("model-metadata", ProviderKind::OpenAi),
            ("title-sidecar", ProviderKind::OpenAi),
            ("native-compaction", ProviderKind::Claude),
        ] {
            for _ in 0..4 {
                let snapshot = credentials.snapshot();
                registry
                    .provider_for_config(provider, &snapshot, operation)
                    .await
                    .expect("routine provider resolves from snapshot");
            }
        }

        assert_eq!(loads.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn openai_compaction_updates_session_generation() {
        let registry = ProviderConnectionRegistry::new();
        let connection = registry
            .get_or_create("session-1", ProviderKind::OpenAi)
            .await;
        let ProviderConnection::OpenAi(openai) = connection.as_ref() else {
            panic!("expected OpenAI connection");
        };
        let state = openai
            .session_state(Some("account"), Some("installation"), 1)
            .await;
        assert_eq!(state.window_generation(), 0);

        registry
            .mark_compacted("session-1", ProviderKind::OpenAi, 42)
            .await;

        assert_eq!(state.window_generation(), 42);
    }

    #[tokio::test]
    async fn same_snapshot_with_incomplete_codex_identity_retains_compacted_state() {
        for (account_id, installation_id) in [
            (None, None),
            (None, Some("installation")),
            (Some("account"), None),
        ] {
            let openai = test_openai_connection();
            let snapshot = CredentialSnapshot::for_tests(Credentials {
                codex_access_token: Some("token".to_string()),
                codex_account_id: account_id.map(str::to_string),
                codex_installation_id: installation_id.map(str::to_string),
                anthropic_api_key: None,
            });
            let _first_handle = openai
                .provider_handle(&snapshot)
                .await
                .expect("first provider resolves");
            let original = openai
                .state
                .lock()
                .await
                .as_ref()
                .expect("first handle installs state")
                .state
                .clone();
            openai.mark_compacted(42).await;
            let _second_handle = openai
                .provider_handle(&snapshot)
                .await
                .expect("second provider resolves from the same snapshot");
            let resolved_again = openai
                .state
                .lock()
                .await
                .as_ref()
                .expect("second handle retains installed state")
                .state
                .clone();

            assert!(Arc::ptr_eq(&original, &resolved_again));
            assert_eq!(resolved_again.window_generation(), 42);
        }
    }

    #[tokio::test]
    async fn newer_generation_with_incomplete_codex_identity_rotates_state() {
        for (account_id, installation_id) in [
            (None, None),
            (None, Some("installation")),
            (Some("account"), None),
        ] {
            let openai = test_openai_connection();
            let original = openai.session_state(account_id, installation_id, 1).await;
            original.set_window_generation(42);
            let rotated = openai.session_state(account_id, installation_id, 2).await;

            assert!(!Arc::ptr_eq(&original, &rotated));
            assert_eq!(rotated.window_generation(), 0);
        }
    }

    #[tokio::test]
    async fn stale_codex_generation_is_isolated_from_installed_state() {
        let openai = test_openai_connection();
        let installed = openai
            .session_state(Some("account"), Some("installation"), 2)
            .await;
        installed.set_window_generation(42);

        let stale = openai
            .session_state(Some("account"), Some("installation"), 1)
            .await;
        assert!(!Arc::ptr_eq(&installed, &stale));
        stale.set_window_generation(99);
        openai.mark_compacted(43).await;

        let current = openai
            .session_state(Some("account"), Some("installation"), 2)
            .await;
        assert!(Arc::ptr_eq(&installed, &current));
        assert_eq!(current.window_generation(), 43);
        assert_eq!(stale.window_generation(), 99);
    }

    #[tokio::test]
    async fn known_codex_identity_reuses_across_generations_and_rotates_on_change() {
        let openai = test_openai_connection();
        let original = openai
            .session_state(Some("account-a"), Some("installation-a"), 1)
            .await;
        original.set_window_generation(42);
        let newer = openai
            .session_state(Some("account-a"), Some("installation-a"), 2)
            .await;
        assert!(Arc::ptr_eq(&original, &newer));
        assert_eq!(newer.window_generation(), 42);

        let changed_account = openai
            .session_state(Some("account-b"), Some("installation-a"), 2)
            .await;
        assert!(!Arc::ptr_eq(&newer, &changed_account));
        assert_eq!(changed_account.window_generation(), 0);

        let changed_installation = openai
            .session_state(Some("account-b"), Some("installation-b"), 2)
            .await;
        assert!(!Arc::ptr_eq(&changed_account, &changed_installation));
        assert_eq!(changed_installation.window_generation(), 0);
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
