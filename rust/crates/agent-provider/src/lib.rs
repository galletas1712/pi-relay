#![forbid(unsafe_code)]

use agent_vocab::{AssistantMessage, ProviderReplayItem, ToolDefinition, TranscriptItem};
use async_trait::async_trait;
use thiserror::Error;

pub mod anthropic;
pub mod openai;

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub prompt: PromptSections,
    pub transcript: Vec<ModelTranscriptEntry>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
    pub prompt_cache_key: Option<String>,
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
}
