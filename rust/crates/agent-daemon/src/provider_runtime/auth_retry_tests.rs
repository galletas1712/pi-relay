use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use agent_provider::{
    openai::OpenAiProvider, ModelProvider, ModelRequest, ModelResponse, ModelTranscriptEntry,
    PromptSections, ProviderCompactionRequest, ProviderCompactionResponse, ProviderError,
    ProviderModelInput, ProviderResult, ProviderToolProfile,
};
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, ReasoningEffort, TranscriptItem, TurnId, UserMessage};
use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Notify;

use super::{
    auth_attempt_requests, ensure_compatible_prepared_request, install_refreshed_provider,
    with_auth_retry, PreparedModelRequestState,
};
use crate::auth::{CodexCredentialRefresher, CredentialManager, CredentialSnapshot, Credentials};
use crate::provider_runtime::provider::ProviderHandle;
use crate::provider_runtime::ProviderConnectionRegistry;

fn test_config(kind: ProviderKind) -> SessionConfig {
    SessionConfig {
        project_id: None,
        outer_cwd: "/tmp".to_string(),
        workspaces: Vec::new(),
        system_prompt: "test prompt".to_string(),
        provider: agent_vocab::ProviderConfig {
            kind,
            model: "test-model".to_string(),
            reasoning_effort: ReasoningEffort::Medium,
            max_tokens: None,
            prompt_cache: None,
        },
        metadata: serde_json::Value::Null,
    }
}

struct BlockingTestRefresher {
    entered: Arc<Notify>,
    release: Arc<Notify>,
    replacement: Credentials,
}

#[async_trait]
impl CodexCredentialRefresher for BlockingTestRefresher {
    async fn refresh(&self, _prior: &Credentials) -> Result<Credentials> {
        self.entered.notify_one();
        self.release.notified().await;
        Ok(self.replacement.clone())
    }
}

#[tokio::test]
async fn anthropic_changed_reload_retries_once_and_unchanged_reload_preserves_error() {
    let loads = Arc::new(AtomicUsize::new(0));
    let loader_loads = Arc::clone(&loads);
    let manager = CredentialManager::for_tests_with(
        Arc::new(move || {
            let load = loader_loads.fetch_add(1, Ordering::Relaxed);
            credentials(
                "codex-secret",
                Some("account-a"),
                if load == 0 { "old-key" } else { "new-key" },
            )
        }),
        Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            replacement: Default::default(),
            entered: None,
        }),
    );
    let connections = ProviderConnectionRegistry::new();
    connections
        .install_test_provider(Box::new(UnusedProvider), false, None)
        .await;
    let generations = Arc::new(Mutex::new(Vec::new()));
    let recorded_generations = Arc::clone(&generations);
    let result = with_auth_retry(
        &manager,
        &connections,
        &test_config(ProviderKind::Claude),
        "anthropic-rotation",
        test_handle(manager.snapshot(), false),
        (),
        move |provider, ()| {
            let recorded_generations = Arc::clone(&recorded_generations);
            async move {
                let generation = provider.credentials.generation();
                recorded_generations
                    .lock()
                    .expect("generation lock")
                    .push(generation);
                if generation == 1 {
                    Err(ProviderError::Status {
                        status: 401,
                        message: "original anthropic auth failure".to_string(),
                    })
                } else {
                    Ok(generation)
                }
            }
        },
    )
    .await
    .expect("changed credentials retry");
    assert_eq!(result, 2);
    assert_eq!(*generations.lock().expect("generation lock"), vec![1, 2]);
    assert_eq!(loads.load(Ordering::Relaxed), 2);

    connections
        .install_test_provider(Box::new(UnusedProvider), false, None)
        .await;
    let current = manager.snapshot();
    let resolved = connections
        .provider_for_config(ProviderKind::Claude, &current, "subsequent")
        .await
        .expect("subsequent provider resolves");
    assert_eq!(resolved.credentials.generation(), 2);
    assert_eq!(loads.load(Ordering::Relaxed), 2);

    let unchanged_loads = Arc::new(AtomicUsize::new(0));
    let unchanged_loader_loads = Arc::clone(&unchanged_loads);
    let unchanged = CredentialManager::for_tests_with(
        Arc::new(move || {
            unchanged_loader_loads.fetch_add(1, Ordering::Relaxed);
            credentials("codex-secret", Some("account-a"), "same-key")
        }),
        Arc::new(TestRefresher {
            calls: AtomicUsize::new(0),
            replacement: Default::default(),
            entered: None,
        }),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let recorded_calls = Arc::clone(&calls);
    let error = with_auth_retry(
        &unchanged,
        &ProviderConnectionRegistry::new(),
        &test_config(ProviderKind::Claude),
        "anthropic-unchanged",
        test_handle(unchanged.snapshot(), false),
        (),
        move |_, ()| {
            let recorded_calls = Arc::clone(&recorded_calls);
            async move {
                recorded_calls.fetch_add(1, Ordering::Relaxed);
                Err::<(), _>(ProviderError::Status {
                    status: 403,
                    message: "original unchanged failure".to_string(),
                })
            }
        },
    )
    .await
    .expect_err("unchanged credentials keep original auth error");
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(unchanged_loads.load(Ordering::Relaxed), 2);
    match error {
        ProviderError::Status { status, message } => {
            assert_eq!(
                (status, message.as_str()),
                (403, "original unchanged failure")
            );
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[tokio::test]
async fn concurrent_codex_401s_refresh_once_and_both_retry_on_newest_generation() {
    let entered = Arc::new(tokio::sync::Barrier::new(2));
    let refresher = Arc::new(TestRefresher {
        calls: AtomicUsize::new(0),
        replacement: credentials("new-token", Some("account-a"), "anthropic-key"),
        entered: Some(Arc::clone(&entered)),
    });
    let manager = CredentialManager::for_tests_with(
        Arc::new(|| credentials("old-token", Some("account-a"), "anthropic-key")),
        refresher.clone(),
    );
    let connections = ProviderConnectionRegistry::new();
    for _ in 0..2 {
        connections
            .install_test_provider(
                Box::new(UnusedProvider),
                true,
                Some("account-a".to_string()),
            )
            .await;
    }
    let observed = manager.snapshot();
    let spawn_call = |session_id: &'static str| {
        let manager = manager.clone();
        let connections = connections.clone();
        let observed = observed.clone();
        tokio::spawn(async move {
            with_auth_retry(
                &manager,
                &connections,
                &test_config(ProviderKind::OpenAi),
                session_id,
                test_handle(observed, true),
                (),
                |provider, ()| async move {
                    let generation = provider.credentials.generation();
                    if generation == 1 {
                        Err(ProviderError::Status {
                            status: 401,
                            message: "expired".to_string(),
                        })
                    } else {
                        Ok((
                            generation,
                            provider
                                .credentials
                                .credentials()
                                .codex_access_token
                                .as_deref()
                                == Some("new-token"),
                            provider
                                .credentials
                                .credentials()
                                .codex_account_id
                                .as_deref()
                                == Some("account-a"),
                        ))
                    }
                },
            )
            .await
        })
    };

    let first = spawn_call("concurrent-a");
    entered.wait().await;
    let second = spawn_call("concurrent-b");
    let first = first.await.unwrap().expect("first retries");
    let second = second.await.unwrap().expect("second retries");

    assert_eq!(refresher.calls.load(Ordering::Relaxed), 1);
    let expected = (2, true, true);
    assert_eq!(first, expected);
    assert_eq!(second, expected);
    assert_eq!(manager.snapshot().generation(), 2);
}

#[tokio::test]
async fn blocked_codex_refresh_does_not_delay_anthropic_reload_and_retry() {
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let refresher = Arc::new(BlockingTestRefresher {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
        replacement: credentials("new-token", Some("account-a"), "stale-key"),
    });
    let loads = Arc::new(AtomicUsize::new(0));
    let loader_loads = Arc::clone(&loads);
    let manager = CredentialManager::for_tests_with(
        Arc::new(move || {
            let load = loader_loads.fetch_add(1, Ordering::Relaxed);
            credentials(
                "old-token",
                Some("account-a"),
                if load == 0 { "old-key" } else { "new-key" },
            )
        }),
        refresher,
    );
    let connections = ProviderConnectionRegistry::new();
    for (uses_codex_auth, account_id) in [(false, None), (true, Some("account-a".to_string()))] {
        connections
            .install_test_provider(Box::new(UnusedProvider), uses_codex_auth, account_id)
            .await;
    }
    let observed = manager.snapshot();
    let codex_manager = manager.clone();
    let codex_connections = connections.clone();
    let codex_observed = observed.clone();
    let codex = tokio::spawn(async move {
        with_auth_retry(
            &codex_manager,
            &codex_connections,
            &test_config(ProviderKind::OpenAi),
            "blocked-codex",
            test_handle(codex_observed, true),
            (),
            |provider, ()| async move {
                if provider.credentials.codex_generation() == 1 {
                    Err(ProviderError::Status {
                        status: 401,
                        message: "expired".to_string(),
                    })
                } else {
                    Ok(provider.credentials.codex_generation())
                }
            },
        )
        .await
    });
    entered.notified().await;

    let anthropic = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        with_auth_retry(
            &manager,
            &connections,
            &test_config(ProviderKind::Claude),
            "prompt-anthropic",
            test_handle(observed, false),
            (),
            |provider, ()| async move {
                if provider.credentials.anthropic_generation() == 1 {
                    Err(ProviderError::Status {
                        status: 401,
                        message: "expired".to_string(),
                    })
                } else {
                    Ok(provider.credentials.anthropic_generation())
                }
            },
        ),
    )
    .await
    .expect("Anthropic reload and retry are not blocked by Codex")
    .expect("Anthropic retry succeeds");
    assert_eq!(anthropic, 2);

    release.notify_one();
    assert_eq!(
        codex
            .await
            .expect("Codex task joins")
            .expect("Codex retry succeeds"),
        2
    );
    let current = manager.snapshot();
    assert_eq!(
        (
            current.codex_generation(),
            current.anthropic_generation(),
            current.credentials().codex_access_token.as_deref(),
            current.credentials().anthropic_api_key.as_deref(),
        ),
        (2, 2, Some("new-token"), Some("new-key"))
    );
}

fn credentials(codex_token: &str, account: Option<&str>, anthropic_key: &str) -> Credentials {
    Credentials {
        codex_access_token: Some(codex_token.to_string()),
        codex_account_id: account.map(str::to_string),
        codex_installation_id: None,
        anthropic_api_key: Some(anthropic_key.to_string()),
    }
}

struct UnusedProvider;

#[async_trait]
impl ModelProvider for UnusedProvider {
    async fn complete(&self, _request: ModelRequest) -> ProviderResult<ModelResponse> {
        unreachable!("test closure does not call provider")
    }

    async fn compact(
        &self,
        _request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        unreachable!("test closure does not call provider")
    }
}

fn test_handle(snapshot: CredentialSnapshot, uses_codex_auth: bool) -> ProviderHandle {
    ProviderHandle {
        provider: Box::new(UnusedProvider),
        uses_codex_auth,
        codex_account_id: snapshot.credentials().codex_account_id.clone(),
        credentials: snapshot,
    }
}

struct TestRefresher {
    calls: AtomicUsize,
    replacement: Credentials,
    entered: Option<Arc<tokio::sync::Barrier>>,
}

#[async_trait]
impl CodexCredentialRefresher for TestRefresher {
    async fn refresh(&self, _prior: &Credentials) -> Result<Credentials> {
        self.calls.fetch_add(1, Ordering::Relaxed);
        if let Some(entered) = &self.entered {
            entered.wait().await;
        }
        Ok(self.replacement.clone())
    }
}

#[test]
fn auth_retry_request_clone_reuses_logical_input_allocation() {
    let input = Arc::new(ProviderModelInput::new(
        "test-model",
        PromptSections::stable("stable prompt"),
        vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
            UserMessage::text("large transcript"),
        ))],
        ProviderToolProfile::None,
        Vec::new(),
        ReasoningEffort::Medium,
    ));
    let (first_attempt, auth_retry) = auth_attempt_requests(ModelRequest::new(input));

    assert!(std::ptr::eq::<ProviderModelInput>(
        &*first_attempt,
        &*auth_retry
    ));
    assert!(std::ptr::eq(
        first_attempt.transcript().as_ptr(),
        auth_retry.transcript().as_ptr()
    ));
    assert!(std::ptr::eq(
        first_attempt.tools().as_ptr(),
        auth_retry.tools().as_ptr()
    ));
    assert!(std::ptr::eq(first_attempt.prompt(), auth_retry.prompt()));
}

#[tokio::test]
async fn account_compatibility_controls_prepared_byte_reuse() {
    for (previous_account, replacement_account, reuses_allocation) in [
        (None, None, false),
        (Some("account-a"), Some("account-b"), false),
        (Some("account-a"), Some("account-a"), true),
    ] {
        let request = ModelRequest::new(Arc::new(
            ProviderModelInput::new(
                "test-model",
                PromptSections::stable("stable prompt"),
                vec![ModelTranscriptEntry::from(TranscriptItem::UserMessage(
                    UserMessage::text("large transcript"),
                ))],
                ProviderToolProfile::None,
                Vec::new(),
                ReasoningEffort::Medium,
            )
            .with_session_id("session-1"),
        ))
        .with_turn_id(TurnId(1));
        let original =
            OpenAiProvider::codex("original-token", previous_account.map(str::to_string), None);
        original.install_model_metadata_for_test("test-model").await;
        let mut provider = ProviderHandle {
            provider: Box::new(original),
            uses_codex_auth: true,
            codex_account_id: previous_account.map(str::to_string),
            credentials: CredentialSnapshot::for_tests(Credentials {
                codex_account_id: previous_account.map(str::to_string),
                ..Default::default()
            }),
        };
        let mut prepared = PreparedModelRequestState::default();
        ensure_compatible_prepared_request(&provider, &request, &mut prepared)
            .await
            .expect("original provider prepares");
        let original_prepared = prepared
            .request
            .as_ref()
            .expect("OpenAI prepares bytes")
            .clone();

        let replacement = OpenAiProvider::codex(
            "replacement-token",
            replacement_account.map(str::to_string),
            None,
        );
        replacement
            .install_model_metadata_for_test("test-model")
            .await;
        install_refreshed_provider(
            &mut provider,
            ProviderHandle {
                provider: Box::new(replacement),
                uses_codex_auth: true,
                codex_account_id: replacement_account.map(str::to_string),
                credentials: CredentialSnapshot::for_tests(Credentials {
                    codex_account_id: replacement_account.map(str::to_string),
                    ..Default::default()
                }),
            },
            previous_account,
            &request,
            &mut prepared,
        )
        .await
        .expect("replacement provider prepares");

        let replacement_allocation = prepared
            .request
            .as_ref()
            .expect("replacement bytes exist")
            .body_allocation()
            .expect("replacement allocation");
        let original_allocation = original_prepared
            .body_allocation()
            .expect("original allocation");
        assert_eq!(
            replacement_allocation == original_allocation,
            reuses_allocation
        );
    }
}
