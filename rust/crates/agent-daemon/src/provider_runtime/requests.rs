use agent_provider::{ModelRequest, ModelResponse, ProviderToolProfile};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::TurnId;
use anyhow::Result;

use crate::auth::Credentials;
use crate::model_metadata;
use crate::state::AppState;

use super::auth_retry::complete_with_auth_retry;
use super::prompt::assemble_agent_prompt;
use super::provider::provider_for_config;
use super::transcript::provider_transcript;

pub(crate) fn model_prompt_cache_key(config: &SessionConfig, session_id: &str) -> String {
    config
        .provider
        .prompt_cache_key()
        .map(str::to_string)
        .unwrap_or_else(|| session_id.to_string())
}

pub(crate) async fn run_model(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    turn_id: TurnId,
    model_context: ModelContext,
) -> Result<ModelResponse> {
    let request =
        build_model_request(state, config, session_id, Some(turn_id), model_context).await?;
    complete_model_request(state, config, session_id, request).await
}

pub(crate) async fn build_model_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    turn_id: Option<TurnId>,
    model_context: ModelContext,
) -> Result<ModelRequest> {
    let prompt = assemble_agent_prompt(state, config, session_id).await?;
    Ok(ModelRequest {
        model: config.provider.model.clone(),
        transcript_cache_prefix_len: None,
        prompt,
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: state
            .tools
            .provider_tools_for_provider(config.provider.kind),
        max_tokens: config.provider.max_tokens,
        reasoning_effort: model_metadata::normalize_reasoning_effort(
            config.provider.kind,
            &config.provider.model,
            config.provider.reasoning_effort,
        ),
        prompt_cache_key: Some(model_prompt_cache_key(config, session_id)),
        session_id: Some(session_id.to_string()),
        turn_id,
    })
}

pub(super) async fn complete_model_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    request: ModelRequest,
) -> Result<ModelResponse> {
    let credentials = Credentials::load();
    let provider = provider_for_config(state, config, &credentials, session_id).await?;
    Ok(complete_with_auth_retry(state, config, session_id, provider, request).await?)
}
