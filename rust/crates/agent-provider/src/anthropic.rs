use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ToolCall, ToolCallId,
    ToolDefinition, TranscriptItem, UserMessage,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{ModelProvider, ModelRequest, ModelResponse, ProviderError, ProviderResult};

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
        let mut body = json!({
            "model": request.model,
            "max_tokens": request.max_tokens.unwrap_or(4096),
            "messages": transcript_to_messages(&request.transcript)?,
        });
        if let Some(system_prompt) = request.prompt.render_joined() {
            body["system"] = json!(system_prompt);
        }
        if !request.tools.is_empty() {
            body["tools"] = Value::Array(request.tools.iter().map(anthropic_tool).collect());
        }

        let response: Value = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        Ok(ModelResponse {
            assistant: parse_anthropic_message(&response)?,
        })
    }
}

fn anthropic_tool(tool: &ToolDefinition) -> Value {
    json!({
        "name": tool.name,
        "description": tool.description,
        "input_schema": tool.input_schema,
    })
}

fn transcript_to_messages(items: &[TranscriptItem]) -> ProviderResult<Vec<Value>> {
    let mut messages = Vec::new();
    for item in items {
        match item {
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
                let mut content = Vec::new();
                for item in &message.items {
                    match item {
                        AssistantItem::Text(text) => {
                            content.push(json!({ "type": "text", "text": text }))
                        }
                        AssistantItem::ThinkingRedacted => {}
                        AssistantItem::ToolCall(call) => content.push(json!({
                            "type": "tool_use",
                            "id": call.id.as_str(),
                            "name": call.tool_name,
                            "input": call.args_value().unwrap_or_else(|_| json!({})),
                        })),
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

fn parse_anthropic_message(response: &Value) -> ProviderResult<AssistantMessage> {
    let content = response
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| ProviderError::Provider("missing content array".to_string()))?;
    let mut items = Vec::new();
    for block in content {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    items.push(AssistantItem::Text(text.to_string()));
                }
            }
            Some("thinking") | Some("redacted_thinking") => {
                items.push(AssistantItem::ThinkingRedacted);
            }
            Some("tool_use") => {
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
    Ok(AssistantMessage { items })
}
