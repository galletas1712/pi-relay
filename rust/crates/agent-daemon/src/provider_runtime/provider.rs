use agent_provider::ModelProvider;
use agent_store::SessionConfig;
use anyhow::Result;

use crate::auth::Credentials;
use crate::state::AppState;

pub(super) struct ProviderHandle {
    pub(super) provider: Box<dyn ModelProvider>,
    pub(super) uses_codex_auth: bool,
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
