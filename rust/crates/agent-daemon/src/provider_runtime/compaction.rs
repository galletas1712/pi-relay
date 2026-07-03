use agent_provider::{
    ModelRequest, ModelTranscriptEntry, PromptSections, ProviderCompactionRequest,
    ProviderCompactionResponse, ProviderModelMetadata, ProviderToolProfile,
};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, ProviderReplayItem, TranscriptItem, UserMessage};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum AnthropicNativeCompactionVersion {
    #[serde(rename = "compact_20260112")]
    Compact20260112,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anthropic_native_compaction: Option<AnthropicNativeCompactionVersion>,
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
            anthropic_native_compaction: None,
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
    #[serde(default)]
    pub consecutive_recompactions: usize,
}

pub(crate) fn compaction_config(config: &SessionConfig) -> CompactionConfig {
    resolve_compaction_config(config, None)
}

pub(crate) fn compaction_config_with_model_metadata(
    config: &SessionConfig,
    discovered: Option<ProviderModelMetadata>,
) -> CompactionConfig {
    resolve_compaction_config(config, discovered)
}

pub(crate) fn resolve_compaction_config(
    config: &SessionConfig,
    discovered: Option<ProviderModelMetadata>,
) -> CompactionConfig {
    let remote_mode_value = config
        .metadata
        .pointer("/compaction/config/remote_mode")
        .or_else(|| config.metadata.pointer("/compaction/remote_mode"));
    let remote_mode = remote_mode_value
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let anthropic_native_compaction = config
        .metadata
        .pointer("/compaction/config/anthropic_native_compaction")
        .or_else(|| {
            config
                .metadata
                .pointer("/compaction/anthropic_native_compaction")
        })
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok());
    let auto_enabled_configured = config
        .metadata
        .pointer("/compaction/config/auto_enabled")
        .is_some()
        || config
            .metadata
            .get("compaction")
            .and_then(|value| value.get("auto_enabled"))
            .is_some();
    let context_window_configured = config
        .metadata
        .pointer("/compaction/config/context_window")
        .is_some_and(|value| !value.is_null())
        || config
            .metadata
            .pointer("/compaction/context_window")
            .is_some_and(|value| !value.is_null());
    let auto_limit_configured = config
        .metadata
        .pointer("/compaction/config/auto_limit_tokens")
        .is_some_and(|value| !value.is_null())
        || config
            .metadata
            .pointer("/compaction/auto_limit_tokens")
            .is_some_and(|value| !value.is_null());

    let config_value = config
        .metadata
        .pointer("/compaction/config")
        .cloned()
        .or_else(|| config.metadata.get("compaction").cloned());
    let parsed_config = config_value
        .map(|mut value| {
            if config.provider.kind == ProviderKind::OpenAi {
                // This Claude-only field did not exist in d296e3f. Preserve
                // OpenAI's legacy unknown-field behavior even when its value
                // is malformed, while known legacy siblings still deserialize
                // atomically as one object.
                if let Some(object) = value.as_object_mut() {
                    object.remove("anthropic_native_compaction");
                }
            }
            value
        })
        .map(serde_json::from_value::<CompactionConfig>)
        .transpose();
    let config_is_valid = parsed_config.as_ref().is_ok_and(|value| value.is_some());
    let mut resolved = parsed_config.ok().flatten().unwrap_or_default();

    let default_window = discovered
        .and_then(|metadata| metadata.max_input_tokens)
        .or_else(|| model_metadata::context_window(config.provider.kind, &config.provider.model));
    if !context_window_configured {
        resolved.context_window = default_window;
    }
    if !auto_limit_configured {
        resolved.auto_limit_tokens = resolved.context_window.map(|window| {
            model_metadata::default_auto_limit_for_window(
                config.provider.kind,
                &config.provider.model,
                window,
            )
        });
    }

    resolved.anthropic_native_compaction = match config.provider.kind {
        ProviderKind::Claude => anthropic_native_compaction,
        ProviderKind::OpenAi => None,
    };
    resolved.remote_mode = match config.provider.kind {
        ProviderKind::Claude => match remote_mode {
            // Never is independently authoritative even if another config
            // field is malformed. Auto/Always require both a wholly valid
            // config object and the versioned native-beta enrollment marker.
            Some(RemoteCompactionMode::Never) => RemoteCompactionMode::Never,
            Some(mode @ (RemoteCompactionMode::Auto | RemoteCompactionMode::Always))
                if config_is_valid
                    && anthropic_native_compaction
                        == Some(AnthropicNativeCompactionVersion::Compact20260112) =>
            {
                mode
            }
            Some(RemoteCompactionMode::Auto | RemoteCompactionMode::Always) | None => {
                RemoteCompactionMode::Never
            }
        },
        ProviderKind::OpenAi => match resolved.remote_mode {
            // Preserve the legacy whole-object serde fallback exactly:
            // malformed siblings discard the entire object to Default::Auto,
            // and Auto is then normalized to Always for OpenAI/Codex.
            RemoteCompactionMode::Never => RemoteCompactionMode::Never,
            RemoteCompactionMode::Auto | RemoteCompactionMode::Always => {
                RemoteCompactionMode::Always
            }
        },
    };

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

pub(crate) fn compaction_auto_explicitly_disabled(config: &SessionConfig) -> bool {
    config
        .metadata
        .pointer("/compaction/config/auto_enabled")
        .or_else(|| config.metadata.pointer("/compaction/auto_enabled"))
        .and_then(Value::as_bool)
        == Some(false)
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
    let credentials = Credentials::load();
    let provider = provider_for_config(state, config, &credentials, session_id).await?;
    let runner = LiveCompactionRunner {
        state,
        config,
        session_id,
        supports_remote: provider.provider.supports_remote_compaction(),
    };
    let output = run_compaction_with_runner(
        config.provider.kind,
        remote_mode,
        session_id,
        model_context,
        &runner,
    )
    .await?;
    append_delegation_ledger_to_output(state, session_id, output).await
}

#[async_trait]
trait CompactionRunner: Send + Sync {
    fn supports_remote(&self) -> bool;
    async fn run_remote(&self, model_context: ModelContext) -> Result<CompactionOutput>;
    async fn run_local(&self, model_context: ModelContext) -> Result<CompactionOutput>;
}

struct LiveCompactionRunner<'a> {
    state: &'a AppState,
    config: &'a SessionConfig,
    session_id: &'a str,
    supports_remote: bool,
}

#[async_trait]
impl CompactionRunner for LiveCompactionRunner<'_> {
    fn supports_remote(&self) -> bool {
        self.supports_remote
    }

    async fn run_remote(&self, model_context: ModelContext) -> Result<CompactionOutput> {
        run_remote_compaction_with_trimming(self.state, self.config, self.session_id, model_context)
            .await
    }

    async fn run_local(&self, model_context: ModelContext) -> Result<CompactionOutput> {
        run_local_summary_compaction(self.state, self.config, self.session_id, model_context).await
    }
}

async fn run_compaction_with_runner(
    provider: ProviderKind,
    remote_mode: RemoteCompactionMode,
    session_id: &str,
    model_context: ModelContext,
    runner: &dyn CompactionRunner,
) -> Result<CompactionOutput> {
    if remote_mode != RemoteCompactionMode::Never && runner.supports_remote() {
        eprintln!(
            "attempting provider-native compaction for {session_id} with {}",
            provider
        );
        match runner.run_remote(model_context.clone()).await {
            Ok(output) => return Ok(output),
            Err(error) if should_fallback_after_remote_failure(provider, remote_mode) => {
                eprintln!(
                    "provider-native compaction failed for {session_id}; falling back to local summary (provider={provider}, reason={error})"
                );
            }
            Err(error) => return Err(error),
        }
    } else if remote_mode == RemoteCompactionMode::Always {
        return Err(anyhow!(
            "remote compaction unsupported for provider {}",
            provider
        ));
    } else if remote_mode == RemoteCompactionMode::Auto {
        eprintln!(
            "provider-native compaction unavailable for {session_id}; falling back to local summary (provider={provider}, reason=provider does not advertise remote compaction support)"
        );
    }

    runner.run_local(model_context).await
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

fn should_fallback_after_remote_failure(
    provider: ProviderKind,
    remote_mode: RemoteCompactionMode,
) -> bool {
    remote_mode == RemoteCompactionMode::Auto && provider != ProviderKind::OpenAi
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
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
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

    struct TestCompactionRunner {
        remote_error: String,
        remote_calls: Arc<AtomicUsize>,
        local_calls: Arc<AtomicUsize>,
    }

    #[async_trait]
    impl CompactionRunner for TestCompactionRunner {
        fn supports_remote(&self) -> bool {
            true
        }

        async fn run_remote(&self, _model_context: ModelContext) -> Result<CompactionOutput> {
            self.remote_calls.fetch_add(1, Ordering::Relaxed);
            Err(anyhow::Error::new(
                agent_provider::ProviderError::native_compaction(
                    agent_provider::NativeCompactionErrorKind::Unsupported,
                    self.remote_error.clone(),
                ),
            ))
        }

        async fn run_local(&self, _model_context: ModelContext) -> Result<CompactionOutput> {
            self.local_calls.fetch_add(1, Ordering::Relaxed);
            Ok(CompactionOutput {
                summary: "local fallback".to_string(),
                summary_kind: CompactionSummaryKind::ProviderText,
                provider_replay: Vec::new(),
                remote: false,
                provider: ProviderKind::Claude,
                usage: None,
            })
        }
    }

    #[tokio::test]
    async fn run_compaction_auto_falls_back_but_always_exposes_native_error() {
        for (mode, expect_fallback) in [
            (RemoteCompactionMode::Auto, true),
            (RemoteCompactionMode::Always, false),
        ] {
            let remote_calls = Arc::new(AtomicUsize::new(0));
            let local_calls = Arc::new(AtomicUsize::new(0));
            let runner = TestCompactionRunner {
                remote_error: "model is unsupported".to_string(),
                remote_calls: remote_calls.clone(),
                local_calls: local_calls.clone(),
            };
            let result = run_compaction_with_runner(
                ProviderKind::Claude,
                mode,
                "test-session",
                ModelContext::default(),
                &runner,
            )
            .await;

            assert_eq!(remote_calls.load(Ordering::Relaxed), 1);
            if expect_fallback {
                let output = result.expect("Auto uses local fallback");
                assert!(!output.remote);
                assert_eq!(output.summary, "local fallback");
                assert_eq!(local_calls.load(Ordering::Relaxed), 1);
            } else {
                let error = result.expect_err("Always exposes native error");
                assert!(matches!(
                    error.downcast_ref::<agent_provider::ProviderError>(),
                    Some(agent_provider::ProviderError::NativeCompaction {
                        kind: agent_provider::NativeCompactionErrorKind::Unsupported,
                        ..
                    })
                ));
                assert_eq!(local_calls.load(Ordering::Relaxed), 0);
            }
        }
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
        assert_eq!(resolved.context_window, Some(272_000));
        assert_eq!(resolved.auto_limit_tokens, Some(231_200));
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Always);
    }

    #[test]
    fn gpt56_defaults_use_live_codex_window_and_raw_threshold() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let config = test_config(ProviderKind::OpenAi, model, serde_json::json!({}));
            let resolved = resolve_compaction_config(&config, None);

            assert!(resolved.auto_enabled);
            assert_eq!(resolved.context_window, Some(372_000));
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
            assert_eq!(resolved.context_window, Some(1_000_000));
            assert_eq!(resolved.auto_limit_tokens, Some(500_000));
        }
    }

    #[test]
    fn claude_remote_compaction_remains_opt_in_and_auto_falls_back() {
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

        assert!(should_fallback_after_remote_failure(
            ProviderKind::Claude,
            RemoteCompactionMode::Auto
        ));
        assert!(!should_fallback_after_remote_failure(
            ProviderKind::Claude,
            RemoteCompactionMode::Always
        ));
        assert!(!should_fallback_after_remote_failure(
            ProviderKind::OpenAi,
            RemoteCompactionMode::Auto
        ));
    }

    #[test]
    fn legacy_web_claude_auto_metadata_does_not_enroll_native_compaction() {
        let legacy = test_config(
            ProviderKind::Claude,
            "claude-opus-4-8",
            serde_json::json!({
                "title": "Legacy web session",
                "created_by": "web",
                "compaction": {
                    "config": {
                        "auto_enabled": true,
                        "remote_mode": "auto",
                        "max_consecutive_failures": 3
                    }
                }
            }),
        );

        let resolved = resolve_compaction_config(&legacy, None);

        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Never);
        assert_eq!(resolved.anthropic_native_compaction, None);
    }

    #[test]
    fn claude_remote_policy_fails_closed_for_malformed_metadata() {
        let cases = [
            (
                "null remote mode",
                serde_json::json!({
                    "remote_mode": null,
                    "anthropic_native_compaction": "compact_20260112"
                }),
            ),
            (
                "unknown remote mode",
                serde_json::json!({
                    "remote_mode": "sometimes",
                    "anthropic_native_compaction": "compact_20260112"
                }),
            ),
            (
                "wrong remote mode type",
                serde_json::json!({
                    "remote_mode": 1,
                    "anthropic_native_compaction": "compact_20260112"
                }),
            ),
            (
                "missing version marker",
                serde_json::json!({ "remote_mode": "auto" }),
            ),
            (
                "unknown version marker",
                serde_json::json!({
                    "remote_mode": "always",
                    "anthropic_native_compaction": "future_version"
                }),
            ),
            (
                "malformed sibling",
                serde_json::json!({
                    "remote_mode": "auto",
                    "anthropic_native_compaction": "compact_20260112",
                    "auto_limit_tokens": "not a number"
                }),
            ),
            (
                "explicit never with malformed sibling",
                serde_json::json!({
                    "remote_mode": "never",
                    "anthropic_native_compaction": "compact_20260112",
                    "auto_limit_tokens": "not a number"
                }),
            ),
        ];

        for (name, compaction_config) in cases {
            let config = test_config(
                ProviderKind::Claude,
                "claude-opus-4-8",
                serde_json::json!({
                    "compaction": {
                        "config": compaction_config
                    }
                }),
            );
            assert_eq!(
                resolve_compaction_config(&config, None).remote_mode,
                RemoteCompactionMode::Never,
                "{name}"
            );
        }
    }

    #[test]
    fn openai_legacy_remote_policy_behavior_is_preserved() {
        for (name, config_value, expected) in [
            (
                "missing",
                serde_json::json!({}),
                RemoteCompactionMode::Always,
            ),
            (
                "legacy auto",
                serde_json::json!({ "remote_mode": "auto" }),
                RemoteCompactionMode::Always,
            ),
            (
                "malformed",
                serde_json::json!({ "remote_mode": null }),
                RemoteCompactionMode::Always,
            ),
            (
                "explicit never with malformed sibling",
                serde_json::json!({
                    "remote_mode": "never",
                    "auto_limit_tokens": "not a number"
                }),
                RemoteCompactionMode::Always,
            ),
            (
                "explicit never ignores malformed Claude-only marker",
                serde_json::json!({
                    "remote_mode": "never",
                    "anthropic_native_compaction": { "malformed": true }
                }),
                RemoteCompactionMode::Never,
            ),
        ] {
            let config = test_config(
                ProviderKind::OpenAi,
                "gpt-5.6-sol",
                serde_json::json!({ "compaction": { "config": config_value } }),
            );
            assert_eq!(
                resolve_compaction_config(&config, None).remote_mode,
                expected,
                "{name}"
            );
        }
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
    fn remote_retry_trimming_keeps_compaction_root_and_newer_history() {
        let entry = |item| ModelTranscriptEntry {
            item,
            provider_replay: Vec::new(),
        };
        let mut groups = transcript_groups(vec![
            entry(TranscriptItem::CompactionSummary(
                agent_vocab::CompactionSummary::new(
                    "session",
                    "old-leaf",
                    "prior checkpoint",
                    None,
                    agent_vocab::TurnId(1),
                ),
            )),
            entry(TranscriptItem::TurnStarted {
                turn_id: agent_vocab::TurnId(2),
            }),
            entry(TranscriptItem::UserMessage(UserMessage::text("old turn"))),
            entry(TranscriptItem::TurnFinished {
                turn_id: agent_vocab::TurnId(2),
                outcome: agent_vocab::TurnOutcome::Graceful,
            }),
            entry(TranscriptItem::TurnStarted {
                turn_id: agent_vocab::TurnId(3),
            }),
            entry(TranscriptItem::UserMessage(UserMessage::text("new turn"))),
            entry(TranscriptItem::TurnFinished {
                turn_id: agent_vocab::TurnId(3),
                outcome: agent_vocab::TurnOutcome::Graceful,
            }),
        ]);

        assert!(trim_oldest_complete_group(&mut groups));
        let remaining = entries_from_groups(&groups);
        assert!(matches!(
            remaining.first().map(|entry| &entry.item),
            Some(TranscriptItem::CompactionSummary(_))
        ));
        let visible = remaining
            .iter()
            .filter_map(|entry| match &entry.item {
                TranscriptItem::UserMessage(message) => message.as_text(),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(visible, vec!["new turn"]);
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
        let resolved = resolve_compaction_config(&config, None);

        assert!(!resolved.auto_enabled);
        assert_eq!(resolved.context_window, None);
        assert_eq!(resolved.auto_limit_tokens, None);
    }

    #[test]
    fn discovered_context_window_enables_unknown_claude_model_compaction() {
        let config = test_config(ProviderKind::Claude, "claude-future", serde_json::json!({}));
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(500_000),
                max_output_tokens: Some(96_000),
            }),
        );

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(500_000));
        assert_eq!(resolved.auto_limit_tokens, Some(425_000));
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
                        "remote_mode": "auto",
                        "anthropic_native_compaction": "compact_20260112"
                    }
                }
            }),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(123));
        assert_eq!(resolved.auto_limit_tokens, Some(77));
        assert_eq!(resolved.remote_mode, RemoteCompactionMode::Auto);
        assert_eq!(
            resolved.anthropic_native_compaction,
            Some(AnthropicNativeCompactionVersion::Compact20260112)
        );

        let discovered = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(500_000),
                max_output_tokens: Some(96_000),
            }),
        );
        assert_eq!(discovered.context_window, Some(123));
        assert_eq!(discovered.auto_limit_tokens, Some(77));
    }

    #[test]
    fn explicit_context_window_drives_default_limit_but_explicit_limit_wins() {
        let context_only = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({
                "compaction": {
                    "config": {
                        "context_window": 600_000
                    }
                }
            }),
        );
        let resolved = resolve_compaction_config(
            &context_only,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(1_000_000),
                max_output_tokens: Some(128_000),
            }),
        );
        assert_eq!(resolved.context_window, Some(600_000));
        assert_eq!(resolved.auto_limit_tokens, Some(510_000));

        let explicit_limit = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({
                "compaction": {
                    "config": {
                        "context_window": 600_000,
                        "auto_limit_tokens": 700_000
                    }
                }
            }),
        );
        let resolved = resolve_compaction_config(&explicit_limit, None);
        assert_eq!(resolved.auto_limit_tokens, Some(700_000));
        assert_eq!(auto_limit_tokens(&resolved), Some(600_000));
    }

    #[test]
    fn resolved_compaction_config_respects_explicit_auto_disabled() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-4-5",
            serde_json::json!({ "compaction": { "config": { "auto_enabled": false } } }),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(!resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(200_000));
        assert_eq!(resolved.auto_limit_tokens, Some(170_000));
    }

    #[test]
    fn explicit_null_limits_preserve_safe_discovered_defaults() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-future",
            serde_json::json!({
                "compaction": {
                    "config": {
                        "context_window": null,
                        "auto_limit_tokens": null
                    }
                }
            }),
        );
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(500_000),
                max_output_tokens: Some(96_000),
            }),
        );

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.context_window, Some(500_000));
        assert_eq!(resolved.auto_limit_tokens, Some(425_000));
    }
}
