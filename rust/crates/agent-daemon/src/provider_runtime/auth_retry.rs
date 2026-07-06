use std::future::Future;

use agent_provider::{
    ModelRequest, ModelResponse, ProviderCompactionRequest, ProviderCompactionResponse,
    ProviderError, ProviderModelMetadata, ProviderTokenCountRequest, ProviderTokenCountResponse,
};
use agent_store::SessionConfig;

use crate::auth::refresh_codex_credentials;
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
    let uses_codex_auth = provider.uses_codex_auth;
    match call(provider, request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if uses_codex_auth && error.status_code() == Some(401) => {
            agent_perf::provider_auth_retry();
            agent_perf::auth_refresh();
            let preparation = agent_perf::phase(agent_perf::Phase::RequestPreparation);
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(state, config, &credentials, session_id)
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            drop(preparation);
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
    agent_perf::request_copied();
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
    agent_perf::request_copied();
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
    agent_perf::request_copied();
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
