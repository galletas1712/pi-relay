use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ProviderKind,
    ProviderReplayRecord, ToolCall, ToolCallId, TranscriptItem, UserMessage,
};
use async_trait::async_trait;
use reqwest::{
    header::{ACCEPT, ACCEPT_ENCODING},
    StatusCode,
};
use serde_json::{json, Value};

use crate::{ModelProvider, ModelRequest, ModelResponse, ProviderError, ProviderResult};

const DEFAULT_PROMPT_CACHE_KEY: &str = "pi-relay-openai-responses";
const RESPONSES_REASONING_INCLUDE: &str = "reasoning.encrypted_content";
const EXTENDED_PROMPT_CACHE_RETENTION: &str = "24h";
const CODEX_RESIDENCY_HEADER: &str = "x-openai-internal-codex-residency";
const CODEX_RESIDENCY_US: &str = "us";

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    auth: OpenAiAuth,
    base_url: String,
}

#[derive(Debug, Clone)]
enum OpenAiAuth {
    ApiKey(String),
    Codex {
        access_token: String,
        account_id: Option<String>,
    },
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth: OpenAiAuth::ApiKey(api_key.into()),
            base_url: "https://api.openai.com/v1".to_string(),
        }
    }

    pub fn codex(access_token: impl Into<String>, account_id: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth: OpenAiAuth::Codex {
                access_token: access_token.into(),
                account_id,
            },
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_responses_api(self) -> Self {
        self
    }

    fn add_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        let request = request.header(ACCEPT_ENCODING, "identity");
        match &self.auth {
            OpenAiAuth::ApiKey(api_key) => request.bearer_auth(api_key),
            OpenAiAuth::Codex {
                access_token,
                account_id,
            } => {
                let request = request
                    .bearer_auth(access_token)
                    .header(CODEX_RESIDENCY_HEADER, CODEX_RESIDENCY_US);
                if let Some(account_id) = account_id {
                    request.header("ChatGPT-Account-ID", account_id)
                } else {
                    request
                }
            }
        }
    }

    fn replay_provider_kind(&self) -> ProviderKind {
        match self.auth {
            OpenAiAuth::ApiKey(_) => ProviderKind::OpenAi,
            OpenAiAuth::Codex { .. } => ProviderKind::Codex,
        }
    }

    fn supports_extended_prompt_cache_retention(&self) -> bool {
        matches!(self.auth, OpenAiAuth::ApiKey(_))
    }
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        self.complete_responses(request).await
    }
}

impl OpenAiProvider {
    async fn complete_responses(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let body = responses_body(request, self.supports_extended_prompt_cache_retention())?;

        let text = self
            .add_auth(
                self.client
                    .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
                    .header(ACCEPT, "text/event-stream"),
            )
            .json(&body)
            .send()
            .await?;
        let (status, text) = response_text(text).await?;
        ensure_success(status, &text)?;

        Ok(ModelResponse {
            assistant: parse_responses_sse(&text, self.replay_provider_kind())?,
        })
    }
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
                .or_else(|| value.pointer("/detail"))
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

fn responses_body(
    request: ModelRequest,
    include_prompt_cache_retention: bool,
) -> ProviderResult<Value> {
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| DEFAULT_PROMPT_CACHE_KEY.to_string());
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect();
    let mut body = json!({
        "model": request.model,
        "instructions": request.prompt.render_joined().unwrap_or_default(),
        "input": transcript_to_response_items(&request.transcript)?,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": null,
        "service_tier": "priority",
        "store": false,
        "stream": true,
        "include": [RESPONSES_REASONING_INCLUDE],
        "prompt_cache_key": prompt_cache_key,
    });
    if include_prompt_cache_retention {
        body["prompt_cache_retention"] = json!(EXTENDED_PROMPT_CACHE_RETENTION);
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_tokens);
    }
    Ok(body)
}

fn transcript_to_response_items(items: &[TranscriptItem]) -> ProviderResult<Vec<Value>> {
    let mut responses = Vec::new();
    for item in items {
        match item {
            TranscriptItem::UserMessage(message) => {
                responses.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": responses_user_content(message),
                }));
            }
            TranscriptItem::CompactionSummary(summary) => {
                responses.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": compaction_summary_text(summary) }],
                }));
            }
            TranscriptItem::AssistantMessage(message) => {
                let replay_items = openai_replay_items(message)?;
                if !replay_items.is_empty() {
                    responses.extend(replay_items);
                } else {
                    let text = message.text();
                    if !text.is_empty() {
                        responses.push(json!({
                            "type": "message",
                            "role": "assistant",
                            "content": [{ "type": "output_text", "text": text }],
                        }));
                    }
                    for call in message.tool_calls() {
                        responses.push(json!({
                            "type": "function_call",
                            "call_id": call.id.as_str(),
                            "name": call.tool_name,
                            "arguments": call.args_json,
                        }));
                    }
                }
            }
            TranscriptItem::ToolResult(result) => {
                responses.push(json!({
                    "type": "function_call_output",
                    "call_id": result.tool_call_id.as_str(),
                    "output": result.output,
                }));
            }
            TranscriptItem::TurnStarted { .. }
            | TranscriptItem::ToolCallStarted { .. }
            | TranscriptItem::TurnFinished { .. } => {}
        }
    }
    Ok(responses)
}

fn openai_replay_items(message: &AssistantMessage) -> ProviderResult<Vec<Value>> {
    message
        .replay_records()
        .filter(|record| matches!(record.provider, ProviderKind::OpenAi | ProviderKind::Codex))
        .map(|record| record.raw_value().map_err(ProviderError::Json))
        .collect()
}

fn responses_user_content(message: &UserMessage) -> Vec<Value> {
    message
        .content
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => json!({ "type": "input_text", "text": text }),
            ContentBlock::Image { image } => match &image.source {
                agent_vocab::ImageSource::Url(url) => {
                    json!({ "type": "input_image", "image_url": url })
                }
                agent_vocab::ImageSource::Base64(data) => {
                    let url = format!("data:{};base64,{}", image.mime_type, data);
                    json!({ "type": "input_image", "image_url": url })
                }
            },
        })
        .collect()
}

fn compaction_summary_text(summary: &CompactionSummary) -> String {
    format!(
        "The conversation history before this point was compacted into this summary:\n\n{}",
        summary.summary
    )
}

fn parse_responses_sse(text: &str, provider: ProviderKind) -> ProviderResult<AssistantMessage> {
    let mut items = Vec::new();
    for data in sse_data_events(text) {
        let event: Value = serde_json::from_str(data)?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    parse_response_output_item(item, &mut items, provider)?;
                }
            }
            Some("response.failed") => {
                let message = event
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response.failed");
                return Err(ProviderError::Provider(message.to_string()));
            }
            Some("response.incomplete") => {
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown");
                return Err(ProviderError::Provider(format!(
                    "response incomplete: {reason}"
                )));
            }
            _ => {}
        }
    }
    Ok(AssistantMessage { items })
}

fn sse_data_events(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| !line.trim().is_empty() && *line != "[DONE]")
}

fn parse_response_output_item(
    item: &Value,
    items: &mut Vec<AssistantItem>,
    provider: ProviderKind,
) -> ProviderResult<()> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    items.push(AssistantItem::ProviderReplayRecord(
        ProviderReplayRecord::new(provider, &item_type, item)?,
    ));

    match item_type.as_str() {
        "message" => {
            if item.get("role").and_then(Value::as_str) != Some("assistant") {
                return Ok(());
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                items.push(AssistantItem::Text(text.to_string()));
                            }
                        }
                    }
                }
            }
        }
        "function_call" => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .unwrap_or("{}");
            items.push(AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new(call_id),
                tool_name: name.to_string(),
                args_json: arguments.to_string(),
            }));
        }
        "reasoning" | "reasoning_summary" => {}
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptSections;
    use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};

    #[test]
    fn codex_auth_adds_account_and_residency_headers() {
        let provider = OpenAiProvider::codex("access-token", Some("account-id".to_string()));
        let request = provider
            .add_auth(
                provider
                    .client
                    .post("https://chatgpt.com/backend-api/codex/responses"),
            )
            .build()
            .expect("request builds");

        assert_eq!(
            request
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|value| value.to_str().ok()),
            Some("Bearer access-token")
        );
        assert_eq!(
            request
                .headers()
                .get("ChatGPT-Account-ID")
                .and_then(|value| value.to_str().ok()),
            Some("account-id")
        );
        assert_eq!(
            request
                .headers()
                .get(CODEX_RESIDENCY_HEADER)
                .and_then(|value| value.to_str().ok()),
            Some(CODEX_RESIDENCY_US)
        );
    }

    #[test]
    fn responses_body_sets_openai_request_policy() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.1".to_string(),
                prompt: PromptSections::new(
                    Some("static system".to_string()),
                    Some("cwd: /tmp/project".to_string()),
                ),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello"))],
                tools: vec![ToolDefinition {
                    name: "read".to_string(),
                    description: "read a file".to_string(),
                    input_schema: json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        },
                        "required": ["path"]
                    }),
                }],
                max_tokens: Some(2048),
                prompt_cache_key: Some("pi-relay-test".to_string()),
            },
            true,
        )
        .expect("responses body renders");

        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["prompt_cache_key"], "pi-relay-test");
        assert_eq!(
            body["prompt_cache_retention"],
            EXTENDED_PROMPT_CACHE_RETENTION
        );
        assert_eq!(body["include"][0], RESPONSES_REASONING_INCLUDE);
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["instructions"], "static system\n\ncwd: /tmp/project");
        assert_eq!(body["input"][0]["role"], "user");
    }

    #[test]
    fn responses_body_uses_default_cache_key() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.1".to_string(),
                prompt: PromptSections::default(),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello"))],
                tools: Vec::new(),
                max_tokens: None,
                prompt_cache_key: None,
            },
            false,
        )
        .expect("responses body renders");

        assert_eq!(body["prompt_cache_key"], DEFAULT_PROMPT_CACHE_KEY);
        assert!(body.get("prompt_cache_retention").is_none());
        assert_eq!(body["tools"], json!([]));
        assert!(body.get("max_output_tokens").is_none());
    }

    #[test]
    fn responses_body_keeps_stable_prompt_before_dynamic_context() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.1-codex".to_string(),
                prompt: PromptSections::new(
                    Some("stable agent rules".to_string()),
                    Some("workspace: /tmp/pi".to_string()),
                ),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello"))],
                tools: Vec::new(),
                max_tokens: None,
                prompt_cache_key: Some("cache-key".to_string()),
            },
            false,
        )
        .expect("responses body renders");

        assert_eq!(
            body["instructions"],
            "stable agent rules\n\nworkspace: /tmp/pi"
        );
        assert_eq!(body["prompt_cache_key"], "cache-key");
    }

    #[test]
    fn transcript_to_response_items_preserves_assistant_tool_calls() {
        let tool_call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "read".to_string(),
            args_json: "{\"path\":\"README.md\"}".to_string(),
        };
        let items = transcript_to_response_items(&[
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

        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[0]["name"], "read");
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
    }

    #[test]
    fn responses_sse_parses_text_and_tool_calls() {
        let sse = r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}
data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"README.md\"}"}}
data: {"type":"response.completed","response":{"id":"resp_1"}}
"#;

        let assistant = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");

        assert_eq!(assistant.text(), "hello");
        let calls = assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "call_1");
        assert_eq!(calls[0].tool_name, "read");
        let replay = assistant.replay_records().collect::<Vec<_>>();
        assert_eq!(replay.len(), 2);
        assert_eq!(replay[0].record_type, "message");
        assert_eq!(replay[1].record_type, "function_call");
    }

    #[test]
    fn responses_input_prefers_openai_replay_records() {
        let raw = json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hello", "annotations": [] }],
            "status": "completed",
        });
        let items =
            transcript_to_response_items(&[TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ProviderReplayRecord(
                        ProviderReplayRecord::new(ProviderKind::OpenAi, "message", &raw).unwrap(),
                    ),
                    AssistantItem::Text("hello".to_string()),
                ],
            })])
            .expect("responses input renders");

        assert_eq!(items, vec![raw]);
    }

    #[test]
    fn responses_input_preserves_images_and_tool_results() {
        let items = transcript_to_response_items(&[
            TranscriptItem::UserMessage(UserMessage::from_parts(vec![
                ContentBlock::text("look"),
                ContentBlock::Image {
                    image: agent_vocab::ImageContent {
                        mime_type: "image/png".to_string(),
                        source: agent_vocab::ImageSource::Base64("abc".to_string()),
                    },
                },
            ])),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId::new("call_1"),
                "read",
                "contents",
            )),
        ])
        .expect("responses input renders");

        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["content"][0]["type"], "input_text");
        assert_eq!(items[0]["content"][1]["type"], "input_image");
        assert_eq!(
            items[0]["content"][1]["image_url"],
            "data:image/png;base64,abc"
        );
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
    }
}
