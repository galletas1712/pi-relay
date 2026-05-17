#![forbid(unsafe_code)]

use agent_vocab::{
    AssistantMessage, ProviderKind, ProviderReplayItem, ReasoningEffort, ToolDefinition,
    TranscriptItem,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod anthropic;
pub mod openai;

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub prompt: PromptSections,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tool_profile: ProviderToolProfile,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub reasoning_effort: ReasoningEffort,
    /// Explicit override for the provider's prompt-cache routing key. When
    /// `None`, providers fall back to `session_id` (the documented "unique to
    /// us" cohort) and finally to a deterministic config-hash for test/CLI
    /// paths that don't carry a session.
    pub prompt_cache_key: Option<String>,
    /// Stable identifier for the pi-relay session that owns this request.
    /// Mirrors Codex CLI's `thread_id` semantics: it doubles as the prompt
    /// cache key (so each session gets its own routing bucket and stays
    /// under OpenAI's ~15 RPM per-shard ceiling) and as the value of the
    /// `session_id` / `thread_id` / `x-client-request-id` headers.
    pub session_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ProviderCompactionRequest {
    pub model: String,
    pub prompt: PromptSections,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tool_profile: ProviderToolProfile,
    pub tools: Vec<ToolDefinition>,
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
    pub tools: Vec<ToolDefinition>,
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
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub input_tokens: Option<usize>,
    pub output_tokens: Option<usize>,
    pub total_tokens: Option<usize>,
    pub cache_read_input_tokens: Option<usize>,
    pub cache_creation_input_tokens: Option<usize>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned an error: {0}")]
    Provider(String),
    #[error("provider returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
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
