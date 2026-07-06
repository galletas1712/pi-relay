use agent_provider::{ModelRequest, ModelResponse};
use agent_store::SessionConfig;
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
    pub(crate) prompt_cache_key: String,
    pub(crate) request: ModelRequest,
}

pub(crate) async fn run_model_sidecar(
    state: &AppState,
    config: &SessionConfig,
    request: ModelSidecarRequest,
) -> Result<ModelResponse> {
    let sidecar_session_id = request.sidecar_session_id;
    let mut model_request = request.request;
    model_request.set_prompt_cache_key(request.prompt_cache_key);
    // Preserve an owner session id when the caller built the sidecar from a
    // normal model request; providers use that id as part of cache routing. If
    // there is no owner session, isolate the request under the sidecar id.
    model_request.set_session_id_if_missing(sidecar_session_id.clone());
    model_request.turn_id = None;
    let result =
        async { complete_model_request(state, config, &sidecar_session_id, model_request).await }
            .await;
    state
        .provider_connections
        .remove_session(&sidecar_session_id)
        .await;
    let response = result?;
    if let Some(error) = response.refusal_error() {
        anyhow::bail!("{error}");
    }
    Ok(response)
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
