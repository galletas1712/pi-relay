use std::sync::Arc;

use agent_provider::{ModelRequest, ModelResponse, ProviderModelInput, ProviderToolProfile};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::TurnId;
use anyhow::Result;

use crate::auth::Credentials;
use crate::state::AppState;
use crate::types::RuntimeConfig;

use super::auth_retry::{complete_with_auth_retry, PreparedModelRequestState};
use super::prompt::{effective_prompt_profile, provider_tools_for_session};
use super::provider::{provider_for_config, ProviderHandle};
use super::transcript::provider_transcript_owned;

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
    call: &mut PreparedModelCall,
) -> Result<ModelResponse> {
    #[cfg(test)]
    if let Some(result) = injected_model_result(config, session_id) {
        return result;
    }
    if call.provider.is_none() {
        let credentials = Credentials::load();
        call.provider = Some(provider_for_config(state, config, &credentials, session_id).await?);
    }
    let provider = call
        .provider
        .as_mut()
        .expect("provider was installed for the prepared model call");
    Ok(complete_with_auth_retry(
        state,
        config,
        session_id,
        provider,
        call.request.clone(),
        &mut call.prepared,
    )
    .await?)
}

pub(crate) struct PreparedModelCall {
    request: ModelRequest,
    prepared: PreparedModelRequestState,
    provider: Option<ProviderHandle>,
}

impl PreparedModelCall {
    pub(crate) fn new(
        input: Arc<ProviderModelInput>,
        turn_id: TurnId,
        max_tokens: Option<u32>,
    ) -> Self {
        let mut request = ModelRequest::new(input).with_turn_id(turn_id);
        // Provider adapters apply authoritative discovered/static output
        // ceilings. Do not pre-clamp here or stale daemon metadata could
        // override a newer Models API result.
        request.max_tokens = max_tokens;
        Self {
            request,
            prepared: PreparedModelRequestState::default(),
            provider: None,
        }
    }
}

#[cfg(test)]
fn injected_model_result(
    config: &SessionConfig,
    session_id: &str,
) -> Option<Result<ModelResponse>> {
    use agent_provider::{ModelStopReason, ProviderError};
    use agent_vocab::{AssistantItem, AssistantMessage, ToolCall, ToolCallId};

    let result = config
        .metadata
        .pointer("/fault_injection/model_result")
        .and_then(serde_json::Value::as_str)?;
    record_injected_provider_start(session_id);
    Some(match result {
        "complete" => Ok(ModelResponse {
            assistant: AssistantMessage {
                items: vec![AssistantItem::Text("injected completion".to_string())],
            },
            provider_replay: Vec::new(),
            usage: None,
            stop_reason: ModelStopReason::Complete,
            stop_details: None,
        }),
        "tool" => Ok(ModelResponse {
            assistant: AssistantMessage {
                items: vec![AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::from_u64(1),
                    tool_name: "Bash".to_string(),
                    args_json: r#"{"command":"true"}"#.to_string(),
                })],
            },
            provider_replay: Vec::new(),
            usage: None,
            stop_reason: ModelStopReason::Complete,
            stop_details: None,
        }),
        "overflow" => Err(ProviderError::Status {
            status: 400,
            message: "context_length_exceeded: injected overflow".to_string(),
        }
        .into()),
        other => panic!("unsupported injected model result {other}"),
    })
}

#[cfg(test)]
fn injected_provider_starts() -> &'static std::sync::Mutex<std::collections::HashMap<String, usize>>
{
    static STARTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, usize>>> =
        std::sync::OnceLock::new();
    STARTS.get_or_init(Default::default)
}

#[cfg(test)]
fn record_injected_provider_start(session_id: &str) {
    *injected_provider_starts()
        .lock()
        .expect("injected provider counter lock poisoned")
        .entry(session_id.to_string())
        .or_default() += 1;
}

#[cfg(test)]
pub(crate) fn injected_provider_start_count(session_id: &str) -> usize {
    injected_provider_starts()
        .lock()
        .expect("injected provider counter lock poisoned")
        .get(session_id)
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
pub(crate) async fn build_model_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    turn_id: Option<TurnId>,
    model_context: &ModelContext,
) -> Result<ModelRequest> {
    let config = RuntimeConfig::from(config.clone());
    let input =
        build_provider_model_input(state, &config, session_id, model_context.clone()).await?;
    let mut request = ModelRequest::new(input);
    if let Some(turn_id) = turn_id {
        request = request.with_turn_id(turn_id);
    }
    request.max_tokens = config.provider.max_tokens;
    Ok(request)
}

pub(crate) async fn build_provider_model_input(
    state: &AppState,
    config: &RuntimeConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<Arc<ProviderModelInput>> {
    Ok(Arc::new(
        ProviderModelInput::from_shared(
            config.provider.model.clone(),
            Arc::clone(config.prompt()),
            provider_transcript_owned(model_context),
            ProviderToolProfile::for_provider(config.provider.kind),
            provider_tools_for_session(
                state,
                config.provider.kind,
                effective_prompt_profile(state, config, session_id).await?,
            ),
            config.provider.reasoning_effort,
        )
        .with_prompt_cache_key(model_prompt_cache_key(config, session_id))
        .with_session_id(session_id),
    ))
}

pub(super) async fn complete_model_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    request: ModelRequest,
) -> Result<ModelResponse> {
    let credentials = Credentials::load();
    let mut provider = provider_for_config(state, config, &credentials, session_id).await?;
    let mut prepared = PreparedModelRequestState::default();
    Ok(complete_with_auth_retry(
        state,
        config,
        session_id,
        &mut provider,
        request,
        &mut prepared,
    )
    .await?)
}
