use agent_provider::{
    ModelRequest, ModelResponse, ModelTranscriptEntry, PromptSections, ProviderToolProfile,
};
use agent_store::SessionConfig;
use agent_tools::ProviderTool;
use agent_vocab::ReasoningEffort;
use anyhow::Result;

use crate::state::AppState;

use super::requests::complete_model_request;

const SIDECAR_ID_PREFIX_CHARS: usize = 16;
const SIDECAR_ID_OWNER_CHARS: usize = 24;

/// A non-persistent provider invocation for auxiliary model work.
///
/// Sidecar requests reuse the same provider configuration, credential refresh,
/// connection registry, and provider adapter path as normal model calls, but do
/// not create session actions, transcript entries, events, or provider replay
/// rows. Callers are responsible for deciding whether the sidecar response
/// should cause a separate durable mutation.
pub(crate) struct ModelSidecarRequest {
    pub(crate) sidecar_session_id: String,
    pub(crate) prompt: PromptSections,
    pub(crate) transcript: Vec<ModelTranscriptEntry>,
    pub(crate) tool_profile: ProviderToolProfile,
    pub(crate) tools: Vec<ProviderTool>,
    pub(crate) max_tokens: Option<u32>,
    pub(crate) reasoning_effort: ReasoningEffort,
}

pub(crate) async fn run_model_sidecar(
    state: &AppState,
    config: &SessionConfig,
    request: ModelSidecarRequest,
) -> Result<ModelResponse> {
    let sidecar_session_id = request.sidecar_session_id;
    let result = async {
        let model_request = ModelRequest {
            model: config.provider.model.clone(),
            prompt: request.prompt,
            transcript: request.transcript,
            tool_profile: request.tool_profile,
            tools: request.tools,
            max_tokens: request.max_tokens,
            reasoning_effort: request.reasoning_effort,
            prompt_cache_key: Some(sidecar_session_id.clone()),
            session_id: Some(sidecar_session_id.clone()),
            turn_id: None,
        };
        complete_model_request(state, config, &sidecar_session_id, model_request).await
    }
    .await;
    state
        .provider_connections
        .remove_session(&sidecar_session_id)
        .await;
    result
}

pub(crate) fn sidecar_session_id(prefix: &str, owner_session_id: &str, parts: &[&str]) -> String {
    let clean_prefix = clean_sidecar_segment(prefix, SIDECAR_ID_PREFIX_CHARS)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "sidecar".to_string());
    let clean_owner = clean_sidecar_segment(owner_session_id, SIDECAR_ID_OWNER_CHARS)
        .filter(|value| !value.is_empty());
    let hash = sidecar_hash(prefix, owner_session_id, parts);
    match clean_owner {
        Some(clean_owner) => format!("{clean_prefix}-{clean_owner}-{hash:016x}"),
        None => format!("{clean_prefix}-{hash:016x}"),
    }
}

fn clean_sidecar_segment(value: &str, max_chars: usize) -> Option<String> {
    let segment = value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_'))
        .take(max_chars)
        .collect::<String>();
    (!segment.is_empty()).then_some(segment)
}

fn sidecar_hash(prefix: &str, owner_session_id: &str, parts: &[&str]) -> u64 {
    let mut hash = 0xcbf29ce484222325;
    hash = fnv1a_update(hash, prefix.as_bytes());
    hash = fnv1a_update(hash, &[0]);
    hash = fnv1a_update(hash, owner_session_id.as_bytes());
    for part in parts {
        hash = fnv1a_update(hash, &[0]);
        hash = fnv1a_update(hash, part.as_bytes());
    }
    hash
}

fn fnv1a_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

#[cfg(test)]
#[path = "sidecar_tests.rs"]
mod tests;
