use agent_provider::{ProviderTokenCountRequest, ProviderToolProfile};
use agent_session::{ModelContext, ModelContextEntry, TranscriptStorageNode};
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, TranscriptItem};
use anyhow::Result;
use serde_json::Value;

use crate::auth::Credentials;
use crate::state::AppState;

use super::auth_retry::count_tokens_with_auth_retry;
use super::prompt::assemble_agent_prompt;
use super::provider::provider_for_config;
use super::transcript::provider_transcript;

pub(crate) async fn model_input_tokens_for_gate(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    context_leaf_id: Option<&str>,
    model_context: ModelContext,
) -> Result<usize> {
    match config.provider.kind {
        ProviderKind::Claude => {
            count_claude_model_input_tokens_remotely(state, config, session_id, model_context).await
        }
        ProviderKind::OpenAi => {
            estimate_codex_model_input_tokens_from_usage_anchor(
                state,
                config,
                session_id,
                context_leaf_id,
                model_context,
            )
            .await
        }
    }
}

async fn count_claude_model_input_tokens_remotely(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<usize> {
    // Claude has an authoritative remote preflight backend. Count the exact
    // local tool surface sent on the next /messages call, including web
    // wrappers now that they are normal client JSON tools.
    let prompt = assemble_agent_prompt(state, config).await?;
    let request = ProviderTokenCountRequest {
        model: config.provider.model.clone(),
        prompt,
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: state
            .tools
            .provider_tools_for_provider(config.provider.kind),
        max_tokens: config.provider.max_tokens,
        reasoning_effort: config.provider.reasoning_effort,
        prompt_cache_key: config
            .provider
            .prompt_cache
            .as_ref()
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            .map(str::to_string),
        session_id: Some(session_id.to_string()),
    };

    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;
    Ok(count_tokens_with_auth_retry(config, provider, request)
        .await?
        .input_tokens)
}

async fn estimate_codex_model_input_tokens_from_usage_anchor(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    context_leaf_id: Option<&str>,
    model_context: ModelContext,
) -> Result<usize> {
    // The Codex/ChatGPT backend has no usable remote count endpoint: probing
    // /responses/input_tokens returns a Cloudflare challenge instead of a
    // count. Mirror Codex CLI's practical backend: anchor on the latest
    // provider-reported usage from a completed response, estimate only local
    // transcript suffixes appended after that point, and let reactive
    // compaction/retry handle rare overflow misses.
    if let Some(context_leaf_id) = context_leaf_id {
        if let Some(usage) = state
            .repo
            .latest_model_token_usage_estimate(session_id, context_leaf_id)
            .await?
        {
            let suffix_entries =
                suffix_after_first_model_generated_item(usage.suffix_entries.clone());
            let suffix_context = ModelContext::from_entries(
                suffix_entries
                    .into_iter()
                    .map(|entry| ModelContextEntry {
                        item: entry.item,
                        provider_replay: entry.provider_replay,
                    })
                    .collect(),
            );
            let suffix_transcript = provider_transcript(suffix_context);
            let suffix_tokens = agent_provider::estimate_transcript_tokens(
                &suffix_transcript,
                config.provider.kind,
            )
            .tokens;
            return Ok(usage
                .with_estimated_suffix_tokens(suffix_tokens)
                .total_tokens);
        }
    }

    estimate_model_input_tokens_from_local_heuristic(state, config, model_context).await
}

fn suffix_after_first_model_generated_item(
    entries: Vec<TranscriptStorageNode>,
) -> Vec<TranscriptStorageNode> {
    let start = entries
        .iter()
        .position(|entry| matches!(entry.item, TranscriptItem::AssistantMessage(_)))
        .map(|index| index.saturating_add(1))
        .unwrap_or(0);
    entries.into_iter().skip(start).collect()
}

async fn estimate_model_input_tokens_from_local_heuristic(
    state: &AppState,
    config: &SessionConfig,
    model_context: ModelContext,
) -> Result<usize> {
    let prompt = assemble_agent_prompt(state, config).await?;
    let transcript = provider_transcript(model_context);
    Ok(agent_provider::estimate_model_input_tokens(
        &prompt,
        &transcript,
        config.provider.kind,
    ))
}
