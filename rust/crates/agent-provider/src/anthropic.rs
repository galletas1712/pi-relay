use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ProviderKind,
    ProviderReplayItem, ToolCall, ToolCallId, ToolDefinition, TranscriptItem, UserMessage,
};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{json, Value};

use crate::{
    ModelProvider, ModelRequest, ModelResponse, ModelTranscriptEntry, ProviderError, ProviderResult,
};

const THINKING_BUDGET_TOKENS: u32 = 1024;
const ANTHROPIC_BETA_HEADER: &str = "interleaved-thinking-2025-05-14,extended-cache-ttl-2025-04-11";

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let body = messages_body(request)?;

        let response = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", ANTHROPIC_BETA_HEADER)
            .json(&body)
            .send()
            .await?;
        let (status, text) = response_text(response).await?;
        ensure_success(status, &text)?;
        let response: Value = serde_json::from_str(&text).map_err(|error| {
            ProviderError::Provider(format!(
                "failed to parse Anthropic response JSON: {error}; body: {}",
                response_excerpt(&text)
            ))
        })?;

        parse_anthropic_message(&response)
    }
}

fn messages_body(request: ModelRequest) -> ProviderResult<Value> {
    let max_tokens = request
        .max_tokens
        .unwrap_or(4096)
        .max(THINKING_BUDGET_TOKENS + 1);
    let mut body = json!({
        "model": request.model,
        "max_tokens": max_tokens,
        "messages": transcript_to_messages(&request.transcript)?,
        "thinking": {
            "type": "enabled",
            "budget_tokens": THINKING_BUDGET_TOKENS,
        },
    });
    if let Some(system_blocks) = anthropic_system_blocks(&request.prompt) {
        body["system"] = Value::Array(system_blocks);
    }
    if !request.tools.is_empty() {
        body["tools"] = Value::Array(request.tools.iter().map(anthropic_tool).collect());
        body["tool_choice"] = json!({ "type": "auto" });
    }
    Ok(body)
}

async fn response_text(response: reqwest::Response) -> ProviderResult<(StatusCode, String)> {
    let status = response.status();
    let bytes = response.bytes().await?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

fn ensure_success(status: StatusCode, body: &str) -> ProviderResult<()> {
    if status.is_success() {
        return Ok(());
    }
    Err(ProviderError::Status {
        status: status.as_u16(),
        message: response_error_message(body),
    })
}

fn response_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .pointer("/error/message")
                .or_else(|| value.pointer("/message"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .unwrap_or_else(|| response_excerpt(body))
}

fn response_excerpt(body: &str) -> String {
    const MAX_CHARS: usize = 1200;
    let trimmed = body.trim();
    let mut excerpt = trimmed.chars().take(MAX_CHARS).collect::<String>();
    if trimmed.chars().count() > MAX_CHARS {
        excerpt.push_str("...");
    }
    if excerpt.is_empty() {
        "empty response body".to_string()
    } else {
        excerpt
    }
}

fn anthropic_tool(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn anthropic_system_blocks(prompt: &crate::PromptSections) -> Option<Vec<Value>> {
    let mut blocks = Vec::new();
    if let Some(stable) = &prompt.stable_prefix {
        blocks.push(json!({
            "type": "text",
            "text": stable,
            "cache_control": {
                "type": "ephemeral",
                "ttl": "1h",
            },
        }));
    }
    if let Some(dynamic) = &prompt.dynamic_context {
        blocks.push(json!({
            "type": "text",
            "text": dynamic,
        }));
    }
    (!blocks.is_empty()).then_some(blocks)
}

fn transcript_to_messages(items: &[ModelTranscriptEntry]) -> ProviderResult<Vec<Value>> {
    let mut messages = Vec::new();
    for entry in items {
        match &entry.item {
            TranscriptItem::UserMessage(message) => {
                messages
                    .push(json!({ "role": "user", "content": anthropic_user_content(message) }));
            }
            TranscriptItem::CompactionSummary(summary) => {
                messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": compaction_summary_text(summary) }],
                }));
            }
            TranscriptItem::AssistantMessage(message) => {
                let mut content = anthropic_replay_blocks(&entry.provider_replay)?;
                if content.is_empty() {
                    for item in &message.items {
                        match item {
                            AssistantItem::Text(text) => {
                                content.push(json!({ "type": "text", "text": text }))
                            }
                            AssistantItem::ToolCall(call) => content.push(json!({
                                "type": "tool_use",
                                "id": call.id.as_str(),
                                "name": call.tool_name,
                                "input": call.args_value().unwrap_or_else(|_| json!({})),
                            })),
                        }
                    }
                }
                if !content.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": content }));
                }
            }
            TranscriptItem::ToolResult(result) => {
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id.as_str(),
                        "content": result.output,
                        "is_error": matches!(result.status, agent_vocab::ToolResultStatus::Error | agent_vocab::ToolResultStatus::Interrupted | agent_vocab::ToolResultStatus::Crashed),
                    }]
                }));
            }
            TranscriptItem::TurnStarted { .. }
            | TranscriptItem::ToolCallStarted { .. }
            | TranscriptItem::TurnFinished { .. } => {}
        }
    }
    Ok(messages)
}

fn anthropic_replay_blocks(replay: &[ProviderReplayItem]) -> ProviderResult<Vec<Value>> {
    replay
        .iter()
        .filter(|record| record.provider == ProviderKind::Claude)
        .map(|record| record.raw_value().map_err(ProviderError::Json))
        .collect()
}

fn compaction_summary_text(summary: &CompactionSummary) -> String {
    format!(
        "The conversation history before this point was compacted into this summary:\n\n{}",
        summary.summary
    )
}

fn anthropic_user_content(message: &UserMessage) -> Value {
    Value::Array(
        message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
                ContentBlock::Image { image } => match &image.source {
                    agent_vocab::ImageSource::Base64(data) => json!({
                        "type": "image",
                        "source": {
                            "type": "base64",
                            "media_type": image.mime_type,
                            "data": data,
                        }
                    }),
                    agent_vocab::ImageSource::Url(url) => {
                        json!({ "type": "text", "text": format!("[image url: {url}]") })
                    }
                },
            })
            .collect(),
    )
}

fn parse_anthropic_message(response: &Value) -> ProviderResult<ModelResponse> {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Provider("missing content array".to_string()))?;
    let mut items = Vec::new();
    let mut provider_replay = Vec::new();
    for block in content {
        let Some(block_type) = block.get("type").and_then(Value::as_str) else {
            continue;
        };
        provider_replay.push(ProviderReplayItem::new(ProviderKind::Claude, block)?);

        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    items.push(AssistantItem::Text(text.to_string()));
                }
            }
            "thinking" | "redacted_thinking" => {}
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).unwrap_or_default();
                let name = block
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let input = block.get("input").cloned().unwrap_or_else(|| json!({}));
                items.push(AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new(id),
                    tool_name: name.to_string(),
                    args_json: serde_json::to_string(&input)?,
                }));
            }
            _ => {}
        }
    }
    Ok(ModelResponse {
        assistant: AssistantMessage { items },
        provider_replay,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptSections;

    #[test]
    fn messages_body_enables_thinking_and_auto_tools() {
        let body = messages_body(ModelRequest {
            model: "claude-sonnet-4-5".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tools: vec![ToolDefinition {
                name: "read".to_string(),
                description: "read a file".to_string(),
                input_schema: json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            }],
            max_tokens: Some(2048),
            prompt_cache_key: None,
        })
        .expect("body renders");

        assert_eq!(
            body["system"],
            json!([{
                "type": "text",
                "text": "stable rules",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h",
                },
            }])
        );
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["thinking"]["budget_tokens"], THINKING_BUDGET_TOKENS);
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert_eq!(body["tools"][0]["name"], "read");
    }

    #[test]
    fn anthropic_parser_preserves_thinking_and_tool_blocks() {
        let response = json!({
            "content": [
                { "type": "thinking", "thinking": "private", "signature": "sig" },
                { "type": "redacted_thinking", "data": "opaque" },
                { "type": "text", "text": "hello" },
                { "type": "tool_use", "id": "toolu_1", "name": "read", "input": { "path": "README.md" } }
            ]
        });

        let response = parse_anthropic_message(&response).expect("message parses");
        let assistant = response.assistant;

        assert_eq!(assistant.text(), "hello");
        let calls = assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "toolu_1");
        assert_eq!(calls[0].tool_name, "read");
        assert_eq!(response.provider_replay.len(), 4);
        assert_eq!(response.provider_replay[0].provider, ProviderKind::Claude);
        assert_eq!(
            response.provider_replay[0].raw_type().as_deref(),
            Some("thinking")
        );
        assert_eq!(
            response.provider_replay[1].raw_type().as_deref(),
            Some("redacted_thinking")
        );
        assert_eq!(
            response.provider_replay[3].raw_type().as_deref(),
            Some("tool_use")
        );
    }

    #[test]
    fn anthropic_serializer_prefers_replay_blocks() {
        let raw = json!({ "type": "thinking", "thinking": "private", "signature": "sig" });
        let messages = transcript_to_messages(&[ModelTranscriptEntry {
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("visible".to_string())],
            }),
            provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &raw).unwrap()],
        }])
        .expect("messages render");

        assert_eq!(messages[0]["content"], json!([raw]));
    }
}
