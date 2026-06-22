#![forbid(unsafe_code)]

use agent_tools::{ProviderTool, ToolRegistry};
use agent_vocab::{
    AssistantMessage, ProviderKind, ProviderReplayItem, ReasoningEffort, ToolCall, TranscriptItem,
    TurnId,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

pub mod anthropic;
mod common;
mod http;
pub mod openai;
mod sse;
mod token_estimator;
mod transcript;

pub use token_estimator::{
    approx_tokens_from_byte_count, estimate_model_input, estimate_model_input_tokens,
    estimate_transcript_tokens, TokenEstimate,
};
pub use transcript::normalize_transcript_for_provider;

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub prompt: PromptSections,
    /// If set, providers that support transcript cache markers should place
    /// those markers only within the first `n` transcript entries.
    ///
    /// This is useful for non-persistent sidecar requests that append an
    /// instruction after an otherwise normal model-turn transcript: the
    /// provider-visible prefix stays identical to the regular request, and the
    /// sidecar-only suffix does not become the cache breakpoint.
    pub transcript_cache_prefix_len: Option<usize>,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tool_profile: ProviderToolProfile,
    pub tools: Vec<ProviderTool>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: ReasoningEffort,
    /// Explicit override for the provider's prompt-cache routing key. OpenAI
    /// uses this first, then falls back to the request/session routing id. When
    /// neither the request nor provider session state carries an id, OpenAI
    /// generates a fresh UUID for CLI/test paths before building the body.
    pub prompt_cache_key: Option<String>,
    /// Stable identifier for the pi-relay session that owns this request.
    /// Mirrors Codex CLI's `thread_id` semantics: it doubles as the prompt
    /// cache key when no explicit override is set (so each session gets its
    /// own routing bucket and stays
    /// under OpenAI's ~15 RPM per-shard ceiling) and as the value of the
    /// `session_id` / `thread_id` / `x-client-request-id` headers.
    pub session_id: Option<String>,
    /// Turn identifier for the user turn that owns this model request.
    ///
    /// Codex treats `x-codex-turn-state` as turn-scoped sticky routing state:
    /// any value returned by an upstream request should be replayed by later
    /// requests for the same turn, but must not leak into future turns.
    pub turn_id: Option<TurnId>,
}

#[derive(Debug, Clone)]
pub struct ProviderCompactionRequest {
    pub model: String,
    pub prompt: PromptSections,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tool_profile: ProviderToolProfile,
    pub tools: Vec<ProviderTool>,
    pub reasoning_effort: ReasoningEffort,
    pub prompt_cache_key: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderCompactionResponse {
    /// Provider-returned text, if the provider exposes one. Provider-native
    /// compaction endpoints can return only opaque replay state.
    pub summary: Option<String>,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub usage: Option<ProviderUsage>,
}

#[derive(Debug, Clone)]
pub struct ProviderTokenCountRequest {
    pub model: String,
    pub prompt: PromptSections,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tool_profile: ProviderToolProfile,
    pub tools: Vec<ProviderTool>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: ReasoningEffort,
    pub prompt_cache_key: Option<String>,
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTokenCountResponse {
    pub input_tokens: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderToolProfile {
    None,
    CustomDefinitions,
    OpenAiCoding,
    AnthropicCoding,
}

impl ProviderToolProfile {
    pub fn for_provider(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenAi => Self::OpenAiCoding,
            ProviderKind::Claude => Self::AnthropicCoding,
        }
    }
}

fn effective_provider_tools(
    profile: ProviderToolProfile,
    tools: Vec<ProviderTool>,
) -> Vec<ProviderTool> {
    if !tools.is_empty() {
        return tools;
    }
    match profile {
        ProviderToolProfile::OpenAiCoding => {
            ToolRegistry::with_builtin_tools().provider_tools_for_provider(ProviderKind::OpenAi)
        }
        ProviderToolProfile::AnthropicCoding => {
            ToolRegistry::with_builtin_tools().provider_tools_for_provider(ProviderKind::Claude)
        }
        ProviderToolProfile::None | ProviderToolProfile::CustomDefinitions => Vec::new(),
    }
}

impl ModelTranscriptEntry {
    pub fn provider_replay_for(&self, provider: ProviderKind) -> Vec<ProviderReplayItem> {
        self.provider_replay
            .iter()
            .filter(|record| record.provider == provider)
            .filter_map(|record| canonical_provider_replay(record, provider))
            .collect()
    }

    pub fn item(&self) -> &TranscriptItem {
        &self.item
    }
}

pub fn canonical_tool_call_for_provider(provider: ProviderKind, call: &ToolCall) -> ToolCall {
    let tool_name = canonical_tool_name_for_provider(provider, &call.tool_name);
    if tool_name == call.tool_name {
        return call.clone();
    }
    ToolCall {
        id: call.id.clone(),
        tool_name: tool_name.to_string(),
        args_json: call.args_json.clone(),
    }
}

pub fn canonical_tool_name_for_provider(provider: ProviderKind, name: &str) -> &str {
    match provider {
        ProviderKind::OpenAi => match name {
            "apply_patch" => "Edit",
            "web_search" => "WebSearch",
            "web_fetch" => "WebFetch",
            other => other,
        },
        ProviderKind::Claude => match name {
            "str_replace_based_edit_tool" => "Edit",
            "web_search" => "WebSearch",
            "web_fetch" => "WebFetch",
            other => other,
        },
    }
}

fn canonical_provider_replay(
    record: &ProviderReplayItem,
    provider: ProviderKind,
) -> Option<ProviderReplayItem> {
    let mut raw = record.raw_value().ok()?;
    if let Some(name) = raw.get("name").and_then(Value::as_str) {
        let canonical = canonical_tool_name_for_provider(provider, name);
        // Local client-tool calls are stored internally under canonical
        // pi-relay names. Provider-hosted/server replay blocks keep their
        // provider-native names so a later stateless request can replay them
        // byte-for-byte and the web UI can still pair provider result blocks.
        let is_provider_hosted_replay = matches!(
            raw.get("type").and_then(Value::as_str),
            Some("server_tool_use" | "web_search_call")
        );
        if canonical != name && !is_provider_hosted_replay {
            raw["name"] = Value::String(canonical.to_string());
        }
    }
    ProviderReplayItem::new_with_display(provider, &raw, record.display.clone()).ok()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTranscriptEntry {
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}

impl From<TranscriptItem> for ModelTranscriptEntry {
    fn from(item: TranscriptItem) -> Self {
        Self {
            item,
            provider_replay: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptSections {
    pub stable_prefix: Option<String>,
    pub dynamic_context: Option<String>,
}

impl PromptSections {
    pub fn new(stable_prefix: Option<String>, dynamic_context: Option<String>) -> Self {
        Self {
            stable_prefix: normalize_prompt_section(stable_prefix),
            dynamic_context: normalize_prompt_section(dynamic_context),
        }
    }

    pub fn stable(stable_prefix: impl Into<String>) -> Self {
        Self::new(Some(stable_prefix.into()), None)
    }

    pub fn render_joined(&self) -> Option<String> {
        match (&self.stable_prefix, &self.dynamic_context) {
            (Some(stable), Some(dynamic)) => Some(format!("{stable}\n\n{dynamic}")),
            (Some(stable), None) => Some(stable.clone()),
            (None, Some(dynamic)) => Some(dynamic.clone()),
            (None, None) => None,
        }
    }
}

fn normalize_prompt_section(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub assistant: AssistantMessage,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub usage: Option<ProviderUsage>,
    pub stop_reason: ModelStopReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Complete,
    MaxOutputTokens,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub input_tokens: Option<usize>,
    pub output_tokens: Option<usize>,
    pub total_tokens: Option<usize>,
    pub cache_read_input_tokens: Option<usize>,
    pub cache_creation_input_tokens: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_ray: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_turn_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_included: Option<bool>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider request timed out: {0}")]
    Timeout(String),
    #[error("transient provider error: {0}")]
    Transient(String),
    #[error("provider returned an error: {0}")]
    Provider(String),
    #[error("provider returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl ProviderError {
    pub fn status_code(&self) -> Option<u16> {
        match self {
            ProviderError::Status { status, .. } => Some(*status),
            ProviderError::Http(error) => error.status().map(|status| status.as_u16()),
            ProviderError::Timeout(_)
            | ProviderError::Transient(_)
            | ProviderError::Provider(_)
            | ProviderError::Json(_) => None,
        }
    }

    pub fn is_retryable_transient(&self) -> bool {
        if self.is_context_overflow() {
            return false;
        }

        if self
            .status_code()
            .is_some_and(is_retryable_transient_status)
        {
            return true;
        }

        match self {
            ProviderError::Timeout(_) | ProviderError::Transient(_) => true,
            ProviderError::Http(error) => {
                error.is_timeout()
                    || error.is_connect()
                    || error.is_request()
                    || error.is_body()
                    || error.is_decode()
            }
            ProviderError::Status { .. } | ProviderError::Provider(_) | ProviderError::Json(_) => {
                false
            }
        }
    }

    pub fn retry_diagnostic(&self) -> Option<String> {
        match self {
            ProviderError::Http(error) => {
                let flags = [
                    ("timeout", error.is_timeout()),
                    ("connect", error.is_connect()),
                    ("request", error.is_request()),
                    ("body", error.is_body()),
                    ("decode", error.is_decode()),
                    ("status", error.is_status()),
                ]
                .into_iter()
                .filter_map(|(name, enabled)| enabled.then_some(name))
                .collect::<Vec<_>>();
                let mut parts = Vec::new();
                if !flags.is_empty() {
                    parts.push(format!("reqwest_flags={}", flags.join(",")));
                }
                if let Some(status) = error.status() {
                    parts.push(format!("status={}", status.as_u16()));
                }
                let source_chain = error_source_chain(error);
                if !source_chain.is_empty() {
                    parts.push(format!("sources={}", source_chain.join(" <- ")));
                }
                (!parts.is_empty()).then(|| parts.join("; "))
            }
            ProviderError::Timeout(message) => Some(format!("timeout={message}")),
            ProviderError::Transient(message) => Some(format!("transient={message}")),
            ProviderError::Status { status, .. } => Some(format!("status={status}")),
            ProviderError::Provider(_) | ProviderError::Json(_) => None,
        }
    }

    pub fn is_context_overflow(&self) -> bool {
        // Only match errors whose message clearly identifies a context-window
        // overflow. A plain 400 is not enough: Anthropic /count_tokens, for
        // example, returns schema-validation 400s for unsupported server tools.
        let status = self.status_code();
        let message = match self {
            ProviderError::Status { message, .. }
            | ProviderError::Transient(message)
            | ProviderError::Provider(message) => message.clone(),
            ProviderError::Http(error) => error.to_string(),
            ProviderError::Timeout(_) => return false,
            ProviderError::Json(_) => return false,
        };
        let lower = message.to_ascii_lowercase();
        if status == Some(413) {
            return true;
        }
        if lower.contains("prompt is too long") {
            return true;
        }
        if lower.contains("context_length_exceeded") {
            return true;
        }
        lower.contains("context")
            && (lower.contains("length")
                || lower.contains("window")
                || lower.contains("too large")
                || lower.contains("exceed")
                || lower.contains("maximum"))
    }
}

fn is_retryable_transient_status(status: u16) -> bool {
    matches!(status, 408 | 409 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn error_source_chain(error: &(dyn std::error::Error + 'static)) -> Vec<String> {
    let mut chain = Vec::new();
    let mut source = error.source();
    while let Some(error) = source {
        chain.push(error.to_string());
        source = error.source();
    }
    chain
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse>;

    fn supports_remote_compaction(&self) -> bool {
        false
    }

    async fn compact(
        &self,
        _request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        Err(ProviderError::Provider(
            "provider does not support remote compaction".to_string(),
        ))
    }

    async fn count_tokens(
        &self,
        _request: ProviderTokenCountRequest,
    ) -> ProviderResult<ProviderTokenCountResponse> {
        Err(ProviderError::Provider(
            "provider does not support token counting".to_string(),
        ))
    }
}

#[cfg(test)]
mod provider_error_tests {
    use super::*;
    use agent_vocab::{ReplayDisplay, ReplayDisplayKind};
    use serde_json::json;

    #[test]
    fn context_overflow_classifier_matches_known_provider_messages() {
        assert!(ProviderError::Status {
            status: 400,
            message: "invalid_request_error: prompt is too long: 1100000 tokens > 1000000 maximum"
                .to_string(),
        }
        .is_context_overflow());
        assert!(ProviderError::Status {
            status: 413,
            message: "request entity too large".to_string(),
        }
        .is_context_overflow());
        assert!(ProviderError::Status {
            status: 400,
            message: "context_length_exceeded: input is too long".to_string(),
        }
        .is_context_overflow());
        assert!(ProviderError::Provider(
            "Your input exceeds the context window of this model.".to_string(),
        )
        .is_context_overflow());
        assert!(!ProviderError::Status {
            status: 400,
            message:
                "invalid_request_error: Server tools are not supported in the count_tokens endpoint: web_fetch_20250910, web_search_20250305."
                    .to_string(),
        }
        .is_context_overflow());
        assert!(!ProviderError::Status {
            status: 400,
            message: "invalid_request_error: messages: at least one message is required"
                .to_string(),
        }
        .is_context_overflow());
    }

    #[test]
    fn retryable_transient_classifier_matches_retryable_statuses_only() {
        for status in [408, 409, 429, 500, 502, 503, 504, 529] {
            assert!(
                ProviderError::Status {
                    status,
                    message: "transient".to_string(),
                }
                .is_retryable_transient(),
                "status {status} should be retryable"
            );
        }

        for status in [400, 401, 403, 404, 413, 422] {
            assert!(
                !ProviderError::Status {
                    status,
                    message: "not transient".to_string(),
                }
                .is_retryable_transient(),
                "status {status} should not be retryable"
            );
        }
    }

    #[test]
    fn retryable_transient_classifier_excludes_context_overflow() {
        assert!(!ProviderError::Status {
            status: 413,
            message: "request entity too large".to_string(),
        }
        .is_retryable_transient());
    }

    #[test]
    fn timeout_errors_are_retryable_transients() {
        let error = ProviderError::Timeout("response headers timed out".to_string());

        assert!(error.is_retryable_transient());
        assert!(!error.is_context_overflow());
        assert_eq!(
            error.retry_diagnostic(),
            Some("timeout=response headers timed out".to_string())
        );
    }

    #[test]
    fn transient_provider_errors_are_retryable_unless_context_overflow() {
        let error = ProviderError::Transient("server disconnected".to_string());

        assert!(error.is_retryable_transient());
        assert_eq!(
            error.retry_diagnostic(),
            Some("transient=server disconnected".to_string())
        );

        let error = ProviderError::Transient("context_length_exceeded".to_string());

        assert!(error.is_context_overflow());
        assert!(!error.is_retryable_transient());
    }

    #[test]
    fn provider_replay_canonicalizes_local_tool_names_only() {
        let local = ProviderReplayItem::new(
            ProviderKind::OpenAi,
            &json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "web_search",
                "arguments": "{\"query\":\"rust\"}",
            }),
        )
        .unwrap();
        let local = canonical_provider_replay(&local, ProviderKind::OpenAi)
            .unwrap()
            .raw_value()
            .unwrap();
        assert_eq!(local["name"], "WebSearch");

        let hosted = ProviderReplayItem::new_with_display(
            ProviderKind::Claude,
            &json!({
                "type": "server_tool_use",
                "id": "srv_1",
                "name": "web_fetch",
                "input": { "url": "https://example.com" },
            }),
            Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "WebFetch".to_string(),
                input_summary: Some("https://example.com".to_string()),
            }),
        )
        .unwrap();
        let hosted = canonical_provider_replay(&hosted, ProviderKind::Claude)
            .unwrap()
            .raw_value()
            .unwrap();
        assert_eq!(hosted["name"], "web_fetch");
    }
}
