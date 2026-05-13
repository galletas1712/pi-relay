use agent_provider::anthropic::AnthropicProvider;
use agent_provider::openai::OpenAiProvider;
use agent_provider::{
    ModelProvider, ModelRequest, ModelResponse, ModelTranscriptEntry, PromptSections, ProviderError,
};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_tools::limit_tool_output;
use agent_vocab::{ProviderKind, TranscriptItem, UserMessage};
use anyhow::{anyhow, Result};
use serde_json::Value;

use crate::auth::{refresh_codex_credentials, Credentials};
use crate::state::AppState;

pub(crate) async fn run_model(
    state: &AppState,
    config: &SessionConfig,
    model_context: ModelContext,
) -> Result<ModelResponse> {
    let request = ModelRequest {
        model: config.provider.model.clone(),
        prompt: PromptSections::new(
            state.repo.global_system_prompt().await?,
            Some(dynamic_prompt_context(state)),
        ),
        transcript: provider_transcript(model_context),
        tools: state.tools.definitions(),
        max_tokens: config.provider.max_tokens,
        prompt_cache_key: config
            .provider
            .prompt_cache
            .as_ref()
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            .map(str::to_string),
    };

    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;
    match provider.complete(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error)
            if config.provider.kind.is_codex() && provider_error_status(&error) == Some(401) =>
        {
            let credentials = refresh_codex_credentials().await?;
            let provider = provider_for_config(config, &credentials)?;
            Ok(provider.complete(request).await?)
        }
        Err(error) => Err(anyhow::Error::from(error)),
    }
}

const COMPACTION_SYSTEM_PROMPT: &str = "\
Summarize the conversation transcript for future continuation. Preserve concrete
files, commands, constraints, decisions, unresolved work, and user preferences.
Do not mention that you are summarizing unless it is useful context.";

const COMPACTION_USER_PROMPT: &str = "\
Summarize the transcript above into a compact continuation context.";

pub(crate) async fn run_compaction(
    config: &SessionConfig,
    model_context: ModelContext,
) -> Result<String> {
    let mut transcript = provider_transcript(model_context);
    transcript.push(TranscriptItem::UserMessage(UserMessage::text(COMPACTION_USER_PROMPT)).into());
    let request = ModelRequest {
        model: config.provider.model.clone(),
        prompt: PromptSections::new(Some(COMPACTION_SYSTEM_PROMPT.to_string()), None),
        transcript,
        tools: Vec::new(),
        max_tokens: config.provider.max_tokens,
        prompt_cache_key: config
            .provider
            .prompt_cache
            .as_ref()
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            .map(|key| format!("{key}:compaction")),
    };

    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;
    let assistant = match provider.complete(request.clone()).await {
        Ok(response) => response.assistant,
        Err(error)
            if config.provider.kind.is_codex() && provider_error_status(&error) == Some(401) =>
        {
            let credentials = refresh_codex_credentials().await?;
            let provider = provider_for_config(config, &credentials)?;
            provider.complete(request).await?.assistant
        }
        Err(error) => return Err(anyhow::Error::from(error)),
    };
    let summary = assistant.text().trim().to_string();
    if summary.is_empty() {
        return Err(anyhow!("compaction provider returned an empty summary"));
    }
    Ok(summary)
}

fn dynamic_prompt_context(state: &AppState) -> String {
    format!(
        "Current working directory: {}",
        state.tool_context.cwd.display()
    )
}

fn provider_transcript(model_context: ModelContext) -> Vec<ModelTranscriptEntry> {
    model_context
        .into_entries()
        .into_iter()
        .map(|entry| ModelTranscriptEntry {
            item: limit_transcript_tool_output(entry.item),
            provider_replay: entry.provider_replay,
        })
        .collect()
}

fn limit_transcript_tool_output(item: TranscriptItem) -> TranscriptItem {
    match item {
        TranscriptItem::ToolResult(mut result) => {
            result.output = limit_tool_output(result.output);
            TranscriptItem::ToolResult(result)
        }
        item => item,
    }
}

fn provider_for_config(
    config: &SessionConfig,
    credentials: &Credentials,
) -> Result<Box<dyn ModelProvider>> {
    let provider: Box<dyn ModelProvider> =
        match config.provider.kind {
            ProviderKind::OpenAi => Box::new(OpenAiProvider::new(
                credentials.openai_api_key.clone().ok_or_else(|| {
                    anyhow!("OPENAI_API_KEY not found in env or ~/.codex/auth.json")
                })?,
            )),
            ProviderKind::Codex => Box::new(OpenAiProvider::codex(
                credentials.codex_access_token.clone().ok_or_else(|| {
                    anyhow!("CODEX_ACCESS_TOKEN or ~/.codex ChatGPT token not found")
                })?,
                credentials.codex_account_id.clone(),
            )),
            ProviderKind::Claude => Box::new(AnthropicProvider::new(
                credentials.anthropic_api_key.clone().ok_or_else(|| {
                    anyhow!("ANTHROPIC_API_KEY not found in env or Claude Code keychain")
                })?,
            )),
        };
    Ok(provider)
}

fn provider_error_status(error: &ProviderError) -> Option<u16> {
    match error {
        ProviderError::Status { status, .. } => Some(*status),
        ProviderError::Http(error) => error.status().map(|status| status.as_u16()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCallId, ToolResultMessage};

    #[test]
    fn provider_transcript_bounds_historical_tool_results() {
        let model_context = ModelContext::from_transcript_items(vec![TranscriptItem::ToolResult(
            ToolResultMessage::success(ToolCallId::from_u64(1), "bash", "x".repeat(30_000)),
        )]);

        let transcript = provider_transcript(model_context);
        let TranscriptItem::ToolResult(result) = &transcript[0].item else {
            panic!("expected tool result");
        };

        assert!(result.output.len() < 30_000);
        assert!(result.output.contains("[tool output truncated:"));
    }
}
