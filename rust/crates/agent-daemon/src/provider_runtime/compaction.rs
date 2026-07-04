use agent_provider::{
    ModelRequest, ModelTranscriptEntry, PromptSections, ProviderCompactionRequest,
    ProviderCompactionResponse, ProviderModelMetadata, ProviderToolProfile,
};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, ProviderReplayItem, TranscriptItem, UserMessage};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::future::Future;

use crate::auth::Credentials;
use crate::delegation_context::compaction_delegation_ledger;
use crate::model_metadata;
use crate::state::AppState;

use super::auth_retry::{compact_with_auth_retry, complete_with_auth_retry};
use super::prompt::{
    effective_prompt_profile, provider_tools_for_session, render_pi_compaction_prompt,
};
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

#[derive(Debug)]
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionConfig {
    pub remote_mode: RemoteCompactionMode,
    pub auto_enabled: bool,
    pub auto_limit_tokens: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct StoredCompactionPolicy {
    #[serde(default)]
    remote_mode: PolicyField<RemoteCompactionMode>,
    #[serde(default)]
    auto_enabled: PolicyField<bool>,
    #[serde(default)]
    context_window: PolicyField<usize>,
    #[serde(default)]
    auto_limit_tokens: PolicyField<usize>,
    #[serde(default)]
    anthropic_native_compaction: Value,
}

#[derive(Debug)]
enum ParsedCompactionPolicyState {
    Missing,
    Valid(StoredCompactionPolicy),
    Invalid,
}

#[derive(Debug)]
pub(crate) struct ParsedCompactionPolicy {
    state: ParsedCompactionPolicyState,
}

impl ParsedCompactionPolicy {
    pub(crate) fn explicitly_disables_auto(&self) -> bool {
        match &self.state {
            ParsedCompactionPolicyState::Missing => false,
            ParsedCompactionPolicyState::Valid(policy) => policy.auto_enabled.get() == Some(false),
            ParsedCompactionPolicyState::Invalid => true,
        }
    }
}

#[derive(Debug, Default)]
enum PolicyField<T> {
    #[default]
    Missing,
    Value(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PolicyField<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::Value)
    }
}

impl<T: Copy> PolicyField<T> {
    fn get(&self) -> Option<T> {
        match self {
            Self::Missing => None,
            Self::Value(value) => Some(*value),
        }
    }
}

pub(crate) fn parse_compaction_policy(config: &SessionConfig) -> ParsedCompactionPolicy {
    let selected = config
        .metadata
        .pointer("/compaction/config")
        .or_else(|| config.metadata.get("compaction"));
    let state = match selected {
        None => ParsedCompactionPolicyState::Missing,
        Some(value) => match serde_json::from_value(value.clone()) {
            Ok(policy) => ParsedCompactionPolicyState::Valid(policy),
            Err(_) => ParsedCompactionPolicyState::Invalid,
        },
    };
    ParsedCompactionPolicy { state }
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
    pub last_success_leaf_id: Option<String>,
    #[serde(default)]
    pub consecutive_recompactions: usize,
}

pub(crate) fn compaction_config(config: &SessionConfig) -> CompactionConfig {
    let policy = parse_compaction_policy(config);
    resolve_compaction_config_with_policy(config, None, &policy)
}

pub(crate) fn compaction_config_with_model_metadata(
    config: &SessionConfig,
    discovered: Option<ProviderModelMetadata>,
    policy: &ParsedCompactionPolicy,
) -> CompactionConfig {
    resolve_compaction_config_with_policy(config, discovered, policy)
}

#[cfg(test)]
fn resolve_compaction_config(
    config: &SessionConfig,
    discovered: Option<ProviderModelMetadata>,
) -> CompactionConfig {
    let policy = parse_compaction_policy(config);
    resolve_compaction_config_with_policy(config, discovered, &policy)
}

fn resolve_compaction_config_with_policy(
    config: &SessionConfig,
    discovered: Option<ProviderModelMetadata>,
    parsed: &ParsedCompactionPolicy,
) -> CompactionConfig {
    let default_policy = StoredCompactionPolicy::default();
    let policy = match &parsed.state {
        ParsedCompactionPolicyState::Missing => &default_policy,
        ParsedCompactionPolicyState::Valid(policy) => policy,
        ParsedCompactionPolicyState::Invalid => {
            return invalid_compaction_config(config.provider.kind)
        }
    };
    let claude_native_enabled = match (config.provider.kind, &policy.anthropic_native_compaction) {
        (ProviderKind::Claude, Value::Null) => false,
        (ProviderKind::Claude, Value::String(marker)) if marker == "compact_20260112" => true,
        (ProviderKind::Claude, _) => return invalid_compaction_config(ProviderKind::Claude),
        (ProviderKind::OpenAi, _) => false,
    };
    let context_window = policy.context_window.get().or_else(|| {
        discovered
            .and_then(|metadata| metadata.max_input_tokens)
            .or_else(|| {
                model_metadata::context_window(config.provider.kind, &config.provider.model)
            })
    });
    let requested_limit = policy.auto_limit_tokens.get().or_else(|| {
        context_window.map(|window| {
            model_metadata::default_auto_limit_for_window(
                config.provider.kind,
                &config.provider.model,
                window,
            )
        })
    });
    let auto_limit_tokens = effective_auto_limit(context_window, requested_limit);
    let auto_enabled = policy
        .auto_enabled
        .get()
        .unwrap_or(auto_limit_tokens.is_some())
        && (context_window.is_none() || auto_limit_tokens.is_some());
    let remote_mode = match (config.provider.kind, policy.remote_mode.get()) {
        (
            ProviderKind::Claude,
            Some(mode @ (RemoteCompactionMode::Auto | RemoteCompactionMode::Always)),
        ) if claude_native_enabled => mode,
        (ProviderKind::Claude, _) => RemoteCompactionMode::Never,
        (ProviderKind::OpenAi, Some(RemoteCompactionMode::Never)) => RemoteCompactionMode::Never,
        (ProviderKind::OpenAi, _) => RemoteCompactionMode::Always,
    };
    CompactionConfig {
        remote_mode,
        auto_enabled,
        auto_limit_tokens,
    }
}

fn invalid_compaction_config(provider: ProviderKind) -> CompactionConfig {
    CompactionConfig {
        remote_mode: match provider {
            ProviderKind::Claude => RemoteCompactionMode::Never,
            ProviderKind::OpenAi => RemoteCompactionMode::Always,
        },
        auto_enabled: false,
        auto_limit_tokens: None,
    }
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

fn effective_auto_limit(window: Option<usize>, limit: Option<usize>) -> Option<usize> {
    match (window, limit) {
        (Some(window), _) if window < MIN_AUTO_COMPACTION_LIMIT => None,
        (Some(window), Some(limit)) => Some(limit.clamp(MIN_AUTO_COMPACTION_LIMIT, window)),
        (None, Some(limit)) => Some(limit.max(MIN_AUTO_COMPACTION_LIMIT)),
        (_, None) => None,
    }
}

pub(crate) async fn run_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    let output = match compaction_config(config).remote_mode {
        RemoteCompactionMode::Never => {
            run_local_summary_compaction(state, config, session_id, model_context).await
        }
        RemoteCompactionMode::Auto | RemoteCompactionMode::Always => {
            eprintln!(
                "attempting provider-native compaction for {session_id} with {}",
                config.provider.kind
            );
            run_remote_compaction(state, config, session_id, model_context).await
        }
    }?;
    append_delegation_ledger_to_output(state, session_id, output).await
}

async fn run_remote_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    run_remote_compaction_algorithm(
        config.provider.kind,
        model_context,
        |transcript| async move {
            let request = remote_compaction_request(state, config, session_id, transcript).await?;
            let credentials = Credentials::load();
            let provider = provider_for_config(state, config, &credentials, session_id).await?;
            compact_with_auth_retry(state, config, session_id, provider, request)
                .await
                .map_err(Into::into)
        },
    )
    .await
}

async fn run_remote_compaction_algorithm<F, Fut>(
    provider: ProviderKind,
    model_context: ModelContext,
    compact: F,
) -> Result<CompactionOutput>
where
    F: FnOnce(Vec<ModelTranscriptEntry>) -> Fut,
    Fut: Future<Output = Result<ProviderCompactionResponse>>,
{
    let result = compact(provider_transcript(model_context)).await?;
    Ok(remote_compaction_output(provider, result))
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
    let compaction_instructions = if config.provider.kind == ProviderKind::Claude {
        Some(format!(
            "{}\n\nDo not call any tools while writing this summary. Respond with summary text only.",
            render_pi_compaction_prompt(state, config)?
        ))
    } else {
        None
    };
    Ok(ProviderCompactionRequest {
        model: config.provider.model.clone(),
        // Compaction uses the stable prompt plus transcript/model history. Any
        // previous post-compaction delegation ledger already present in the
        // transcript is ordinary prior summary text; fresh parent state is
        // appended to the stored compaction result after the provider returns.
        prompt: PromptSections::stable(config.system_prompt.clone()),
        transcript,
        tool_profile: ProviderToolProfile::for_provider(config.provider.kind),
        tools: provider_tools_for_session(
            state,
            config.provider.kind,
            effective_prompt_profile(state, config, session_id).await?,
        ),
        reasoning_effort: model_metadata::normalize_reasoning_effort(
            config.provider.kind,
            &config.provider.model,
            config.provider.reasoning_effort,
        ),
        prompt_cache_key: config.provider.prompt_cache_key().map(str::to_string),
        session_id: Some(session_id.to_string()),
        compaction_instructions,
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
        if let Some(error) = response.refusal_error() {
            return Err(anyhow!(error));
        }
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
    use agent_provider::{ModelProvider, ModelResponse, ProviderError, ProviderResult};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Mutex,
    };

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

    #[derive(Default)]
    struct RecordingOverflowProvider {
        compact_calls: AtomicUsize,
        complete_calls: AtomicUsize,
        compact_transcripts: Mutex<Vec<Vec<ModelTranscriptEntry>>>,
    }

    #[async_trait::async_trait]
    impl ModelProvider for RecordingOverflowProvider {
        async fn complete(&self, _request: ModelRequest) -> ProviderResult<ModelResponse> {
            self.complete_calls.fetch_add(1, Ordering::Relaxed);
            Err(ProviderError::Provider(
                "local summary compaction must not run".to_string(),
            ))
        }

        async fn compact(
            &self,
            request: ProviderCompactionRequest,
        ) -> ProviderResult<ProviderCompactionResponse> {
            self.compact_calls.fetch_add(1, Ordering::Relaxed);
            self.compact_transcripts
                .lock()
                .expect("recorded transcripts lock")
                .push(request.transcript);
            Err(ProviderError::Status {
                status: 413,
                message: "context length exceeded".to_string(),
            })
        }
    }

    #[tokio::test]
    async fn native_context_overflow_makes_one_compact_request_with_full_transcript() {
        let mut items = Vec::new();
        for (turn, text) in [
            (1, "oldest retained user instruction"),
            (2, "middle retained user instruction"),
            (3, "newest retained user instruction"),
        ] {
            let turn_id = agent_vocab::TurnId(turn);
            items.extend([
                TranscriptItem::TurnStarted { turn_id },
                TranscriptItem::UserMessage(UserMessage::text(text)),
                TranscriptItem::TurnFinished {
                    turn_id,
                    outcome: agent_vocab::TurnOutcome::Graceful,
                },
            ]);
        }
        let original_len = items.len();
        let provider = RecordingOverflowProvider::default();
        let error = run_remote_compaction_algorithm(
            ProviderKind::Claude,
            ModelContext::from_transcript_items(items),
            |transcript| async {
                let request = ProviderCompactionRequest {
                    model: "claude-opus-4-8".to_string(),
                    prompt: PromptSections::stable("test prompt"),
                    transcript,
                    tool_profile: ProviderToolProfile::AnthropicCoding,
                    tools: Vec::new(),
                    reasoning_effort: agent_vocab::ReasoningEffort::High,
                    prompt_cache_key: None,
                    session_id: Some("test-session".to_string()),
                    compaction_instructions: Some("compact".to_string()),
                };
                provider.compact(request).await.map_err(anyhow::Error::from)
            },
        )
        .await
        .expect_err("native context overflow must surface");

        assert!(error
            .downcast_ref::<ProviderError>()
            .is_some_and(ProviderError::is_context_overflow));
        assert_eq!(
            provider.compact_calls.load(Ordering::Relaxed),
            1,
            "restoring the internal native trim/retry loop would make another compact request"
        );
        assert_eq!(provider.complete_calls.load(Ordering::Relaxed), 0);
        let transcripts = provider
            .compact_transcripts
            .lock()
            .expect("recorded transcripts lock");
        assert_eq!(transcripts.len(), 1);
        assert_eq!(transcripts[0].len(), original_len);
        let sent_user_text = transcripts[0]
            .iter()
            .filter_map(|entry| match &entry.item {
                TranscriptItem::UserMessage(message) => message.as_text(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            sent_user_text,
            vec![
                "oldest retained user instruction",
                "middle retained user instruction",
                "newest retained user instruction"
            ]
        );
    }

    #[test]
    fn resolved_compaction_config_uses_known_rust_defaults() {
        let config = test_config(
            ProviderKind::OpenAi,
            "gpt-5.1-codex-max",
            serde_json::json!({}),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(231_200));
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Always);
    }

    #[test]
    fn gpt56_defaults_use_live_codex_window_and_raw_threshold() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let config = test_config(ProviderKind::OpenAi, model, serde_json::json!({}));
            let resolved = resolve_compaction_config(&config, None);

            assert!(resolved.auto_enabled);
            assert_eq!(resolved.auto_limit_tokens, Some(334_800));
        }
    }

    #[test]
    fn discovered_one_million_claude_window_defaults_to_half() {
        for model in ["claude-sonnet-5", "claude-future"] {
            let config = test_config(ProviderKind::Claude, model, serde_json::json!({}));
            let resolved = resolve_compaction_config(
                &config,
                Some(ProviderModelMetadata {
                    max_input_tokens: Some(1_000_000),
                    max_output_tokens: Some(128_000),
                }),
            );

            assert!(resolved.auto_enabled);
            assert_eq!(resolved.auto_limit_tokens, Some(500_000));
        }
    }

    #[test]
    fn claude_remote_compaction_remains_opt_in() {
        let default = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({}),
        );
        assert_eq!(
            resolve_compaction_config(&default, None).remote_mode,
            RemoteCompactionMode::Never
        );
        let limits_only = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({
                "compaction": {
                    "config": {
                        "auto_enabled": true,
                        "auto_limit_tokens": 100_000
                    }
                }
            }),
        );
        assert_eq!(
            resolve_compaction_config(&limits_only, None).remote_mode,
            RemoteCompactionMode::Never,
            "non-policy compaction settings must not enroll Claude in the beta"
        );
        for (configured, expected) in [
            ("auto", RemoteCompactionMode::Auto),
            ("always", RemoteCompactionMode::Always),
        ] {
            let explicit = test_config(
                ProviderKind::Claude,
                "claude-sonnet-5",
                serde_json::json!({
                    "compaction": {
                        "config": {
                            "remote_mode": configured,
                            "anthropic_native_compaction": "compact_20260112"
                        }
                    }
                }),
            );
            assert_eq!(
                resolve_compaction_config(&explicit, None).remote_mode,
                expected
            );
        }
        for marker in [
            serde_json::json!("compact_different_version"),
            serde_json::json!({ "malformed": true }),
        ] {
            let invalid_enrollment = test_config(
                ProviderKind::Claude,
                "claude-sonnet-5",
                serde_json::json!({
                    "compaction": {
                        "config": {
                            "remote_mode": "auto",
                            "anthropic_native_compaction": marker
                        }
                    }
                }),
            );
            assert_eq!(
                resolve_compaction_config(&invalid_enrollment, None).remote_mode,
                RemoteCompactionMode::Never
            );
        }
        let explicit_never = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({
                "compaction": {
                    "config": {
                        "remote_mode": "never",
                        "anthropic_native_compaction": "compact_20260112"
                    }
                }
            }),
        );
        assert_eq!(
            resolve_compaction_config(&explicit_never, None).remote_mode,
            RemoteCompactionMode::Never
        );
    }

    #[test]
    fn remote_compaction_output_preserves_checkpoint_replay_and_raw_usage() {
        let block = serde_json::json!({
            "type": "compaction",
            "content": "opaque Anthropic summary",
            "provider_extension": { "preserve": true }
        });
        let replay = ProviderReplayItem::new(ProviderKind::Claude, &block).unwrap();
        let raw_usage = serde_json::json!({
            "input_tokens": 0,
            "output_tokens": 0,
            "iterations": [{
                "type": "compaction",
                "input_tokens": 180000,
                "output_tokens": 3500
            }]
        });
        let output = remote_compaction_output(
            ProviderKind::Claude,
            ProviderCompactionResponse {
                summary: None,
                provider_replay: vec![replay],
                usage: Some(agent_provider::ProviderUsage {
                    input_tokens: Some(0),
                    output_tokens: Some(0),
                    total_tokens: Some(0),
                    raw_provider_usage: Some(raw_usage.clone()),
                    ..agent_provider::ProviderUsage::default()
                }),
            },
        );

        assert!(output.remote);
        assert_eq!(output.summary_kind, CompactionSummaryKind::Generic);
        assert_eq!(output.provider_replay[0].raw_value().unwrap(), block);
        let serialized_replay =
            serde_json::to_string(&output.provider_replay).expect("provider replay serializes");
        let restored_replay: Vec<ProviderReplayItem> =
            serde_json::from_str(&serialized_replay).expect("provider replay deserializes");
        assert_eq!(restored_replay[0].raw_value().unwrap(), block);
        assert_eq!(
            output
                .usage
                .as_ref()
                .and_then(|usage| usage.get("raw_provider_usage")),
            Some(&raw_usage)
        );
    }

    #[test]
    fn missing_policy_uses_provider_defaults() {
        let openai = resolve_compaction_config(
            &test_config(ProviderKind::OpenAi, "gpt-5.6-sol", serde_json::json!({})),
            None,
        );
        assert_eq!(openai.remote_mode, RemoteCompactionMode::Always);
        assert!(openai.auto_enabled);
        assert_eq!(openai.auto_limit_tokens, Some(334_800));

        let unknown = resolve_compaction_config(
            &test_config(ProviderKind::OpenAi, "unknown", serde_json::json!({})),
            None,
        );
        assert!(!unknown.auto_enabled);
        assert_eq!(unknown.auto_limit_tokens, None);
    }

    #[test]
    fn current_and_markerless_legacy_nested_policies_are_distinct() {
        let current = test_config(
            ProviderKind::Claude,
            "claude-opus-4-8",
            serde_json::json!({
                "compaction": { "config": {
                    "remote_mode": "auto",
                    "anthropic_native_compaction": "compact_20260112"
                }}
            }),
        );
        assert_eq!(
            resolve_compaction_config(&current, None).remote_mode,
            RemoteCompactionMode::Auto
        );

        let legacy = test_config(
            ProviderKind::Claude,
            "claude-opus-4-8",
            serde_json::json!({
                "title": "Legacy web session",
                "created_by": "web",
                "compaction": { "config": {
                    "auto_enabled": true,
                    "remote_mode": "auto",
                    "max_consecutive_failures": 3
                }}
            }),
        );
        assert_eq!(
            resolve_compaction_config(&legacy, None).remote_mode,
            RemoteCompactionMode::Never
        );
    }

    #[test]
    fn nested_policy_wins_as_one_whole_object() {
        let config = test_config(
            ProviderKind::OpenAi,
            "unknown",
            serde_json::json!({
                "compaction": {
                    "auto_enabled": true,
                    "auto_limit_tokens": 123_456,
                    "remote_mode": "never",
                    "config": { "auto_enabled": false }
                }
            }),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(!resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, None);
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Always);
    }

    #[test]
    fn direct_policy_layout_remains_read_compatible() {
        let disabled = resolve_compaction_config(
            &test_config(
                ProviderKind::OpenAi,
                "gpt-5.6-sol",
                serde_json::json!({ "compaction": { "auto_enabled": false } }),
            ),
            None,
        );
        assert!(!disabled.auto_enabled);

        let limited = resolve_compaction_config(
            &test_config(
                ProviderKind::Claude,
                "claude-future",
                serde_json::json!({ "compaction": { "auto_limit_tokens": 123_456 } }),
            ),
            None,
        );
        assert_eq!(limited.auto_limit_tokens, Some(123_456));

        let local_only = resolve_compaction_config(
            &test_config(
                ProviderKind::OpenAi,
                "gpt-5.6-sol",
                serde_json::json!({ "compaction": { "remote_mode": "never" } }),
            ),
            None,
        );
        assert_eq!(local_only.remote_mode, RemoteCompactionMode::Never);
    }

    #[test]
    fn malformed_selected_direct_or_nested_policy_fails_closed() {
        for metadata in [
            serde_json::json!({ "compaction": { "auto_enabled": "invalid" } }),
            serde_json::json!({
                "compaction": {
                    "auto_enabled": true,
                    "auto_limit_tokens": 123_456,
                    "config": null
                }
            }),
        ] {
            let config = test_config(ProviderKind::OpenAi, "gpt-5.6-sol", metadata);
            let policy = parse_compaction_policy(&config);
            let resolved = resolve_compaction_config_with_policy(&config, None, &policy);

            assert!(policy.explicitly_disables_auto());
            assert!(!resolved.auto_enabled);
            assert_eq!(resolved.auto_limit_tokens, None);
            assert_eq!(resolved.remote_mode, RemoteCompactionMode::Always);
        }
    }

    #[test]
    fn malformed_known_policy_fails_closed_without_changing_openai_native_baseline() {
        for malformed_policy in [
            serde_json::json!({
                "remote_mode": "auto",
                "auto_limit_tokens": "invalid",
                "anthropic_native_compaction": "compact_20260112"
            }),
            serde_json::json!({ "remote_mode": null }),
            serde_json::json!({ "auto_enabled": null }),
        ] {
            for provider in [ProviderKind::Claude, ProviderKind::OpenAi] {
                let config = test_config(
                    provider,
                    if provider == ProviderKind::Claude {
                        "claude-opus-4-8"
                    } else {
                        "gpt-5.6-sol"
                    },
                    serde_json::json!({
                        "compaction": { "config": malformed_policy }
                    }),
                );
                let resolved = resolve_compaction_config(&config, None);
                assert!(!resolved.auto_enabled);
                assert_eq!(resolved.auto_limit_tokens, None);
                assert_eq!(
                    resolved.remote_mode,
                    if provider == ProviderKind::Claude {
                        RemoteCompactionMode::Never
                    } else {
                        RemoteCompactionMode::Always
                    }
                );
                assert!(parse_compaction_policy(&config).explicitly_disables_auto());
            }
        }

        let openai_with_bad_claude_marker = test_config(
            ProviderKind::OpenAi,
            "gpt-5.6-sol",
            serde_json::json!({
                "compaction": { "config": {
                    "anthropic_native_compaction": { "ignored": true }
                }}
            }),
        );
        assert!(
            resolve_compaction_config(&openai_with_bad_claude_marker, None).auto_enabled,
            "OpenAI ignores Claude-only raw policy"
        );
    }

    #[test]
    fn store_owned_failure_limit_does_not_change_daemon_policy() {
        let config = test_config(
            ProviderKind::OpenAi,
            "gpt-5.6-sol",
            serde_json::json!({
                "compaction": { "config": {
                    "auto_enabled": true,
                    "auto_limit_tokens": 123_456,
                    "remote_mode": "never",
                    "max_consecutive_failures": "invalid"
                }}
            }),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(123_456));
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Never);
    }

    #[test]
    fn parsed_explicit_disable_is_shared_by_early_and_resolved_checks() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-4-5",
            serde_json::json!({
                "compaction": { "config": { "auto_enabled": false } }
            }),
        );
        let policy = parse_compaction_policy(&config);
        assert!(policy.explicitly_disables_auto());
        assert!(!resolve_compaction_config_with_policy(&config, None, &policy).auto_enabled);
    }

    #[test]
    fn explicit_auto_without_known_threshold_remains_reactive_only() {
        let config = test_config(
            ProviderKind::OpenAi,
            "unknown",
            serde_json::json!({
                "compaction": { "config": { "auto_enabled": true } }
            }),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, None);
    }

    #[test]
    fn valid_limits_are_effective_once_and_never_exceed_the_window() {
        for (policy, expected) in [
            (
                serde_json::json!({ "context_window": 600_000 }),
                Some(510_000),
            ),
            (
                serde_json::json!({
                    "context_window": 600_000,
                    "auto_limit_tokens": 700_000
                }),
                Some(600_000),
            ),
            (
                serde_json::json!({
                    "context_window": 600_000,
                    "auto_limit_tokens": 3_700
                }),
                Some(MIN_AUTO_COMPACTION_LIMIT),
            ),
        ] {
            let config = test_config(
                ProviderKind::Claude,
                "claude-future",
                serde_json::json!({ "compaction": { "config": policy } }),
            );
            assert_eq!(
                resolve_compaction_config(&config, None).auto_limit_tokens,
                expected
            );
        }
    }

    #[test]
    fn tiny_window_disables_automatic_compaction() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-4-5",
            serde_json::json!({
                "compaction": { "config": {
                    "auto_enabled": true,
                    "context_window": 123
                }}
            }),
        );
        let resolved = resolve_compaction_config(&config, None);
        assert!(!resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, None);
    }

    #[test]
    fn discovered_metadata_supplies_provider_aware_default() {
        let config = test_config(ProviderKind::Claude, "claude-future", serde_json::json!({}));
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(500_000),
                max_output_tokens: Some(96_000),
            }),
        );
        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(425_000));
    }

    #[test]
    fn explicit_limit_without_known_window_is_safely_floored() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-future",
            serde_json::json!({
                "compaction": { "config": { "auto_limit_tokens": 100 } }
            }),
        );
        assert_eq!(
            resolve_compaction_config(&config, None).auto_limit_tokens,
            Some(MIN_AUTO_COMPACTION_LIMIT)
        );
    }
}
