use agent_provider::anthropic::AnthropicProvider;
use agent_provider::openai::OpenAiProvider;
use agent_provider::{
    ModelProvider, ModelRequest, ModelResponse, ModelTranscriptEntry, PromptSections,
    ProviderCompactionRequest, ProviderCompactionResponse, ProviderError,
    ProviderTokenCountRequest, ProviderToolProfile,
};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_tools::limit_tool_output;
use agent_vocab::{ProviderKind, ProviderReplayItem, TranscriptItem, UserMessage};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::{refresh_codex_credentials, Credentials};
use crate::state::AppState;

pub(crate) async fn run_model(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<ModelResponse> {
    let request = ModelRequest {
        model: config.provider.model.clone(),
        prompt: PromptSections::new(
            state.repo.global_system_prompt().await?,
            Some(dynamic_prompt_context(state, config)),
        ),
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: state.tools.definitions_for_provider(config.provider.kind),
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
    Ok(complete_with_auth_retry(config, provider, request).await?)
}

async fn count_tokens_with_auth_retry(
    config: &SessionConfig,
    provider: ProviderHandle,
    request: ProviderTokenCountRequest,
) -> std::result::Result<agent_provider::ProviderTokenCountResponse, ProviderError> {
    match provider.provider.count_tokens(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if provider.uses_codex_auth && provider_error_status(&error) == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(config, &credentials)
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            provider.provider.count_tokens(request).await
        }
        Err(error) => Err(error),
    }
}

pub(crate) async fn count_model_input_tokens(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<usize> {
    let request = ProviderTokenCountRequest {
        model: config.provider.model.clone(),
        prompt: PromptSections::new(
            state.repo.global_system_prompt().await?,
            Some(dynamic_prompt_context(state, config)),
        ),
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: state.tools.definitions_for_provider(config.provider.kind),
        reasoning_effort: config.provider.reasoning_effort,
        session_id: Some(session_id.to_string()),
    };

    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;
    Ok(count_tokens_with_auth_retry(config, provider, request)
        .await?
        .input_tokens)
}

async fn complete_with_auth_retry(
    config: &SessionConfig,
    provider: ProviderHandle,
    request: ModelRequest,
) -> std::result::Result<ModelResponse, ProviderError> {
    match provider.provider.complete(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if provider.uses_codex_auth && provider_error_status(&error) == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(config, &credentials)
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            provider.provider.complete(request).await
        }
        Err(error) => Err(error),
    }
}

async fn compact_with_auth_retry(
    config: &SessionConfig,
    provider: ProviderHandle,
    request: ProviderCompactionRequest,
) -> std::result::Result<ProviderCompactionResponse, ProviderError> {
    match provider.provider.compact(request.clone()).await {
        Ok(response) => Ok(response),
        Err(error) if provider.uses_codex_auth && provider_error_status(&error) == Some(401) => {
            let credentials = refresh_codex_credentials()
                .await
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            let provider = provider_for_config(config, &credentials)
                .map_err(|error| ProviderError::Provider(error.to_string()))?;
            provider.provider.compact(request).await
        }
        Err(error) => Err(error),
    }
}

fn generic_remote_compaction_summary(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::OpenAi => {
            "Conversation history before this point was compacted using OpenAI provider-native compaction.".to_string()
        }
        ProviderKind::Claude => {
            "Conversation history before this point was compacted using provider-native compaction.".to_string()
        }
    }
}

const COMPACTION_SYSTEM_PROMPT: &str = "\
Summarize the conversation transcript for future continuation. Preserve concrete
files, commands, constraints, decisions, unresolved work, and user preferences.
Do not mention that you are summarizing unless it is useful context.";

const COMPACTION_USER_PROMPT: &str = "\
Summarize the transcript above into a compact continuation context.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CompactionSummaryKind {
    ProviderText,
    Generic,
}

impl CompactionSummaryKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::ProviderText => "provider_text",
            Self::Generic => "generic",
        }
    }
}

pub(crate) struct CompactionOutput {
    pub summary: String,
    pub summary_kind: CompactionSummaryKind,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub remote: bool,
    pub provider: ProviderKind,
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RemoteCompactionMode {
    Auto,
    Always,
    Never,
}

fn default_remote_compaction_mode() -> RemoteCompactionMode {
    RemoteCompactionMode::Auto
}

fn default_auto_compaction_enabled() -> bool {
    false
}

fn default_auto_compaction_max_failures() -> usize {
    3
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactionConfig {
    #[serde(default = "default_remote_compaction_mode")]
    pub remote_mode: RemoteCompactionMode,
    #[serde(default = "default_auto_compaction_enabled")]
    pub auto_enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_limit_tokens: Option<usize>,
    #[serde(default = "default_auto_compaction_max_failures")]
    pub max_consecutive_failures: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            remote_mode: default_remote_compaction_mode(),
            auto_enabled: default_auto_compaction_enabled(),
            context_window: None,
            auto_limit_tokens: None,
            max_consecutive_failures: default_auto_compaction_max_failures(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct CompactionAutoState {
    #[serde(default)]
    pub consecutive_failures: usize,
    #[serde(default)]
    pub suppressed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_failure_leaf_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_success_root_id: Option<String>,
}

pub(crate) fn compaction_config(config: &SessionConfig) -> CompactionConfig {
    let auto_enabled_configured = config
        .metadata
        .pointer("/compaction/config/auto_enabled")
        .is_some()
        || config
            .metadata
            .get("compaction")
            .and_then(|value| value.get("auto_enabled"))
            .is_some();
    let mut resolved: CompactionConfig = config
        .metadata
        .pointer("/compaction/config")
        .cloned()
        .or_else(|| config.metadata.get("compaction").cloned())
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default();
    if !auto_enabled_configured
        && (resolved.context_window.is_some() || resolved.auto_limit_tokens.is_some())
    {
        resolved.auto_enabled = true;
    }
    resolved
}

pub(crate) fn compaction_auto_state(config: &SessionConfig) -> CompactionAutoState {
    config
        .metadata
        .pointer("/compaction/auto_state")
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

pub(crate) fn auto_limit_tokens(config: &CompactionConfig) -> Option<usize> {
    match (config.context_window, config.auto_limit_tokens) {
        (Some(window), Some(limit)) => Some(limit.min(window.saturating_mul(9) / 10)),
        (Some(window), None) => Some(window.saturating_mul(9) / 10),
        (None, Some(limit)) => Some(limit),
        (None, None) => None,
    }
}

pub(crate) async fn run_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    let compaction_config = compaction_config(config);
    let remote_mode = compaction_config.remote_mode;
    if remote_mode == RemoteCompactionMode::Always && config.provider.kind != ProviderKind::OpenAi {
        return Err(anyhow!(
            "remote compaction unsupported for provider {}",
            config.provider.kind
        ));
    }
    let credentials = Credentials::load();
    let provider = provider_for_config(config, &credentials)?;

    if remote_mode != RemoteCompactionMode::Never && provider.provider.supports_remote_compaction()
    {
        let request =
            remote_compaction_request(state, config, session_id, model_context.clone()).await?;
        match compact_with_auth_retry(config, provider, request).await {
            Ok(result) => {
                let (summary, summary_kind) = match result.summary {
                    Some(summary) if !summary.trim().is_empty() => (
                        summary.trim().to_string(),
                        CompactionSummaryKind::ProviderText,
                    ),
                    _ => (
                        generic_remote_compaction_summary(config.provider.kind),
                        CompactionSummaryKind::Generic,
                    ),
                };
                return Ok(CompactionOutput {
                    summary,
                    summary_kind,
                    provider_replay: result.provider_replay,
                    remote: true,
                    provider: config.provider.kind,
                    usage: result
                        .usage
                        .and_then(|usage| serde_json::to_value(usage).ok()),
                });
            }
            Err(error) if remote_mode == RemoteCompactionMode::Auto => {
                eprintln!("remote compaction failed for {session_id}; falling back to local summary: {error}");
            }
            Err(error) => return Err(anyhow::Error::from(error)),
        }
    } else if remote_mode == RemoteCompactionMode::Always {
        return Err(anyhow!(
            "remote compaction unsupported for provider {}",
            config.provider.kind
        ));
    }

    run_local_summary_compaction(state, config, session_id, model_context).await
}

async fn remote_compaction_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<ProviderCompactionRequest> {
    Ok(ProviderCompactionRequest {
        model: config.provider.model.clone(),
        instructions: state.repo.global_system_prompt().await?,
        transcript: provider_transcript(model_context),
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: state.tools.definitions_for_provider(config.provider.kind),
        reasoning_effort: config.provider.reasoning_effort,
        session_id: Some(session_id.to_string()),
    })
}

async fn run_local_summary_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    const MAX_LOCAL_COMPACTION_ATTEMPTS: usize = 4;
    let base_transcript = provider_transcript(model_context);
    let mut groups = transcript_groups(base_transcript);
    let mut last_context_error = None;
    for attempt in 0..MAX_LOCAL_COMPACTION_ATTEMPTS {
        let request =
            local_summary_request(state, config, session_id, entries_from_groups(&groups));
        let credentials = Credentials::load();
        let provider = provider_for_config(config, &credentials)?;
        let response = match complete_with_auth_retry(config, provider, request).await {
            Ok(response) => response,
            Err(error)
                if attempt + 1 < MAX_LOCAL_COMPACTION_ATTEMPTS
                    && provider_error_is_context_too_large(&error)
                    && trim_oldest_complete_group(&mut groups) =>
            {
                last_context_error = Some(error.to_string());
                continue;
            }
            Err(error) => return Err(anyhow::Error::from(error)),
        };
        let summary = response.assistant.text().trim().to_string();
        if summary.is_empty() {
            return Err(anyhow!("compaction provider returned an empty summary"));
        }
        return Ok(CompactionOutput {
            summary,
            summary_kind: CompactionSummaryKind::ProviderText,
            provider_replay: Vec::new(),
            remote: false,
            provider: config.provider.kind,
            usage: response
                .usage
                .and_then(|usage| serde_json::to_value(usage).ok()),
        });
    }
    Err(anyhow!(
        "local summary compaction still exceeded context limits after trimming: {}",
        last_context_error.unwrap_or_else(|| "unknown context-length error".to_string())
    ))
}

fn local_summary_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    mut transcript: Vec<ModelTranscriptEntry>,
) -> ModelRequest {
    transcript.push(TranscriptItem::UserMessage(UserMessage::text(COMPACTION_USER_PROMPT)).into());
    let dynamic_context = (config.provider.kind == ProviderKind::Claude)
        .then(|| dynamic_prompt_context(state, config));
    ModelRequest {
        model: config.provider.model.clone(),
        prompt: PromptSections::new(Some(COMPACTION_SYSTEM_PROMPT.to_string()), dynamic_context),
        transcript,
        tool_profile: ProviderToolProfile::None,
        tools: Vec::new(),
        max_tokens: config.provider.max_tokens,
        reasoning_effort: config.provider.reasoning_effort,
        prompt_cache_key: config
            .provider
            .prompt_cache
            .as_ref()
            .and_then(|value| value.get("key"))
            .and_then(Value::as_str)
            .map(|key| format!("{key}:compaction")),
        // Local summary compaction uses an isolated compaction session id and
        // prompt-cache key. Remote OpenAI compaction intentionally does not.
        session_id: Some(format!("{session_id}:compaction")),
    }
}

#[derive(Debug, Clone)]
enum TranscriptGroup {
    CompactionRoot(ModelTranscriptEntry),
    Turn {
        entries: Vec<ModelTranscriptEntry>,
        complete: bool,
    },
    Other(Vec<ModelTranscriptEntry>),
}

fn transcript_groups(entries: Vec<ModelTranscriptEntry>) -> Vec<TranscriptGroup> {
    let mut groups = Vec::new();
    let mut current_turn: Option<Vec<ModelTranscriptEntry>> = None;
    for entry in entries {
        match &entry.item {
            TranscriptItem::CompactionSummary(_) => {
                if let Some(entries) = current_turn.take() {
                    groups.push(TranscriptGroup::Turn {
                        entries,
                        complete: false,
                    });
                }
                groups.push(TranscriptGroup::CompactionRoot(entry));
            }
            TranscriptItem::TurnStarted { .. } => {
                if let Some(entries) = current_turn.take() {
                    groups.push(TranscriptGroup::Turn {
                        entries,
                        complete: false,
                    });
                }
                current_turn = Some(vec![entry]);
            }
            TranscriptItem::TurnFinished { .. } => {
                if let Some(mut entries) = current_turn.take() {
                    entries.push(entry);
                    groups.push(TranscriptGroup::Turn {
                        entries,
                        complete: true,
                    });
                } else {
                    groups.push(TranscriptGroup::Other(vec![entry]));
                }
            }
            _ => {
                if let Some(entries) = current_turn.as_mut() {
                    entries.push(entry);
                } else {
                    groups.push(TranscriptGroup::Other(vec![entry]));
                }
            }
        }
    }
    if let Some(entries) = current_turn {
        groups.push(TranscriptGroup::Turn {
            entries,
            complete: false,
        });
    }
    groups
}

fn entries_from_groups(groups: &[TranscriptGroup]) -> Vec<ModelTranscriptEntry> {
    groups
        .iter()
        .flat_map(|group| match group {
            TranscriptGroup::CompactionRoot(entry) => vec![entry.clone()],
            TranscriptGroup::Turn { entries, .. } | TranscriptGroup::Other(entries) => {
                entries.clone()
            }
        })
        .collect()
}

fn trim_oldest_complete_group(groups: &mut Vec<TranscriptGroup>) -> bool {
    let start = groups
        .iter()
        .rposition(|group| matches!(group, TranscriptGroup::CompactionRoot(_)))
        .map(|index| index + 1)
        .unwrap_or(0);
    let droppable = groups
        .iter()
        .enumerate()
        .skip(start)
        .filter(|(_, group)| {
            matches!(
                group,
                TranscriptGroup::Turn { complete: true, .. } | TranscriptGroup::Other(_)
            )
        })
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    if droppable.len() <= 1 {
        return false;
    }
    groups.remove(droppable[0]);
    true
}

fn provider_error_is_context_too_large(error: &ProviderError) -> bool {
    let status = provider_error_status(error);
    let message = match error {
        ProviderError::Status { message, .. } | ProviderError::Provider(message) => message.clone(),
        ProviderError::Http(error) => error.to_string(),
        ProviderError::Json(_) => return false,
    };
    let lower = message.to_ascii_lowercase();
    matches!(status, Some(400 | 413))
        || (lower.contains("context")
            && (lower.contains("length")
                || lower.contains("too large")
                || lower.contains("exceed")
                || lower.contains("maximum")))
}

pub(crate) fn dynamic_prompt_context_for_cwd(cwd: &std::path::Path) -> String {
    format!(
        "Starting working directory for this session: {}\n\
         The bash tool runs each command in a fresh shell rooted here; chain commands with `&&` \
         (or call `cd` inside the command) when you need to scope work to a subdirectory.",
        cwd.display()
    )
}

fn dynamic_prompt_context(_state: &AppState, config: &SessionConfig) -> String {
    let cwd = std::path::Path::new(&config.starting_cwd);
    dynamic_prompt_context_for_cwd(cwd)
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

struct ProviderHandle {
    provider: Box<dyn ModelProvider>,
    uses_codex_auth: bool,
}

fn provider_for_config(
    config: &SessionConfig,
    credentials: &Credentials,
) -> Result<ProviderHandle> {
    let handle = match config.provider.kind {
        ProviderKind::OpenAi => ProviderHandle {
            provider: Box::new(OpenAiProvider::codex(
                credentials.codex_access_token.clone().ok_or_else(|| {
                    anyhow!("~/.codex ChatGPT token not found for OpenAI subscription transport")
                })?,
                credentials.codex_account_id.clone(),
                credentials.codex_installation_id.clone(),
            )),
            uses_codex_auth: true,
        },
        ProviderKind::Claude => ProviderHandle {
            provider: Box::new(AnthropicProvider::new(
                credentials.anthropic_api_key.clone().ok_or_else(|| {
                    anyhow!("ANTHROPIC_API_KEY not found in env or Claude Code keychain")
                })?,
            )),
            uses_codex_auth: false,
        },
    };
    Ok(handle)
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

    #[test]
    fn dynamic_prompt_labels_cwd_as_session_starting_point() {
        let prompt = dynamic_prompt_context_for_cwd(std::path::Path::new("/tmp/project"));

        assert!(prompt.contains("Starting working directory for this session: /tmp/project"));
        assert!(prompt.contains("fresh shell rooted here"));
    }
}
