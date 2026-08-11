use std::time::Duration;

use agent_vocab::{
    AssistantItem, AssistantMessage, ToolCall, ToolCallId,
};
use serde::Deserialize;
use serde_json::Value;

use crate::{
    CacheRetention, ModelRequest, ModelResponse, ModelStopReason,
    ProviderCompactionRequest, ProviderCompactionResponse, ProviderError,
    ProviderModelMetadata, ProviderResult, ProviderUsage,
};

/// Default sidecar URL (loopback).
pub const SIDECAR_DEFAULT_URL: &str = "http://127.0.0.1:8732";

/// Timeout for sidecar HTTP requests.
const SIDECAR_TIMEOUT: Duration = Duration::from_secs(300);

/// Provider that delegates all model calls to a pi-ai sidecar (Node.js HTTP server).
///
/// The daemon passes an opaque provider string + model id; the sidecar resolves
/// the provider via models.json and handles all provider-specific logic
/// (API type, wire format, cache hints, auth, compaction, web search).
pub struct PiAiSidecarProvider {
    sidecar_url: String,
    client: reqwest::Client,
    provider: String,
}

impl PiAiSidecarProvider {
    pub fn new(sidecar_url: &str, provider: &str) -> Self {
        Self {
            sidecar_url: sidecar_url.to_string(),
            client: reqwest::Client::builder()
                .timeout(SIDECAR_TIMEOUT)
                .build()
                .unwrap_or_default(),
            provider: provider.to_string(),
        }
    }

    pub fn with_client(sidecar_url: &str, client: reqwest::Client, provider: &str) -> Self {
        Self {
            sidecar_url: sidecar_url.to_string(),
            client,
            provider: provider.to_string(),
        }
    }

    async fn post<T: for<'de> Deserialize<'de>>(&self, body: &Value) -> ProviderResult<T> {
        let response = self
            .client
            .post(&self.sidecar_url)
            .header("Content-Type", "application/json")
            .json(body)
            .send()
            .await
            .map_err(ProviderError::Http)?;

        let status = response.status();
        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(ProviderError::Status {
                status: status.as_u16(),
                message: text,
            });
        }

        let result: T = response.json().await.map_err(ProviderError::Http)?;
        Ok(result)
    }
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarCompleteResult {
    #[serde(default)]
    content: Vec<SidecarContent>,
    #[serde(default)]
    usage: Option<SidecarUsage>,
    stop_reason: String,
    #[serde(default)]
    error_message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarContent {
    #[serde(rename = "type")]
    content_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    thinking: Option<String>,
    #[serde(default)]
    signature: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarUsage {
    #[serde(default)]
    input: Option<usize>,
    #[serde(default)]
    output: Option<usize>,
    #[serde(default)]
    total_tokens: Option<usize>,
    #[serde(default)]
    cache_read: Option<usize>,
    #[serde(default)]
    cache_write: Option<usize>,
    #[serde(default)]
    cache_write_1h: Option<usize>,
}

#[derive(Deserialize)]
struct SidecarCompactResult {
    summary: Option<String>,
    #[serde(default)]
    usage: Option<SidecarUsage>,
}

#[derive(Deserialize)]
struct SidecarModelsResult {
    #[serde(default)]
    models: Vec<SidecarModel>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SidecarModel {
    id: String,
    provider: String,
    #[serde(default)]
    context_window: Option<usize>,
    #[serde(default)]
    max_tokens: Option<usize>,
    #[serde(default)]
    reasoning: Option<bool>,
}

#[derive(Deserialize)]
struct SidecarError {
    code: Option<String>,
    message: String,
}

// ── Helper: serialize reasoning effort ──────────────────────────────────────

fn reasoning_str(e: agent_vocab::ReasoningEffort) -> &'static str {
    match e {
        agent_vocab::ReasoningEffort::None => "off",
        agent_vocab::ReasoningEffort::Minimal => "minimal",
        agent_vocab::ReasoningEffort::Low => "low",
        agent_vocab::ReasoningEffort::Medium => "medium",
        agent_vocab::ReasoningEffort::High => "high",
        agent_vocab::ReasoningEffort::XHigh => "xhigh",
        agent_vocab::ReasoningEffort::Max => "max",
    }
}

fn cache_retention_str(r: CacheRetention) -> &'static str {
    match r {
        CacheRetention::None => "none",
        CacheRetention::Short => "short",
        CacheRetention::Long => "long",
    }
}

fn serialize_transcript(transcript: &[crate::ModelTranscriptEntry]) -> Vec<Value> {
    transcript
        .iter()
        .map(|entry| serde_json::to_value(&entry.item).unwrap_or(Value::Null))
        .collect()
}

fn serialize_tools(tools: &[agent_tools::ProviderTool]) -> Vec<Value> {
    tools
        .iter()
        .map(|tool| {
            serde_json::json!({
                "name": tool.name,
                "description": tool.description,
                "input_schema": tool.input_schema,
            })
        })
        .collect()
}

#[async_trait::async_trait]
impl crate::ModelProvider for PiAiSidecarProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let transcript = serialize_transcript(&request.transcript);
        let tools = serialize_tools(&request.tools);
        let reasoning = reasoning_str(request.reasoning_effort);
        let cache_retention = request.cache_retention.map(cache_retention_str);
        let prompt_value = serde_json::to_value(&request.prompt).unwrap_or(Value::Null);

        let body = serde_json::json!({
            "method": "complete",
            "params": {
                "model": request.model,
                "provider": self.provider,
                "prompt": prompt_value,
                "transcript": transcript,
                "tools": tools,
                "reasoning_effort": reasoning,
                "cache_retention": cache_retention,
                "session_id": request.session_id,
                "max_tokens": request.max_tokens,
            }
        });

        let result: SidecarCompleteResult = self.post(&body).await?;

        // Map content to AssistantItem
        let mut items = Vec::new();
        for c in &result.content {
            match c.content_type.as_str() {
                "text" => {
                    if let Some(text) = &c.text {
                        if !text.trim().is_empty() {
                            items.push(AssistantItem::Text(text.clone()));
                        }
                    }
                }
                "thinking" => {
                    items.push(AssistantItem::Thinking {
                        thinking: c.thinking.clone().unwrap_or_default(),
                        signature: c.signature.clone(),
                    });
                }
                "toolCall" | "tool_call" => {
                    let id = c.id.clone().unwrap_or_else(|| {
                        format!("call_{}", uuid::Uuid::new_v4())
                    });
                    let name = c.name.clone().unwrap_or_default();
                    let args = c
                        .arguments
                        .as_ref()
                        .map(|a| serde_json::to_string(a).unwrap_or_default())
                        .unwrap_or_default();
                    items.push(AssistantItem::ToolCall(ToolCall {
                        id: ToolCallId::new(id),
                        tool_name: name,
                        args_json: args,
                    }));
                }
                _ => {}
            }
        }

        let assistant = AssistantMessage { items };

        let usage = result.usage.map(|u| ProviderUsage {
            input_tokens: u.input,
            output_tokens: u.output,
            total_tokens: u.total_tokens,
            cache_read_input_tokens: u.cache_read,
            cache_creation_input_tokens: u.cache_write,
            ..Default::default()
        });

        let stop_reason = match result.stop_reason.as_str() {
            "stop" | "toolUse" | "tool_use" => ModelStopReason::Complete,
            "length" => ModelStopReason::MaxOutputTokens,
            "error" | "refusal" | "aborted" => ModelStopReason::Refusal,
            _ => ModelStopReason::Complete,
        };

        Ok(ModelResponse {
            assistant,
            provider_replay: Vec::new(),
            usage,
            stop_reason,
            stop_details: None,
        })
    }

    async fn compact(
        &self,
        request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        let transcript = serialize_transcript(&request.transcript);
        let tools = serialize_tools(&request.tools);
        let reasoning = reasoning_str(request.reasoning_effort);
        let prompt_value = serde_json::to_value(&request.prompt).unwrap_or(Value::Null);

        let body = serde_json::json!({
            "method": "compact",
            "params": {
                "model": request.model,
                "provider": self.provider,
                "prompt": prompt_value,
                "transcript": transcript,
                "tools": tools,
                "reasoning_effort": reasoning,
                "session_id": request.session_id,
                "compaction_instructions": request.compaction_instructions,
            }
        });

        let result: SidecarCompactResult = self.post(&body).await?;

        let usage = result.usage.map(|u| ProviderUsage {
            input_tokens: u.input,
            output_tokens: u.output,
            total_tokens: u.total_tokens,
            cache_read_input_tokens: u.cache_read,
            cache_creation_input_tokens: u.cache_write,
            ..Default::default()
        });

        Ok(ProviderCompactionResponse {
            summary: result.summary,
            provider_replay: Vec::new(),
            usage,
        })
    }

    async fn model_metadata(&self, model: &str) -> ProviderResult<Option<ProviderModelMetadata>> {
        let body = serde_json::json!({
            "method": "models.available",
            "params": {}
        });

        let result: SidecarModelsResult = self.post(&body).await?;

        let found = result.models.into_iter().find(|m| {
            m.id == model && m.provider == self.provider
        });

        Ok(found.map(|m| ProviderModelMetadata {
            max_input_tokens: m.context_window,
            recommended_auto_compact_tokens: m.context_window.map(|w| w * 85 / 100),
        }))
    }
}
