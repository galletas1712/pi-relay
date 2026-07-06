use agent_provider::{ProviderTokenCountRequest, ProviderToolProfile};
use agent_session::{ModelContext, ModelContextEntry, TranscriptStorageNode};
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, TranscriptItem};
use anyhow::Result;

use crate::auth::Credentials;
use crate::state::AppState;

use super::auth_retry::count_tokens_with_auth_retry;
use super::prompt::{assemble_agent_prompt, effective_prompt_profile, provider_tools_for_session};
use super::provider::provider_for_config;
use super::transcript::provider_transcript;

pub(crate) async fn model_input_tokens_for_gate(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    context_leaf_id: Option<&str>,
    model_context: ModelContext,
) -> Result<usize> {
    agent_perf::accounting_pass();
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
    let prompt = assemble_agent_prompt(state, config, session_id).await?;
    let request = ProviderTokenCountRequest {
        model: config.provider.model.clone(),
        prompt,
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: provider_tools_for_session(
            state,
            config.provider.kind,
            effective_prompt_profile(state, config, session_id).await?,
        ),
        max_tokens: config.provider.max_tokens,
        reasoning_effort: config.provider.reasoning_effort,
        prompt_cache_key: config.provider.prompt_cache_key().map(str::to_string),
        session_id: Some(session_id.to_string()),
    };

    let credentials = Credentials::load();
    let provider = provider_for_config(state, config, &credentials, session_id).await?;
    agent_perf::logical_count_token_request();
    Ok(
        count_tokens_with_auth_retry(state, config, session_id, provider, request)
            .await?
            .input_tokens,
    )
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
            // The usage anchor already accounts for the prompt that was sent
            // with that older model action. Normal daemon requests no longer
            // append daemon-owned dynamic context, so only estimate the local
            // transcript suffix added after the anchor.
            let suffix_tokens = agent_provider::estimate_transcript_tokens(
                &agent_provider::PromptSections::default(),
                &suffix_transcript,
            )?
            .tokens;
            return Ok(usage
                .with_estimated_suffix_tokens(suffix_tokens)
                .total_tokens);
        }
    }

    estimate_model_input_tokens_from_local_heuristic(state, config, session_id, model_context).await
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
    session_id: &str,
    model_context: ModelContext,
) -> Result<usize> {
    let prompt = assemble_agent_prompt(state, config, session_id).await?;
    let transcript = provider_transcript(model_context);
    Ok(agent_provider::estimate_model_input_tokens(
        &prompt,
        &transcript,
    )?)
}
