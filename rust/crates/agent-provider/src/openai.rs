use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ProviderKind,
    ProviderReplayItem, ReasoningEffort, ToolCall, ToolCallId, TranscriptItem, UserMessage,
};
use async_trait::async_trait;
use reqwest::{
    header::{ACCEPT, ACCEPT_ENCODING},
    StatusCode,
};
use serde_json::{json, Value};

use crate::{
    ModelProvider, ModelRequest, ModelResponse, ModelTranscriptEntry, ProviderError,
    ProviderResult, ProviderUsage,
};

const RESPONSES_REASONING_INCLUDE: &str = "reasoning.encrypted_content";
const OPENAI_PRIORITY_SERVICE_TIER: &str = "priority";
const CODEX_RESIDENCY_HEADER: &str = "x-openai-internal-codex-residency";
const CODEX_RESIDENCY_US: &str = "us";

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    access_token: String,
    account_id: Option<String>,
    base_url: String,
}

impl OpenAiProvider {
    pub fn codex(access_token: impl Into<String>, account_id: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            access_token: access_token.into(),
            account_id,
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
        let request = request
            .header(ACCEPT_ENCODING, "identity")
            .bearer_auth(&self.access_token)
            .header(CODEX_RESIDENCY_HEADER, CODEX_RESIDENCY_US);
        if let Some(account_id) = &self.account_id {
            request.header("ChatGPT-Account-ID", account_id)
        } else {
            request
        }
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
        let body = responses_body(request)?;

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

        parse_responses_sse(&text, ProviderKind::Codex)
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

fn responses_body(request: ModelRequest) -> ProviderResult<Value> {
    let reasoning_effort = openai_reasoning_effort(request.reasoning_effort)?;
    let tools = response_tools(&request.tools);
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| default_prompt_cache_key(&request.model, &request.prompt, &tools));
    let body = json!({
        "model": request.model,
        "instructions": request.prompt.stable_prefix.clone().unwrap_or_default(),
        "input": response_input_items(request.prompt.dynamic_context.as_deref(), &request.transcript)?,
        "tools": tools,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {
            "effort": reasoning_effort,
        },
        "store": false,
        "stream": true,
        "include": [RESPONSES_REASONING_INCLUDE],
        "prompt_cache_key": prompt_cache_key,
        "service_tier": OPENAI_PRIORITY_SERVICE_TIER,
    });
    Ok(body)
}

fn response_tools(tools: &[agent_vocab::ToolDefinition]) -> Vec<Value> {
    let mut tools = tools.to_vec();
    tools.sort_by(|left, right| left.name.cmp(&right.name));
    tools
        .iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.input_schema,
            })
        })
        .collect()
}

fn default_prompt_cache_key(
    model: &str,
    prompt: &crate::PromptSections,
    tools: &[Value],
) -> String {
    let mut hasher = StableHasher::new();
    hasher.write_str("pi-relay:codex-responses:v1");
    hasher.write_str(model);
    hasher.write_str(prompt.stable_prefix.as_deref().unwrap_or_default());
    hasher.write_str(&serde_json::to_string(tools).unwrap_or_default());
    format!("pi-relay-codex-{:016x}", hasher.finish())
}

fn openai_reasoning_effort(effort: ReasoningEffort) -> ProviderResult<&'static str> {
    match effort {
        ReasoningEffort::None
        | ReasoningEffort::Minimal
        | ReasoningEffort::Low
        | ReasoningEffort::Medium
        | ReasoningEffort::High
        | ReasoningEffort::XHigh => Ok(effort.as_str()),
        ReasoningEffort::Max => Err(ProviderError::Provider(
            "reasoning effort max is not supported by OpenAI".to_string(),
        )),
    }
}

fn transcript_to_response_items(items: &[ModelTranscriptEntry]) -> ProviderResult<Vec<Value>> {
    let mut responses = Vec::new();
    for entry in items {
        match &entry.item {
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
                let replay_items = openai_replay_items(&entry.provider_replay)?;
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

fn response_input_items(
    dynamic_context: Option<&str>,
    transcript: &[ModelTranscriptEntry],
) -> ProviderResult<Vec<Value>> {
    let mut items = Vec::new();
    if let Some(dynamic_context) = dynamic_context.filter(|value| !value.trim().is_empty()) {
        items.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": dynamic_context }],
        }));
    }
    items.extend(transcript_to_response_items(transcript)?);
    Ok(items)
}

fn openai_replay_items(replay: &[ProviderReplayItem]) -> ProviderResult<Vec<Value>> {
    replay
        .iter()
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

fn parse_responses_sse(text: &str, provider: ProviderKind) -> ProviderResult<ModelResponse> {
    let mut items = Vec::new();
    let mut provider_replay = Vec::new();
    let mut usage = None;
    for data in sse_data_events(text) {
        let event: Value = serde_json::from_str(data)?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    parse_response_output_item(item, &mut items, &mut provider_replay, provider)?;
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
            Some("response.completed") => {
                usage = event.pointer("/response/usage").and_then(openai_usage);
            }
            _ => {}
        }
    }
    Ok(ModelResponse {
        assistant: AssistantMessage { items },
        provider_replay,
        usage,
    })
}

fn sse_data_events(text: &str) -> impl Iterator<Item = &str> {
    text.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .filter(|line| !line.trim().is_empty() && *line != "[DONE]")
}

fn parse_response_output_item(
    item: &Value,
    items: &mut Vec<AssistantItem>,
    provider_replay: &mut Vec<ProviderReplayItem>,
    provider: ProviderKind,
) -> ProviderResult<()> {
    let item_type = item
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    provider_replay.push(ProviderReplayItem::new(provider, item)?);

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

fn openai_usage(value: &Value) -> Option<ProviderUsage> {
    Some(ProviderUsage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        total_tokens: value
            .get("total_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        cache_read_input_tokens: value
            .pointer("/input_tokens_details/cached_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        cache_creation_input_tokens: None,
    })
}

struct StableHasher(u64);

impl StableHasher {
    fn new() -> Self {
        Self(0xcbf29ce484222325)
    }

    fn write_str(&mut self, value: &str) {
        self.write_usize(value.len());
        for byte in value.as_bytes() {
            self.0 ^= u64::from(*byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn write_usize(&mut self, value: usize) {
        for byte in value.to_le_bytes() {
            self.0 ^= u64::from(byte);
            self.0 = self.0.wrapping_mul(0x100000001b3);
        }
    }

    fn finish(self) -> u64 {
        self.0
    }
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
    fn codex_auth_adds_priority_service_tier() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::default(),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
        })
        .expect("responses body renders");

        assert_eq!(body["service_tier"], "priority");
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn responses_body_sets_codex_request_shape() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("static system".to_string()),
                Some("cwd: /tmp/project".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
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
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: Some("pi-relay-test".to_string()),
        })
        .expect("responses body renders");

        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["store"], false);
        assert_eq!(body["stream"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["prompt_cache_key"], "pi-relay-test");
        assert!(body.get("prompt_cache_retention").is_none());
        assert_eq!(body["include"][0], RESPONSES_REASONING_INCLUDE);
        assert_eq!(body["tool_choice"], "auto");
        assert!(body.get("max_output_tokens").is_none());
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["instructions"], "static system");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "cwd: /tmp/project");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["text"], "hello");
    }

    #[test]
    fn responses_body_uses_stable_default_cache_key() {
        let first = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("stable rules".to_string()),
                Some("cwd: /tmp/one".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
        })
        .expect("responses body renders");
        let second = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("stable rules".to_string()),
                Some("cwd: /tmp/two".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("changed")).into()],
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: None,
        })
        .expect("responses body renders");

        let key = first["prompt_cache_key"].as_str().expect("cache key");
        assert!(key.starts_with("pi-relay-codex-"));
        assert_eq!(first["prompt_cache_key"], second["prompt_cache_key"]);
        assert!(first.get("prompt_cache_retention").is_none());
        assert_eq!(first["service_tier"], "priority");
        assert_eq!(first["tools"], json!([]));
        assert!(first.get("max_output_tokens").is_none());
    }

    #[test]
    fn responses_body_keeps_dynamic_context_out_of_instructions() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("stable agent rules".to_string()),
                Some("workspace: /tmp/pi".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: Some("cache-key".to_string()),
        })
        .expect("responses body renders");

        assert_eq!(body["instructions"], "stable agent rules");
        assert_eq!(body["input"][0]["content"][0]["text"], "workspace: /tmp/pi");
        assert_eq!(body["input"][1]["content"][0]["text"], "hello");
        assert_eq!(body["prompt_cache_key"], "cache-key");
    }

    #[test]
    fn responses_body_sorts_tools_for_cache_stability() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::stable("stable agent rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tools: vec![
                ToolDefinition {
                    name: "write".to_string(),
                    description: "write a file".to_string(),
                    input_schema: json!({ "type": "object" }),
                },
                ToolDefinition {
                    name: "read".to_string(),
                    description: "read a file".to_string(),
                    input_schema: json!({ "type": "object" }),
                },
            ],
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
        })
        .expect("responses body renders");

        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][1]["name"], "write");
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
            })
            .into(),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                tool_call.id,
                "read",
                "contents",
            ))
            .into(),
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

        let response = parse_responses_sse(sse, ProviderKind::Codex).expect("sse parses");
        let assistant = response.assistant;

        assert_eq!(assistant.text(), "hello");
        let calls = assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "call_1");
        assert_eq!(calls[0].tool_name, "read");
        assert_eq!(response.provider_replay.len(), 2);
        assert_eq!(
            response.provider_replay[0].raw_type().as_deref(),
            Some("message")
        );
        assert_eq!(
            response.provider_replay[1].raw_type().as_deref(),
            Some("function_call")
        );
        assert_eq!(response.provider_replay[0].provider, ProviderKind::Codex);
    }

    #[test]
    fn responses_sse_parses_usage_cache_metrics() {
        let sse = r#"data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":80}}}}
"#;

        let response = parse_responses_sse(sse, ProviderKind::Codex).expect("sse parses");
        let usage = response.usage.expect("usage should be parsed");

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(120));
        assert_eq!(usage.cache_read_input_tokens, Some(80));
        assert_eq!(usage.cache_creation_input_tokens, None);
    }

    #[test]
    fn responses_input_prefers_openai_replay_sidecar() {
        let raw = json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hello", "annotations": [] }],
            "status": "completed",
        });
        let items = transcript_to_response_items(&[ModelTranscriptEntry {
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("hello".to_string())],
            }),
            provider_replay: vec![ProviderReplayItem::new(ProviderKind::OpenAi, &raw).unwrap()],
        }])
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
            ]))
            .into(),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId::new("call_1"),
                "read",
                "contents",
            ))
            .into(),
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
