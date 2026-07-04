use std::future::Future;

use agent_provider::{
    ModelRequest, ModelResponse, ProviderCompactionRequest, ProviderCompactionResponse,
    ProviderError, ProviderModelMetadata, ProviderTokenCountRequest, ProviderTokenCountResponse,
};
use agent_store::SessionConfig;

use crate::auth::{refresh_codex_credentials, CodexAccessTokenFingerprint};
use crate::state::AppState;

use super::provider::{provider_for_config, ProviderHandle};

/// Run a provider call, and on a Codex 401 refresh credentials once and retry
/// against a freshly built provider. `call` is invoked at most twice, so the
/// request must be cloneable and the closure must be re-runnable.
async fn with_codex_auth_retry<Req, Resp, F, Fut>(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: Req,
    call: F,
) -> std::result::Result<Resp, ProviderError>
where
    Req: Clone,
    F: Fn(ProviderHandle, Req) -> Fut,
    Fut: Future<Output = std::result::Result<Resp, ProviderError>>,
{
    with_codex_auth_retry_and_rebuild(provider, request, call, |failed_fingerprint| async move {
        let credentials = refresh_codex_credentials(failed_fingerprint)
            .await
            .map_err(|error| ProviderError::Provider(error.to_string()))?;
        provider_for_config(state, config, &credentials, session_id)
            .await
            .map_err(|error| ProviderError::Provider(error.to_string()))
    })
    .await
}

async fn with_codex_auth_retry_and_rebuild<Req, Resp, F, Fut, Rebuild, RebuildFuture>(
    provider: ProviderHandle,
    request: Req,
    call: F,
    rebuild: Rebuild,
) -> std::result::Result<Resp, ProviderError>
where
    Req: Clone,
    F: Fn(ProviderHandle, Req) -> Fut,
    Fut: Future<Output = std::result::Result<Resp, ProviderError>>,
    Rebuild: FnOnce(CodexAccessTokenFingerprint) -> RebuildFuture,
    RebuildFuture: Future<Output = std::result::Result<ProviderHandle, ProviderError>>,
{
    let failed_fingerprint = provider.codex_access_token_fingerprint.clone();
    match call(provider, request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if failed_fingerprint.is_some() && error.status_code() == Some(401) => {
            let provider = rebuild(failed_fingerprint.expect("checked above")).await?;
            call(provider, request).await
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn model_metadata_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    model: String,
) -> std::result::Result<Option<ProviderModelMetadata>, ProviderError> {
    with_codex_auth_retry(
        state,
        config,
        session_id,
        provider,
        model,
        |provider, model| async move { provider.provider.model_metadata(&model).await },
    )
    .await
}

pub(super) async fn count_tokens_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: ProviderTokenCountRequest,
) -> std::result::Result<ProviderTokenCountResponse, ProviderError> {
    with_codex_auth_retry(
        state,
        config,
        session_id,
        provider,
        request,
        |provider, request| async move { provider.provider.count_tokens(request).await },
    )
    .await
}

pub(super) async fn complete_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: ModelRequest,
) -> std::result::Result<ModelResponse, ProviderError> {
    with_codex_auth_retry(
        state,
        config,
        session_id,
        provider,
        request,
        |provider, request| async move { provider.provider.complete(request).await },
    )
    .await
}

pub(super) async fn compact_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: ProviderCompactionRequest,
) -> std::result::Result<ProviderCompactionResponse, ProviderError> {
    with_codex_auth_retry(
        state,
        config,
        session_id,
        provider,
        request,
        |provider, request| async move { provider.provider.compact(request).await },
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::{CodexCredentialRefreshCoordinator, Credentials};
    use agent_provider::{ModelProvider, ProviderModelMetadata, ProviderResult};
    use async_trait::async_trait;
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    const FAILED_TOKEN: &str = "deterministic-failed-generation";
    const REFRESHED_TOKEN: &str = "deterministic-refreshed-generation";

    struct CatalogAttemptProvider {
        caller: usize,
        refreshed: bool,
        attempts: Arc<Vec<AtomicUsize>>,
        generations: Arc<Mutex<Vec<Vec<bool>>>>,
    }

    #[async_trait]
    impl ModelProvider for CatalogAttemptProvider {
        async fn complete(&self, _request: ModelRequest) -> ProviderResult<ModelResponse> {
            Err(ProviderError::Provider(
                "test provider only supports model metadata".to_string(),
            ))
        }

        async fn model_metadata(
            &self,
            _model: &str,
        ) -> ProviderResult<Option<ProviderModelMetadata>> {
            self.attempts[self.caller].fetch_add(1, Ordering::SeqCst);
            self.generations
                .lock()
                .expect("generation record lock")
                .get_mut(self.caller)
                .expect("caller record exists")
                .push(self.refreshed);
            if self.refreshed {
                Ok(Some(ProviderModelMetadata {
                    max_input_tokens: Some(372_000),
                    recommended_auto_compact_tokens: Some(334_800),
                }))
            } else {
                Err(ProviderError::ModelCatalog {
                    status: Some(401),
                    message: "deterministic unauthorized response".to_string(),
                })
            }
        }

        async fn compact(
            &self,
            _request: ProviderCompactionRequest,
        ) -> ProviderResult<ProviderCompactionResponse> {
            Err(ProviderError::Provider(
                "test provider only supports model metadata".to_string(),
            ))
        }
    }

    fn provider_handle(
        caller: usize,
        token: &str,
        attempts: Arc<Vec<AtomicUsize>>,
        generations: Arc<Mutex<Vec<Vec<bool>>>>,
    ) -> ProviderHandle {
        ProviderHandle {
            provider: Box::new(CatalogAttemptProvider {
                caller,
                refreshed: token == REFRESHED_TOKEN,
                attempts,
                generations,
            }),
            codex_access_token_fingerprint: Some(CodexAccessTokenFingerprint::new(token)),
        }
    }

    fn credentials(token: &str, account_id: &str) -> Credentials {
        Credentials {
            codex_access_token: Some(token.to_string()),
            codex_account_id: Some(account_id.to_string()),
            codex_installation_id: None,
            anthropic_api_key: None,
        }
    }

    async fn read_http_request(stream: &mut tokio::net::TcpStream) {
        let mut request = Vec::new();
        let mut buffer = [0; 1024];
        let (header_end, content_length) = loop {
            let read = stream.read(&mut buffer).await.expect("request reads");
            assert!(read > 0, "request closed before headers");
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (header_end, content_length);
        };
        let body_end = header_end + 4 + content_length;
        while request.len() < body_end {
            let read = stream.read(&mut buffer).await.expect("request body reads");
            assert!(read > 0, "request closed before body");
            request.extend_from_slice(&buffer[..read]);
        }
        let headers = String::from_utf8_lossy(&request[..header_end]).to_ascii_lowercase();
        assert!(headers.starts_with("post /oauth/token http/1.1\r\n"));
        let body: serde_json::Value =
            serde_json::from_slice(&request[header_end + 4..body_end]).expect("body is JSON");
        assert_eq!(body["refresh_token"], "deterministic-refresh-token");
    }

    async fn post_fake_oauth_refresh(
        oauth_url: String,
        stored_credentials: Arc<Mutex<Credentials>>,
    ) -> anyhow::Result<Credentials> {
        let response = reqwest::Client::new()
            .post(oauth_url)
            .json(&json!({
                "refresh_token": "deterministic-refresh-token",
            }))
            .send()
            .await?
            .error_for_status()?
            .json::<serde_json::Value>()
            .await?;
        let token = response
            .get("access_token")
            .and_then(serde_json::Value::as_str)
            .expect("fake OAuth response has access token");
        let mut stored = stored_credentials.lock().expect("credential store lock");
        stored.codex_access_token = Some(token.to_string());
        stored.codex_account_id = Some("refreshed-account".to_string());
        Ok(stored.clone())
    }

    #[tokio::test]
    async fn concurrent_same_401_generation_posts_oauth_once_and_retries_once_each() {
        const CALLERS: usize = 24;
        let oauth_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("OAuth listener binds");
        let oauth_url = format!(
            "http://{}/oauth/token",
            oauth_listener.local_addr().expect("OAuth listener address")
        );
        let oauth_posts = Arc::new(AtomicUsize::new(0));
        let server_posts = Arc::clone(&oauth_posts);
        let oauth_server = tokio::spawn(async move {
            loop {
                let Ok(Ok((mut stream, _))) =
                    tokio::time::timeout(Duration::from_millis(200), oauth_listener.accept()).await
                else {
                    break;
                };
                server_posts.fetch_add(1, Ordering::SeqCst);
                read_http_request(&mut stream).await;
                let body = json!({
                    "access_token": REFRESHED_TOKEN,
                    "refresh_token": "rotated-refresh-token",
                })
                .to_string();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("OAuth response writes");
            }
        });

        let coordinator = CodexCredentialRefreshCoordinator::default();
        let stored_credentials = Arc::new(Mutex::new(credentials(FAILED_TOKEN, "failed-account")));
        let attempts = Arc::new(
            (0..CALLERS)
                .map(|_| AtomicUsize::new(0))
                .collect::<Vec<_>>(),
        );
        let generations = Arc::new(Mutex::new(vec![Vec::new(); CALLERS]));
        let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));
        let mut callers = Vec::new();
        for caller in 0..CALLERS {
            let coordinator = coordinator.clone();
            let stored_credentials = Arc::clone(&stored_credentials);
            let attempts = Arc::clone(&attempts);
            let generations = Arc::clone(&generations);
            let oauth_url = oauth_url.clone();
            let barrier = Arc::clone(&barrier);
            callers.push(tokio::spawn(async move {
                let initial = provider_handle(
                    caller,
                    FAILED_TOKEN,
                    Arc::clone(&attempts),
                    Arc::clone(&generations),
                );
                barrier.wait().await;
                with_codex_auth_retry_and_rebuild(
                    initial,
                    "gpt-5.6-sol".to_string(),
                    |provider, model| async move {
                        provider.provider.model_metadata(&model).await.map(|_| ())
                    },
                    move |failed_fingerprint| {
                        let coordinator = coordinator.clone();
                        let load_store = Arc::clone(&stored_credentials);
                        let refresh_store = Arc::clone(&stored_credentials);
                        let attempts = Arc::clone(&attempts);
                        let generations = Arc::clone(&generations);
                        let oauth_url = oauth_url.clone();
                        async move {
                            let current = coordinator
                                .credentials_after_401_with(
                                    failed_fingerprint,
                                    move || {
                                        Ok(load_store
                                            .lock()
                                            .expect("credential store lock")
                                            .clone())
                                    },
                                    move || post_fake_oauth_refresh(oauth_url, refresh_store),
                                )
                                .await
                                .map_err(|error| ProviderError::Provider(error.to_string()))?;
                            let token = current
                                .codex_access_token
                                .as_deref()
                                .expect("rebuilt credentials have access token");
                            Ok(provider_handle(caller, token, attempts, generations))
                        }
                    },
                )
                .await
            }));
        }
        barrier.wait().await;

        for caller in callers {
            caller
                .await
                .expect("caller joins")
                .expect("caller succeeds with refreshed credentials");
        }
        oauth_server.await.expect("OAuth server joins");

        assert_eq!(oauth_posts.load(Ordering::SeqCst), 1);
        assert!(attempts
            .iter()
            .all(|attempts| attempts.load(Ordering::SeqCst) == 2));
        assert!(generations
            .lock()
            .expect("generation record lock")
            .iter()
            .all(|seen| seen == &[false, true]));
    }

    #[tokio::test]
    async fn concurrent_refresh_failure_is_attempted_once_and_never_retries_provider() {
        const CALLERS: usize = 12;
        let attempts = Arc::new(
            (0..CALLERS)
                .map(|_| AtomicUsize::new(0))
                .collect::<Vec<_>>(),
        );
        let generations = Arc::new(Mutex::new(vec![Vec::new(); CALLERS]));
        let coordinator = CodexCredentialRefreshCoordinator::default();
        let refreshes = Arc::new(AtomicUsize::new(0));
        let barrier = Arc::new(tokio::sync::Barrier::new(CALLERS + 1));
        let mut callers = Vec::new();
        for caller in 0..CALLERS {
            let attempts = Arc::clone(&attempts);
            let generations = Arc::clone(&generations);
            let coordinator = coordinator.clone();
            let refreshes = Arc::clone(&refreshes);
            let barrier = Arc::clone(&barrier);
            callers.push(tokio::spawn(async move {
                barrier.wait().await;
                with_codex_auth_retry_and_rebuild(
                    provider_handle(
                        caller,
                        FAILED_TOKEN,
                        Arc::clone(&attempts),
                        Arc::clone(&generations),
                    ),
                    "gpt-5.6-sol".to_string(),
                    |provider, model| async move {
                        provider.provider.model_metadata(&model).await.map(|_| ())
                    },
                    move |failed_fingerprint| async move {
                        coordinator
                            .credentials_after_401_with(
                                failed_fingerprint,
                                move || Ok(credentials(FAILED_TOKEN, "failed-account")),
                                move || {
                                    refreshes.fetch_add(1, Ordering::SeqCst);
                                    async { Err(anyhow::anyhow!("deterministic refresh failure")) }
                                },
                            )
                            .await
                            .map(|_| unreachable!("failed refresh cannot rebuild a provider"))
                            .map_err(|error| ProviderError::Provider(error.to_string()))
                    },
                )
                .await
            }));
        }
        barrier.wait().await;
        for caller in callers {
            let error = caller
                .await
                .expect("caller joins")
                .expect_err("refresh failure must surface");
            assert!(error.to_string().contains("deterministic refresh failure"));
        }

        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert!(attempts
            .iter()
            .all(|attempts| attempts.load(Ordering::SeqCst) == 1));
        assert!(generations
            .lock()
            .expect("generation record lock")
            .iter()
            .all(|seen| seen == &[false]));
    }

    #[tokio::test]
    async fn account_switch_reuses_new_token_and_later_generation_refreshes_independently() {
        let attempts = Arc::new(vec![AtomicUsize::new(0)]);
        let generations = Arc::new(Mutex::new(vec![Vec::new()]));
        let coordinator = CodexCredentialRefreshCoordinator::default();
        let current = credentials(REFRESHED_TOKEN, "different-account");
        let refreshes = Arc::new(AtomicUsize::new(0));
        let counted_refreshes = Arc::clone(&refreshes);
        let next_generation_coordinator = coordinator.clone();
        let rebuilt_attempts = Arc::clone(&attempts);
        let rebuilt_generations = Arc::clone(&generations);
        with_codex_auth_retry_and_rebuild(
            provider_handle(
                0,
                FAILED_TOKEN,
                Arc::clone(&attempts),
                Arc::clone(&generations),
            ),
            "gpt-5.6-sol".to_string(),
            |provider, model| async move {
                provider.provider.model_metadata(&model).await.map(|_| ())
            },
            move |failed_fingerprint| {
                let attempts = Arc::clone(&rebuilt_attempts);
                let generations = Arc::clone(&rebuilt_generations);
                async move {
                    let current = coordinator
                        .credentials_after_401_with(
                            failed_fingerprint,
                            move || Ok(current),
                            move || {
                                counted_refreshes.fetch_add(1, Ordering::SeqCst);
                                async {
                                    Err(anyhow::anyhow!(
                                        "refresh must not run for a newer token generation"
                                    ))
                                }
                            },
                        )
                        .await
                        .map_err(|error| ProviderError::Provider(error.to_string()))?;
                    assert_eq!(
                        current.codex_account_id.as_deref(),
                        Some("different-account")
                    );
                    Ok(provider_handle(
                        0,
                        current
                            .codex_access_token
                            .as_deref()
                            .expect("current access token exists"),
                        attempts,
                        generations,
                    ))
                }
            },
        )
        .await
        .expect("new credentials should be reused");

        assert_eq!(refreshes.load(Ordering::SeqCst), 0);
        assert_eq!(attempts[0].load(Ordering::SeqCst), 2);
        assert_eq!(
            generations.lock().expect("generation record lock")[0],
            [false, true]
        );

        let next_refreshes = Arc::clone(&refreshes);
        let next = next_generation_coordinator
            .credentials_after_401_with(
                CodexAccessTokenFingerprint::new(REFRESHED_TOKEN),
                || Ok(credentials(REFRESHED_TOKEN, "different-account")),
                move || {
                    next_refreshes.fetch_add(1, Ordering::SeqCst);
                    async { Ok(credentials("next-token-generation", "next-account")) }
                },
            )
            .await
            .expect("a distinct failed token generation can refresh");
        assert_eq!(refreshes.load(Ordering::SeqCst), 1);
        assert_eq!(
            next.codex_access_token.as_deref(),
            Some("next-token-generation")
        );
        assert_eq!(next.codex_account_id.as_deref(), Some("next-account"));
    }
}
