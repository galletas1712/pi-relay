use std::sync::Arc;

use agent_provider::{
    normalize_transcript_for_provider, ModelTranscriptEntry, ProviderModelInput,
    ProviderTokenCountRequest,
};
use agent_session::TranscriptStorageNode;
use agent_store::{SessionConfig, TokenUsageEstimate};
use agent_vocab::{ProviderKind, TranscriptItem};
use anyhow::Result;

use crate::state::AppState;

use super::auth_retry::count_tokens_with_auth_retry;
use super::provider::provider_for_config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ModelInputAccounting {
    tokens: usize,
}

impl ModelInputAccounting {
    pub(crate) fn overflow_fallback(limit: usize) -> Self {
        Self { tokens: limit }
    }

    pub(crate) fn tokens(self) -> usize {
        self.tokens
    }
}

pub(crate) async fn model_input_accounting_for_gate(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    context_leaf_id: Option<&str>,
    input: Arc<ProviderModelInput>,
) -> Result<ModelInputAccounting> {
    let tokens = match config.provider.kind {
        ProviderKind::Claude => {
            count_claude_model_input_tokens_remotely(state, config, session_id, input).await
        }
        ProviderKind::OpenAi => {
            estimate_codex_model_input_tokens_from_usage_anchor(
                state,
                session_id,
                context_leaf_id,
                input,
            )
            .await
        }
    }?;
    Ok(ModelInputAccounting { tokens })
}

async fn count_claude_model_input_tokens_remotely(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    input: Arc<ProviderModelInput>,
) -> Result<usize> {
    // Claude has an authoritative remote preflight backend. Count the exact
    // local tool surface sent on the next /messages call, including web
    // wrappers now that they are normal client JSON tools.
    let mut request = ProviderTokenCountRequest::new(input);
    request.max_tokens = config.provider.max_tokens;

    let credentials = state.credentials.snapshot();
    let provider = provider_for_config(state, config, &credentials, session_id).await?;
    Ok(
        count_tokens_with_auth_retry(state, config, session_id, provider, request)
            .await?
            .input_tokens,
    )
}

async fn estimate_codex_model_input_tokens_from_usage_anchor(
    state: &AppState,
    session_id: &str,
    context_leaf_id: Option<&str>,
    input: Arc<ProviderModelInput>,
) -> Result<usize> {
    // The Codex/ChatGPT backend has no usable remote count endpoint: probing
    // /responses/input_tokens returns a Cloudflare challenge instead of a
    // count. Mirror Codex CLI's practical backend: anchor on the latest
    // provider-reported usage from a completed response, estimate only local
    // transcript suffixes appended after that point, and let reactive
    // compaction/retry handle rare overflow misses.
    let usage = if let Some(context_leaf_id) = context_leaf_id {
        state
            .repo
            .latest_model_token_usage_estimate(session_id, context_leaf_id)
            .await?
    } else {
        None
    };

    estimate_codex_model_input_tokens(
        usage,
        &input,
        estimate_model_input_tokens_from_local_heuristic,
    )
}

fn estimate_codex_model_input_tokens(
    usage: Option<TokenUsageEstimate>,
    input: &ProviderModelInput,
    estimate_full_input: impl FnOnce(&ProviderModelInput) -> Result<usize>,
) -> Result<usize> {
    let Some(usage) = usage else {
        return estimate_full_input(input);
    };
    let suffix_transcript =
        provider_transcript_after_first_model_generated_item(usage.suffix_entries);
    // The usage anchor already accounts for the prompt that was sent with
    // that older model action. Normal daemon requests no longer append
    // daemon-owned dynamic context, so only estimate the local transcript
    // suffix added after the anchor.
    let suffix_tokens = agent_provider::estimate_transcript_tokens(
        &agent_provider::PromptSections::default(),
        &suffix_transcript,
    )?
    .tokens;
    Ok(usage.base_tokens.saturating_add(suffix_tokens))
}

fn provider_transcript_after_first_model_generated_item(
    entries: Vec<TranscriptStorageNode>,
) -> Vec<ModelTranscriptEntry> {
    let start = entries
        .iter()
        .position(|entry| matches!(entry.item, TranscriptItem::AssistantMessage(_)))
        .map(|index| index.saturating_add(1))
        .unwrap_or(0);
    normalize_transcript_for_provider(
        entries
            .into_iter()
            .skip(start)
            .map(|entry| ModelTranscriptEntry {
                item: entry.item,
                provider_replay: entry.provider_replay,
            })
            .collect(),
    )
}

fn estimate_model_input_tokens_from_local_heuristic(input: &ProviderModelInput) -> Result<usize> {
    Ok(agent_provider::estimate_model_input_tokens(
        input.prompt(),
        input.transcript(),
    )?)
}

#[cfg(test)]
#[path = "context_accounting_tests.rs"]
mod tests;
