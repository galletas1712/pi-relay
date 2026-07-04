use agent_provider::{ModelProvider, ProviderModelMetadata};
use agent_store::SessionConfig;
use anyhow::Result;

use crate::auth::{CodexAccessTokenFingerprint, Credentials};
use crate::state::AppState;

use super::auth_retry::model_metadata_with_auth_retry;

pub(super) struct ProviderHandle {
    pub(super) provider: Box<dyn ModelProvider>,
    pub(super) codex_access_token_fingerprint: Option<CodexAccessTokenFingerprint>,
}

pub(super) async fn provider_for_config(
    state: &AppState,
    config: &SessionConfig,
    credentials: &Credentials,
    session_id: &str,
) -> Result<ProviderHandle> {
    state
        .provider_connections
        .provider_for_config(config.provider.kind, credentials, session_id)
        .await
}

pub(crate) async fn model_metadata_for_config(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
) -> Result<Option<ProviderModelMetadata>> {
    let credentials = Credentials::load();
    let provider = provider_for_config(state, config, &credentials, session_id).await?;
    Ok(model_metadata_with_auth_retry(
        state,
        config,
        session_id,
        provider,
        config.provider.model.clone(),
    )
    .await?)
}
