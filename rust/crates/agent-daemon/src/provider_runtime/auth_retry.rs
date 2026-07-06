use std::future::Future;

use agent_provider::{
    ModelRequest, ModelResponse, PreparedModelRequest, ProviderCompactionRequest,
    ProviderCompactionResponse, ProviderError, ProviderModelMetadata, ProviderTokenCountRequest,
    ProviderTokenCountResponse,
};
use agent_store::SessionConfig;

use crate::auth::refresh_codex_credentials;
use crate::state::AppState;

use super::provider::{provider_for_config, ProviderHandle};

#[derive(Default)]
pub(super) struct PreparedModelRequestState {
    request: Option<PreparedModelRequest>,
}

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
    let (first_attempt, auth_retry) = auth_attempt_requests(request);
    match call(provider, first_attempt).await {
        Ok(response) => Ok(response),
        Err(error) if uses_codex_auth && error.status_code() == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(state, config, &credentials, session_id)
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            call(provider, auth_retry).await
        }
        Err(error) => Err(error),
    }
}

async fn install_refreshed_provider(
    provider: &mut ProviderHandle,
    refreshed_provider: ProviderHandle,
    previous_account_id: Option<&str>,
    request: &ModelRequest,
    prepared: &mut PreparedModelRequestState,
) -> std::result::Result<(), ProviderError> {
    let same_account = previous_account_id.is_some()
        && previous_account_id == refreshed_provider.codex_account_id.as_deref();
    *provider = refreshed_provider;
    if !same_account {
        prepared.request = None;
    }
    ensure_compatible_prepared_request(provider, request, prepared).await
}

fn auth_attempt_requests<Req: Clone>(request: Req) -> (Req, Req) {
    (request.clone(), request)
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

#[cfg(test)]
#[path = "auth_retry_tests.rs"]
mod tests;

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
    provider: &mut ProviderHandle,
    request: ModelRequest,
    prepared: &mut PreparedModelRequestState,
) -> std::result::Result<ModelResponse, ProviderError> {
    let uses_codex_auth = provider.uses_codex_auth;
    let account_id = provider.codex_account_id.clone();
    let first_attempt = async {
        ensure_compatible_prepared_request(provider, &request, prepared).await?;
        complete_with_provider(provider, request.clone(), prepared.request.as_ref()).await
    }
    .await;
    match first_attempt {
        Ok(response) => Ok(response),
        Err(error) if uses_codex_auth && error.status_code() == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let refreshed_provider = provider_for_config(state, config, &credentials, session_id)
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            install_refreshed_provider(
                provider,
                refreshed_provider,
                account_id.as_deref(),
                &request,
                prepared,
            )
            .await?;
            complete_with_provider(provider, request, prepared.request.as_ref()).await
        }
        Err(error) => Err(error),
    }
}

async fn ensure_compatible_prepared_request(
    provider: &ProviderHandle,
    request: &ModelRequest,
    prepared: &mut PreparedModelRequestState,
) -> std::result::Result<(), ProviderError> {
    prepared.request = provider
        .provider
        .prepare_model_request(request, prepared.request.as_ref())
        .await?;
    Ok(())
}

async fn complete_with_provider(
    provider: &ProviderHandle,
    request: ModelRequest,
    prepared: Option<&PreparedModelRequest>,
) -> std::result::Result<ModelResponse, ProviderError> {
    match prepared {
        Some(prepared) => {
            provider
                .provider
                .complete_prepared(request, prepared.clone())
                .await
        }
        None => provider.provider.complete(request).await,
    }
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
