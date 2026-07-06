use agent_provider::{ModelRequest, ModelResponse, ProviderToolProfile};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::TurnId;
use anyhow::Result;

use crate::auth::Credentials;
use crate::state::AppState;

use super::auth_retry::complete_with_auth_retry;
use super::prompt::{assemble_agent_prompt, effective_prompt_profile, provider_tools_for_session};
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
    #[cfg(test)]
    if let Some(result) = injected_model_result(config, session_id) {
        return result;
    }
    let request = agent_perf::scope_phase(
        agent_perf::Phase::RequestPreparation,
        build_model_request(state, config, session_id, Some(turn_id), model_context),
    )
    .await?;
    complete_model_request(state, config, session_id, request).await
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

pub(crate) async fn build_model_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    turn_id: Option<TurnId>,
    model_context: ModelContext,
) -> Result<ModelRequest> {
    agent_perf::logical_model_request_built();
    let prompt = assemble_agent_prompt(state, config, session_id).await?;
    Ok(ModelRequest {
        model: config.provider.model.clone(),
        transcript_cache_prefix_len: None,
        prompt,
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: provider_tools_for_session(
            state,
            config.provider.kind,
            effective_prompt_profile(state, config, session_id).await?,
        ),
        // Provider adapters apply authoritative discovered/static output
        // ceilings. Do not pre-clamp here or stale daemon metadata could
        // override a newer Models API result.
        max_tokens: config.provider.max_tokens,
        reasoning_effort: config.provider.reasoning_effort,
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
    let _preparation = agent_perf::phase(agent_perf::Phase::RequestPreparation);
    let credentials = Credentials::load();
    let provider = provider_for_config(state, config, &credentials, session_id).await?;
    drop(_preparation);
    Ok(complete_with_auth_retry(state, config, session_id, provider, request).await?)
}
