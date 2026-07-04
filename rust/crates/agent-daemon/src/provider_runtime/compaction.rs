use agent_provider::{
    ModelTranscriptEntry, PromptSections, ProviderCompactionRequest, ProviderCompactionResponse,
    ProviderModelMetadata, ProviderToolProfile,
};
use agent_session::ModelContext;
use agent_store::SessionConfig;
use agent_vocab::{ProviderKind, ProviderReplayItem};
use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::future::Future;

use crate::auth::Credentials;
use crate::delegation_context::compaction_delegation_ledger;
use crate::state::AppState;

use super::auth_retry::compact_with_auth_retry;
use super::prompt::{
    effective_prompt_profile, provider_tools_for_session, render_pi_compaction_prompt,
};
use super::provider::provider_for_config;
use super::transcript::provider_transcript;

fn generic_native_compaction_summary(provider: ProviderKind) -> String {
    match provider {
        ProviderKind::OpenAi => {
            "Conversation history before this point was compacted using OpenAI provider-native compaction.".to_string()
        }
        ProviderKind::Claude => {
            "Conversation history before this point was compacted using provider-native compaction.".to_string()
        }
    }
}

fn generic_auto_limit_for_window(window: usize) -> usize {
    window / 100 * 85 + window % 100 * 85 / 100
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
    pub provider: ProviderKind,
    pub usage: Option<Value>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompactionConfig {
    pub auto_enabled: bool,
    pub auto_limit_tokens: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
struct StoredCompactionPolicy {
    #[serde(default)]
    auto_enabled: PolicyField<bool>,
    #[serde(default)]
    context_window: PolicyField<usize>,
    #[serde(default)]
    auto_limit_tokens: PolicyField<usize>,
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
    let selected = config.metadata.pointer("/compaction/config");
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

pub(crate) fn compaction_config_with_model_metadata(
    discovered: Option<ProviderModelMetadata>,
    policy: &ParsedCompactionPolicy,
) -> CompactionConfig {
    resolve_compaction_config_with_policy(discovered, policy)
}

#[cfg(test)]
fn resolve_compaction_config(
    config: &SessionConfig,
    discovered: Option<ProviderModelMetadata>,
) -> CompactionConfig {
    let policy = parse_compaction_policy(config);
    resolve_compaction_config_with_policy(discovered, &policy)
}

fn resolve_compaction_config_with_policy(
    discovered: Option<ProviderModelMetadata>,
    parsed: &ParsedCompactionPolicy,
) -> CompactionConfig {
    let default_policy = StoredCompactionPolicy::default();
    let policy = match &parsed.state {
        ParsedCompactionPolicyState::Missing => &default_policy,
        ParsedCompactionPolicyState::Valid(policy) => policy,
        ParsedCompactionPolicyState::Invalid => return invalid_compaction_config(),
    };
    let explicit_window = policy.context_window.get();
    let context_window =
        explicit_window.or_else(|| discovered.and_then(|metadata| metadata.max_input_tokens));
    let requested_limit = policy
        .auto_limit_tokens
        .get()
        .or_else(|| explicit_window.map(generic_auto_limit_for_window))
        .or_else(|| discovered.and_then(|metadata| metadata.recommended_auto_compact_tokens))
        .or_else(|| {
            discovered
                .and_then(|metadata| metadata.max_input_tokens)
                .map(generic_auto_limit_for_window)
        });
    let auto_limit_tokens = effective_auto_limit(context_window, requested_limit);
    let auto_enabled = policy.auto_enabled.get().unwrap_or(true)
        && (context_window.is_none() || auto_limit_tokens.is_some());
    CompactionConfig {
        auto_enabled,
        auto_limit_tokens,
    }
}

fn invalid_compaction_config() -> CompactionConfig {
    CompactionConfig {
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
    eprintln!(
        "attempting provider-native compaction for {session_id} with {}",
        config.provider.kind
    );
    let output = run_native_compaction(state, config, session_id, model_context).await?;
    append_delegation_ledger_to_output(state, session_id, output).await
}

async fn run_native_compaction(
    state: &AppState,
    config: &SessionConfig,
    session_id: &str,
    model_context: ModelContext,
) -> Result<CompactionOutput> {
    run_native_compaction_once(
        config.provider.kind,
        model_context,
        |transcript| async move {
            let request = native_compaction_request(state, config, session_id, transcript).await?;
            let credentials = Credentials::load();
            let provider = provider_for_config(state, config, &credentials, session_id).await?;
            compact_with_auth_retry(state, config, session_id, provider, request)
                .await
                .map_err(Into::into)
        },
    )
    .await
}

async fn run_native_compaction_once<F, Fut>(
    provider: ProviderKind,
    model_context: ModelContext,
    compact: F,
) -> Result<CompactionOutput>
where
    F: FnOnce(Vec<ModelTranscriptEntry>) -> Fut,
    Fut: Future<Output = Result<ProviderCompactionResponse>>,
{
    let result = compact(provider_transcript(model_context)).await?;
    Ok(native_compaction_output(provider, result))
}

fn native_compaction_output(
    provider: ProviderKind,
    result: ProviderCompactionResponse,
) -> CompactionOutput {
    let (summary, summary_kind) = match result.summary {
        Some(summary) if !summary.trim().is_empty() => (
            summary.trim().to_string(),
            CompactionSummaryKind::ProviderText,
        ),
        _ => (
            generic_native_compaction_summary(provider),
            CompactionSummaryKind::Generic,
        ),
    };
    CompactionOutput {
        summary,
        summary_kind,
        provider_replay: result.provider_replay,
        provider,
        usage: result
            .usage
            .and_then(|usage| serde_json::to_value(usage).ok()),
    }
}

pub(crate) async fn native_compaction_request(
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
        reasoning_effort: config.provider.reasoning_effort,
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

#[cfg(test)]
mod tests {
    use super::*;
    use agent_provider::{
        ModelProvider, ModelRequest, ModelResponse, ProviderError, ProviderResult,
    };
    use agent_vocab::{TranscriptItem, UserMessage};
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
    struct RecordingProvider {
        compact_calls: AtomicUsize,
        complete_calls: AtomicUsize,
        compact_transcripts: Mutex<Vec<Vec<ModelTranscriptEntry>>>,
        fail_compact: bool,
    }

    #[async_trait::async_trait]
    impl ModelProvider for RecordingProvider {
        async fn complete(&self, _request: ModelRequest) -> ProviderResult<ModelResponse> {
            self.complete_calls.fetch_add(1, Ordering::Relaxed);
            Err(ProviderError::Provider(
                "ordinary generation must not run during compaction".to_string(),
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
            if self.fail_compact {
                Err(ProviderError::Status {
                    status: 413,
                    message: "context length exceeded".to_string(),
                })
            } else {
                Ok(ProviderCompactionResponse {
                    summary: Some("native summary".to_string()),
                    provider_replay: Vec::new(),
                    usage: None,
                })
            }
        }
    }

    fn provider_request(transcript: Vec<ModelTranscriptEntry>) -> ProviderCompactionRequest {
        ProviderCompactionRequest {
            model: "claude-opus-4-8".to_string(),
            prompt: PromptSections::stable("test prompt"),
            transcript,
            tool_profile: ProviderToolProfile::AnthropicCoding,
            tools: Vec::new(),
            reasoning_effort: agent_vocab::ReasoningEffort::High,
            prompt_cache_key: None,
            session_id: Some("test-session".to_string()),
            compaction_instructions: Some("compact".to_string()),
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
        let provider = RecordingProvider {
            fail_compact: true,
            ..RecordingProvider::default()
        };
        let error = run_native_compaction_once(
            ProviderKind::Claude,
            ModelContext::from_transcript_items(items),
            |transcript| async {
                provider
                    .compact(provider_request(transcript))
                    .await
                    .map_err(Into::into)
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
    fn missing_metadata_has_no_static_proactive_threshold() {
        let config = test_config(
            ProviderKind::OpenAi,
            "gpt-5.1-codex-max",
            serde_json::json!({}),
        );
        let resolved = resolve_compaction_config(&config, None);

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, None);
    }

    #[test]
    fn gpt56_uses_provider_discovered_window_and_threshold() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let config = test_config(ProviderKind::OpenAi, model, serde_json::json!({}));
            let resolved = resolve_compaction_config(
                &config,
                Some(ProviderModelMetadata {
                    max_input_tokens: Some(372_000),
                    recommended_auto_compact_tokens: Some(334_800),
                }),
            );

            assert!(resolved.auto_enabled);
            assert_eq!(resolved.auto_limit_tokens, Some(334_800));
        }
    }

    #[test]
    fn gpt54_uses_current_window_recommendation_not_maximum_window() {
        let config = test_config(ProviderKind::OpenAi, "gpt-5.4", serde_json::json!({}));
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(272_000),
                recommended_auto_compact_tokens: Some(244_800),
            }),
        );

        assert_eq!(resolved.auto_limit_tokens, Some(244_800));
        assert_ne!(resolved.auto_limit_tokens, Some(900_000));
    }

    #[test]
    fn sonnet_45_provider_fallback_keeps_generic_170k_scheduler_threshold() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-4-5",
            serde_json::json!({}),
        );
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(200_000),
                recommended_auto_compact_tokens: Some(170_000),
            }),
        );

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(170_000));
    }

    #[tokio::test]
    async fn default_claude_uses_half_window_scheduler_and_native_execution() {
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({}),
        );
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(1_000_000),
                recommended_auto_compact_tokens: Some(500_000),
            }),
        );
        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(500_000));

        let provider = RecordingProvider::default();
        let output = run_native_compaction_once(
            ProviderKind::Claude,
            ModelContext::from_transcript_items(vec![TranscriptItem::UserMessage(
                UserMessage::text("compact this"),
            )]),
            |transcript| async {
                provider
                    .compact(provider_request(transcript))
                    .await
                    .map_err(Into::into)
            },
        )
        .await
        .expect("native compaction succeeds");

        assert_eq!(output.summary, "native summary");
        assert_eq!(provider.compact_calls.load(Ordering::Relaxed), 1);
        assert_eq!(provider.complete_calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn native_compaction_output_preserves_checkpoint_replay_and_raw_usage() {
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
        let output = native_compaction_output(
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
    fn missing_policy_uses_provider_metadata_or_reactive_only() {
        let openai = resolve_compaction_config(
            &test_config(ProviderKind::OpenAi, "gpt-5.6-sol", serde_json::json!({})),
            Some(ProviderModelMetadata {
                max_input_tokens: Some(372_000),
                recommended_auto_compact_tokens: Some(334_800),
            }),
        );
        assert!(openai.auto_enabled);
        assert_eq!(openai.auto_limit_tokens, Some(334_800));

        let unknown = resolve_compaction_config(
            &test_config(ProviderKind::OpenAi, "unknown", serde_json::json!({})),
            None,
        );
        assert!(unknown.auto_enabled);
        assert_eq!(unknown.auto_limit_tokens, None);
    }

    #[test]
    fn unknown_nested_metadata_does_not_change_scheduler_policy() {
        let mut policy = serde_json::Map::new();
        policy.insert(
            "extra_metadata".to_string(),
            serde_json::json!({ "ignored": true }),
        );
        policy.insert(
            "max_consecutive_failures".to_string(),
            serde_json::json!("store-owned"),
        );
        let config = test_config(
            ProviderKind::Claude,
            "claude-sonnet-5",
            serde_json::json!({
                "compaction": { "config": Value::Object(policy) }
            }),
        );

        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(1_000_000),
                recommended_auto_compact_tokens: Some(500_000),
            }),
        );
        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(500_000));
    }

    #[test]
    fn only_nested_scheduler_config_is_active() {
        let config = test_config(
            ProviderKind::OpenAi,
            "gpt-5.6-sol",
            serde_json::json!({
                "compaction": {
                    "auto_enabled": false,
                    "auto_limit_tokens": 123_456
                }
            }),
        );
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(372_000),
                recommended_auto_compact_tokens: Some(334_800),
            }),
        );

        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(334_800));
    }

    #[test]
    fn malformed_active_scheduler_fields_fail_closed() {
        for policy_value in [
            serde_json::json!({ "auto_limit_tokens": "invalid" }),
            serde_json::json!({ "auto_enabled": null }),
            Value::Null,
        ] {
            let config = test_config(
                ProviderKind::OpenAi,
                "gpt-5.6-sol",
                serde_json::json!({ "compaction": { "config": policy_value } }),
            );
            let policy = parse_compaction_policy(&config);
            let resolved = resolve_compaction_config_with_policy(None, &policy);

            assert!(policy.explicitly_disables_auto());
            assert!(!resolved.auto_enabled);
            assert_eq!(resolved.auto_limit_tokens, None);
        }
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
        assert!(!resolve_compaction_config_with_policy(None, &policy).auto_enabled);
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
                recommended_auto_compact_tokens: Some(425_000),
            }),
        );
        assert!(resolved.auto_enabled);
        assert_eq!(resolved.auto_limit_tokens, Some(425_000));
    }

    #[test]
    fn explicit_session_policy_wins_and_is_clamped_against_explicit_window() {
        let config = test_config(
            ProviderKind::OpenAi,
            "gpt-5.6-sol",
            serde_json::json!({
                "compaction": { "config": {
                    "context_window": 100_000,
                    "auto_limit_tokens": 120_000
                }}
            }),
        );
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(372_000),
                recommended_auto_compact_tokens: Some(334_800),
            }),
        );

        assert_eq!(resolved.auto_limit_tokens, Some(100_000));
    }

    #[test]
    fn authoritative_window_without_recommendation_uses_generic_policy() {
        let config = test_config(ProviderKind::OpenAi, "future-model", serde_json::json!({}));
        let resolved = resolve_compaction_config(
            &config,
            Some(ProviderModelMetadata {
                max_input_tokens: Some(200_000),
                recommended_auto_compact_tokens: None,
            }),
        );

        assert_eq!(resolved.auto_limit_tokens, Some(170_000));
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
