use agent_mcp_types::McpSessionSnapshot;
use agent_provider::ProviderToolProfile;
use agent_session::{ModelContext, ModelContextEntry, TranscriptStorageNode};
use agent_store::SessionConfig;
use agent_vocab::TranscriptItem;
use anyhow::Result;

use crate::state::AppState;

use super::mcp::provider_toolset_fingerprint;
use super::prompt::{assemble_agent_prompt, effective_prompt_profile, provider_tools_for_session};
use super::transcript::provider_transcript;

pub(crate) async fn model_input_tokens_for_gate(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    context_leaf_id: Option<&str>,
    model_context: ModelContext,
    snapshot: &McpSessionSnapshot,
) -> Result<usize> {
    estimate_model_input_tokens_from_usage_anchor(
        state,
        config,
        session_id,
        context_leaf_id,
        model_context,
        snapshot,
    )
    .await
}

async fn estimate_model_input_tokens_from_usage_anchor(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    context_leaf_id: Option<&str>,
    model_context: ModelContext,
    snapshot: &McpSessionSnapshot,
) -> Result<usize> {
    // Anchor on the latest provider-reported usage (totalTokens from the
    // ModelResponse) from a completed response, estimate only local transcript
    // suffixes appended after that point, and let reactive compaction/retry
    // handle rare overflow misses.  The sidecar reports usage.totalTokens for
    // every provider, so this works universally.
    if let Some(context_leaf_id) = context_leaf_id {
        let tools = request_tools(state, config, session_id, snapshot).await?;
        let toolset_fingerprint = provider_toolset_fingerprint(&tools);
        if let Some(usage) = state
            .repo
            .latest_model_token_usage_estimate(session_id, context_leaf_id, &toolset_fingerprint)
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

    estimate_model_input_tokens_from_local_heuristic(
        state,
        config,
        session_id,
        model_context,
        snapshot,
    )
    .await
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
    snapshot: &McpSessionSnapshot,
) -> Result<usize> {
    let prompt = assemble_agent_prompt(state, config, session_id).await?;
    let transcript = provider_transcript(model_context);
    let tools = request_tools(state, config, session_id, snapshot).await?;
    Ok(agent_provider::estimate_model_input_tokens_with_tools(
        &prompt,
        &transcript,
        &tools,
    )?)
}

async fn request_tools(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    snapshot: &McpSessionSnapshot,
) -> Result<Vec<agent_tools::ProviderTool>> {
    let mut tools = provider_tools_for_session(
        state,
        config.provider.provider.as_str(),
        effective_prompt_profile(state, config, session_id).await?,
    );
    tools.extend(snapshot.provider_tools(&config.provider.provider));
    Ok(tools)
}
