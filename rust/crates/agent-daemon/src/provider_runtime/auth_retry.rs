use agent_provider::{
    ModelRequest, ModelResponse, ProviderCompactionRequest, ProviderCompactionResponse,
    ProviderError, ProviderTokenCountRequest, ProviderTokenCountResponse,
};
use agent_store::SessionConfig;

use crate::auth::refresh_codex_credentials;
use crate::state::AppState;

use super::provider::{provider_for_config, ProviderHandle};

pub(super) async fn count_tokens_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: ProviderTokenCountRequest,
) -> std::result::Result<ProviderTokenCountResponse, ProviderError> {
    match provider.provider.count_tokens(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if provider.uses_codex_auth && error.status_code() == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(state, config, &credentials, session_id)
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            provider.provider.count_tokens(request).await
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn complete_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: ModelRequest,
) -> std::result::Result<ModelResponse, ProviderError> {
    match provider.provider.complete(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if provider.uses_codex_auth && error.status_code() == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(state, config, &credentials, session_id)
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            provider.provider.complete(request).await
        }
        Err(error) => Err(error),
    }
}

pub(super) async fn compact_with_auth_retry(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    provider: ProviderHandle,
    request: ProviderCompactionRequest,
) -> std::result::Result<ProviderCompactionResponse, ProviderError> {
    match provider.provider.compact(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if provider.uses_codex_auth && error.status_code() == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(state, config, &credentials, session_id)
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            provider.provider.compact(request).await
        }
        Err(error) => Err(error),
    }
}
