use agent_provider::{
    ModelRequest, ModelTranscriptEntry, PromptSections, ProviderCompactionRequest,
    ProviderCompactionResponse, ProviderToolProfile,
};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, ProviderReplayItem, TranscriptItem, UserMessage};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::auth::Credentials;
use crate::delegation_context::compaction_delegation_ledger;
use crate::model_metadata;
use crate::state::AppState;

use super::auth_retry::{compact_with_auth_retry, complete_with_auth_retry};
use super::prompt::{prompt_profile, provider_tools_for_session, render_pi_compaction_prompt};
use super::provider::provider_for_config;
use super::transcript::provider_transcript;

const MAX_COMPACTION_CONTEXT_ATTEMPTS: usize = 4;

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
    resolve_compaction_config(config)
}

pub(crate) fn resolve_compaction_config(config: &SessionConfig) -> CompactionConfig {
    let metadata_configured = config.metadata.pointer("/compaction/config").is_some()
        || config.metadata.get("compaction").is_some();
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

    let default_window =
        model_metadata::context_window(config.provider.kind, &config.provider.model);
    if resolved.context_window.is_none() {
        resolved.context_window = default_window;
    }
    if resolved.auto_limit_tokens.is_none() {
        resolved.auto_limit_tokens =
            model_metadata::default_auto_limit(config.provider.kind, &config.provider.model);
    }

    if !metadata_configured {
        resolved.remote_mode = match config.provider.kind {
            ProviderKind::OpenAi => RemoteCompactionMode::Always,
            ProviderKind::Claude => RemoteCompactionMode::Never,
        };
    } else if matches!(config.provider.kind, ProviderKind::OpenAi)
        && resolved.remote_mode == RemoteCompactionMode::Auto
    {
        // OpenAI/Codex provider-native compaction is the safe default: do not
        // silently hide remote parser/provider failures behind local summary
        // fallback unless the operator explicitly sets remote_mode="never".
        resolved.remote_mode = RemoteCompactionMode::Always;
    }

    if !auto_enabled_configured {
        resolved.auto_enabled =
            resolved.auto_limit_tokens.is_some() || resolved.context_window.is_some();
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

/// Lower bound on the effective auto-compaction limit. A limit below the
/// irreducible post-compaction context (system prompt + summary + the current
/// open turn ≈ a few thousand tokens) makes auto-compaction re-fire every turn
/// without ever creating headroom, since compaction can't shrink below that
/// floor. Clamp the effective limit up so compaction always drops the context
/// below it. This is far below any real window-derived limit (≥170k), so it only
/// affects misconfigured tiny overrides.
const MIN_AUTO_COMPACTION_LIMIT: usize = 8_000;

pub(crate) fn auto_limit_tokens(config: &CompactionConfig) -> Option<usize> {
    let limit = match (config.context_window, config.auto_limit_tokens) {
        (Some(window), Some(limit)) => Some(limit.min(window)),
        (Some(window), None) => Some(window.saturating_mul(85) / 100),
        (None, Some(limit)) => Some(limit),
        (None, None) => None,
    };
    limit.map(|limit| limit.max(MIN_AUTO_COMPACTION_LIMIT))
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
    let provider = provider_for_config(state, config, &credentials, session_id).await?;

    if remote_mode != RemoteCompactionMode::Never && provider.provider.supports_remote_compaction()
    {
        match run_remote_compaction_with_trimming(state, config, session_id, model_context.clone())
            .await
        {
            Ok(output) => {
                return append_delegation_ledger_to_output(state, session_id, output).await
            }
            Err(error)
                if remote_mode == RemoteCompactionMode::Auto
                    && config.provider.kind != ProviderKind::OpenAi =>
            {
                eprintln!("remote compaction failed for {session_id}; falling back to local summary: {error}");
            }
            Err(error) => return Err(error),
        }
    } else if remote_mode == RemoteCompactionMode::Always {
        return Err(anyhow!(
            "remote compaction unsupported for provider {}",
            config.provider.kind
        ));
    }

    let output = run_local_summary_compaction(state, config, session_id, model_context).await?;
    append_delegation_ledger_to_output(state, session_id, output).await
}

async fn run_remote_compaction_with_trimming(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    let base_transcript = provider_transcript(model_context);
    let mut groups = transcript_groups(base_transcript);
    let mut last_context_error = None;
    for attempt in 0..MAX_COMPACTION_CONTEXT_ATTEMPTS {
        let request =
            remote_compaction_request(state, config, session_id, entries_from_groups(&groups))
                .await?;
        let credentials = Credentials::load();
        let provider = provider_for_config(state, config, &credentials, session_id).await?;
        match compact_with_auth_retry(state, config, session_id, provider, request).await {
            Ok(result) => return Ok(remote_compaction_output(config.provider.kind, result)),
            Err(error)
                if attempt + 1 < MAX_COMPACTION_CONTEXT_ATTEMPTS
                    && error.is_context_overflow()
                    && trim_oldest_complete_group(&mut groups) =>
            {
                last_context_error = Some(error.to_string());
                eprintln!(
                    "remote compaction for {session_id} exceeded context; retrying with older transcript group trimmed"
                );
                continue;
            }
            Err(error) => return Err(anyhow::Error::from(error)),
        }
    }
    Err(anyhow!(
        "remote compaction still exceeded context limits after trimming: {}",
        last_context_error.unwrap_or_else(|| "unknown context-length error".to_string())
    ))
}

fn remote_compaction_output(
    provider: ProviderKind,
    result: ProviderCompactionResponse,
) -> CompactionOutput {
    let (summary, summary_kind) = match result.summary {
        Some(summary) if !summary.trim().is_empty() => (
            summary.trim().to_string(),
            CompactionSummaryKind::ProviderText,
        ),
        _ => (
            generic_remote_compaction_summary(provider),
            CompactionSummaryKind::Generic,
        ),
    };
    CompactionOutput {
        summary,
        summary_kind,
        provider_replay: result.provider_replay,
        remote: true,
        provider,
        usage: result
            .usage
            .and_then(|usage| serde_json::to_value(usage).ok()),
    }
}

pub(crate) async fn remote_compaction_request(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    transcript: Vec<ModelTranscriptEntry>,
) -> Result<ProviderCompactionRequest> {
    Ok(ProviderCompactionRequest {
        model: config.provider.model.clone(),
        // Compaction uses the stable prompt plus transcript/model history. Any
        // previous post-compaction delegation ledger already present in the
        // transcript is ordinary prior summary text; fresh parent state is
        // appended to the stored compaction result after the provider returns.
        prompt: PromptSections::stable(config.system_prompt.clone()),
        transcript,
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: provider_tools_for_session(state, config.provider.kind, prompt_profile(config)),
        reasoning_effort: model_metadata::normalize_reasoning_effort(
            config.provider.kind,
            &config.provider.model,
            config.provider.reasoning_effort,
        ),
        prompt_cache_key: config.provider.prompt_cache_key().map(str::to_string),
        session_id: Some(session_id.to_string()),
    })
}

pub(crate) async fn append_delegation_ledger_to_output(
    state: &AppState,
    session_id: &str,
    mut output: CompactionOutput,
) -> Result<CompactionOutput> {
    if let Some(ledger) = compaction_delegation_ledger(state, session_id).await? {
        output.summary = if output.summary.trim().is_empty() {
            ledger
        } else {
            format!("{}\n\n{}", output.summary.trim_end(), ledger)
        };
    }
    Ok(output)
}

async fn run_local_summary_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    let base_transcript = provider_transcript(model_context);
    let mut groups = transcript_groups(base_transcript);
    let mut last_context_error = None;
    for attempt in 0..MAX_COMPACTION_CONTEXT_ATTEMPTS {
        let compaction_session_id = format!("{session_id}:compaction");
        let request = local_summary_request(
            state,
            config,
            session_id,
            &compaction_session_id,
            entries_from_groups(&groups),
        )
        .await?;
        let credentials = Credentials::load();
        let provider =
            provider_for_config(state, config, &credentials, &compaction_session_id).await?;
        let response = match complete_with_auth_retry(
            state,
            config,
            &compaction_session_id,
            provider,
            request,
        )
        .await
        {
            Ok(response) => response,
            Err(error)
                if attempt + 1 < MAX_COMPACTION_CONTEXT_ATTEMPTS
                    && error.is_context_overflow()
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

pub(crate) async fn local_summary_request(
    state: &AppState,
    config: &SessionConfig,
    _session_id: &str,
    compaction_session_id: &str,
    transcript: Vec<ModelTranscriptEntry>,
) -> Result<ModelRequest> {
    let mut transcript = transcript;
    let compaction_request = render_pi_compaction_prompt(state, config)?;
    transcript.push(TranscriptItem::UserMessage(UserMessage::text(compaction_request)).into());
    Ok(ModelRequest {
        model: config.provider.model.clone(),
        transcript_cache_prefix_len: None,
        prompt: PromptSections::stable(config.system_prompt.clone()),
        transcript,
        tool_profile: ProviderToolProfile::None,
        tools: Vec::new(),
        max_tokens: config.provider.max_tokens,
        reasoning_effort: model_metadata::normalize_reasoning_effort(
            config.provider.kind,
            &config.provider.model,
            config.provider.reasoning_effort,
        ),
        prompt_cache_key: config
            .provider
            .prompt_cache_key()
            .map(|key| format!("{key}:compaction")),
        // Local summary compaction uses an isolated compaction session id and
        // prompt-cache key. Remote OpenAI compaction intentionally does not.
        session_id: Some(compaction_session_id.to_string()),
        turn_id: None,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_config(kind: ProviderKind, model: &str, metadata: Value) -> SessionConfig {
        SessionConfig {
            project_id: None,
            outer_cwd: "/tmp".to_string(),
            workspaces: Vec::new(),
            system_prompt: "test prompt".to_string(),
            provider: agent_vocab::ProviderConfig {
                kind,
                model: model.to_string(),
                reasoning_effort: agent_vocab::ReasoningEffort::Medium,
                max_tokens: None,
                prompt_cache: None,
            },
            metadata,
        }
    }

    #[test]
    fn resolved_compaction_config_uses_known_rust_defaults() {
        let config = test_config(
            ProviderKind::OpenAi,
            "gpt-5.1-codex-max",
            serde_json::json!({}),
        );
        let resolved = resolve_compaction_config(&config);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(272_000));
        assert_eq!(resolved.auto_limit_tokens, Some(231_200));
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Always);
    }

    #[test]
    fn auto_limit_tokens_is_floored_to_prevent_compaction_churn() {
        // A misconfigured tiny override clamps up to the churn floor, so
        // auto-compaction can't re-fire every turn without creating headroom.
        let tiny = CompactionConfig {
            context_window: Some(272_000),
            auto_limit_tokens: Some(3_700),
            ..Default::default()
        };
        assert_eq!(auto_limit_tokens(&tiny), Some(MIN_AUTO_COMPACTION_LIMIT));
        // A realistic explicit limit passes through unchanged.
        let realistic = CompactionConfig {
            context_window: Some(272_000),
            auto_limit_tokens: Some(231_200),
            ..Default::default()
        };
        assert_eq!(auto_limit_tokens(&realistic), Some(231_200));
        // The window-derived default (85%) is far above the floor.
        let windowed = CompactionConfig {
            context_window: Some(272_000),
            auto_limit_tokens: None,
            ..Default::default()
        };
        assert_eq!(auto_limit_tokens(&windowed), Some(231_200));
        // No window and no override means no automatic limit at all.
        let unbounded = CompactionConfig {
            context_window: None,
            auto_limit_tokens: None,
            ..Default::default()
        };
        assert_eq!(auto_limit_tokens(&unbounded), None);
    }

    #[test]
    fn resolved_compaction_config_unknown_model_needs_explicit_limit() {
        let config = test_config(ProviderKind::OpenAi, "unknown", serde_json::json!({}));
        let resolved = resolve_compaction_config(&config);

        assert!(!resolved.auto_enabled);
        assert_eq!(resolved.context_window, None);
        assert_eq!(resolved.auto_limit_tokens, None);
    }

    #[test]
    fn resolved_compaction_config_respects_explicit_overrides() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-4-5",
            serde_json::json!({
                "compaction": {
                    "config": {
                        "auto_enabled": true,
                        "context_window": 123,
                        "auto_limit_tokens": 77,
                        "remote_mode": "auto"
                    }
                }
            }),
        );
        let resolved = resolve_compaction_config(&config);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(123));
        assert_eq!(resolved.auto_limit_tokens, Some(77));
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Auto);
    }

    #[test]
    fn resolved_compaction_config_respects_explicit_auto_disabled() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-4-5",
            serde_json::json!({ "compaction": { "config": { "auto_enabled": false } } }),
        );
        let resolved = resolve_compaction_config(&config);

        assert!(!resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(200_000));
        assert_eq!(resolved.auto_limit_tokens, Some(170_000));
    }
}
