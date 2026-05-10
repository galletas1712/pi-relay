#![forbid(unsafe_code)]

use agent_vocab::{AssistantMessage, ToolDefinition, TranscriptItem};
use async_trait::async_trait;
use thiserror::Error;

pub mod anthropic;
pub mod openai;

#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub model: String,
    pub system_prompt: Option<String>,
    pub transcript: Vec<TranscriptItem>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub assistant: AssistantMessage,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider returned an error: {0}")]
    Provider(String),
    #[error("unsupported transcript item for provider request")]
    UnsupportedTranscriptItem,
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse>;
}
