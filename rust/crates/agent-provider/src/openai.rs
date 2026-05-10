use agent_vocab::{
    AssistantItem, AssistantMessage, ContentBlock, ToolCall, ToolCallId, TranscriptItem,
    UserMessage,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{ModelProvider, ModelRequest, ModelResponse, ProviderError, ProviderResult};

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let mut messages = Vec::new();
        if let Some(system_prompt) = request.system_prompt {
            messages.push(json!({ "role": "system", "content": system_prompt }));
        }
        messages.extend(transcript_to_messages(&request.transcript)?);

        let tools: Vec<Value> = request
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.input_schema,
                    }
                })
            })
            .collect();

        let mut body = json!({
            "model": request.model,
            "messages": messages,
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools);
        }
        if let Some(max_tokens) = request.max_tokens {
            body["max_tokens"] = json!(max_tokens);
        }

        let response: Value = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;

        let message = response
            .pointer("/choices/0/message")
            .ok_or_else(|| ProviderError::Provider("missing choices[0].message".to_string()))?;
        Ok(ModelResponse {
            assistant: parse_openai_message(message),
        })
    }
}

fn transcript_to_messages(items: &[TranscriptItem]) -> ProviderResult<Vec<Value>> {
    let mut messages = Vec::new();
    for item in items {
        match item {
            TranscriptItem::UserMessage(message) => {
                messages.push(json!({ "role": "user", "content": openai_user_content(message) }));
            }
            TranscriptItem::Injected(message) => {
                messages.push(json!({ "role": "user", "content": message.content }));
            }
            TranscriptItem::AssistantMessage(message) => {
                let text = message.text();
                let tool_calls = message
                    .tool_calls()
                    .map(|call| {
                        json!({
                            "id": call.id.as_str(),
                            "type": "function",
                            "function": {
                                "name": &call.tool_name,
                                "arguments": &call.args_json,
                            }
                        })
                    })
                    .collect::<Vec<_>>();
                if !tool_calls.is_empty() {
                    messages.push(json!({
                        "role": "assistant",
                        "content": if text.is_empty() { Value::Null } else { json!(text) },
                        "tool_calls": tool_calls,
                    }));
                } else if !text.is_empty() {
                    messages.push(json!({ "role": "assistant", "content": text }));
                }
            }
            TranscriptItem::ToolResult(result) => {
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": result.tool_call_id.as_str(),
                    "content": result.output,
                }));
            }
            TranscriptItem::TurnStarted { .. }
            | TranscriptItem::ToolCallStarted { .. }
            | TranscriptItem::TurnFinished { .. } => {}
        }
    }
    Ok(messages)
}

fn openai_user_content(message: &UserMessage) -> Value {
    if let Some(text) = message.as_text() {
        return json!(text);
    }
    Value::Array(
        message
            .content
            .iter()
            .map(|block| match block {
                ContentBlock::Text { text } => json!({ "type": "text", "text": text }),
                ContentBlock::Image { image } => match &image.source {
                    agent_vocab::ImageSource::Url(url) => {
                        json!({ "type": "image_url", "image_url": { "url": url } })
                    }
                    agent_vocab::ImageSource::Base64(data) => {
                        let url = format!("data:{};base64,{}", image.mime_type, data);
                        json!({ "type": "image_url", "image_url": { "url": url } })
                    }
                },
            })
            .collect(),
    )
}

fn parse_openai_message(message: &Value) -> AssistantMessage {
    let mut items = Vec::new();
    if let Some(content) = message.get("content").and_then(Value::as_str) {
        if !content.is_empty() {
            items.push(AssistantItem::Text(content.to_string()));
        }
    }
    if let Some(tool_calls) = message.get("tool_calls").and_then(Value::as_array) {
        for call in tool_calls {
            let Some(function) = call.get("function") else {
                continue;
            };
            let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let args = function
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            items.push(AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new(id),
                tool_name: name.to_string(),
                args_json: args.to_string(),
            }));
        }
    }
    AssistantMessage { items }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCall, ToolResultMessage};

    #[test]
    fn transcript_to_messages_preserves_assistant_tool_calls() {
        let tool_call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "read".to_string(),
            args_json: "{\"path\":\"README.md\"}".to_string(),
        };
        let messages = transcript_to_messages(&[
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call.clone())],
            }),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                tool_call.id,
                "read",
                "contents",
            )),
        ])
        .expect("tool transcript should render");

        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(messages[0]["tool_calls"][0]["function"]["name"], "read");
        assert_eq!(messages[1]["role"], "tool");
        assert_eq!(messages[1]["tool_call_id"], "call_1");
    }
}
