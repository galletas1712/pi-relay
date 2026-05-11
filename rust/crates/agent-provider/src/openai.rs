use agent_vocab::{
    AssistantItem, AssistantMessage, ContentBlock, ToolCall, ToolCallId, TranscriptItem,
    UserMessage,
};
use async_trait::async_trait;
use serde_json::{json, Value};

use crate::{
    ModelProvider, ModelRequest, ModelResponse, PromptSections, ProviderError, ProviderResult,
};

const DEFAULT_PROMPT_CACHE_KEY: &str = "pi-relay-openai-chat";
const EXTENDED_PROMPT_CACHE_RETENTION: &str = "24h";
const CODEX_RESIDENCY_HEADER: &str = "x-openai-internal-codex-residency";
const CODEX_RESIDENCY_US: &str = "us";

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    auth: OpenAiAuth,
    base_url: String,
    wire_api: OpenAiWireApi,
}

#[derive(Debug, Clone)]
enum OpenAiAuth {
    ApiKey(String),
    Codex {
        access_token: String,
        account_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy)]
enum OpenAiWireApi {
    ChatCompletions,
    Responses,
}

impl OpenAiProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            auth: OpenAiAuth::ApiKey(api_key.into()),
            base_url: "https://api.openai.com/v1".to_string(),
            wire_api: OpenAiWireApi::ChatCompletions,
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
            wire_api: OpenAiWireApi::Responses,
        }
    }

    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    pub fn with_responses_api(mut self) -> Self {
        self.wire_api = OpenAiWireApi::Responses;
        self
    }

    fn add_auth(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
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
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        match self.wire_api {
            OpenAiWireApi::ChatCompletions => self.complete_chat(request).await,
            OpenAiWireApi::Responses => self.complete_responses(request).await,
        }
    }
}

impl OpenAiProvider {
    async fn complete_chat(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let body = chat_completion_body(request)?;

        let response: Value = self
            .add_auth(self.client.post(format!(
                "{}/chat/completions",
                self.base_url.trim_end_matches('/')
            )))
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

    async fn complete_responses(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let body = responses_body(request)?;

        let text = self
            .add_auth(
                self.client
                    .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
                    .header(reqwest::header::ACCEPT, "text/event-stream"),
            )
            .json(&body)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;

        Ok(ModelResponse {
            assistant: parse_responses_sse(&text)?,
        })
    }
}

fn chat_completion_body(request: ModelRequest) -> ProviderResult<Value> {
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| DEFAULT_PROMPT_CACHE_KEY.to_string());
    let mut messages = Vec::new();
    messages.extend(chat_prompt_messages(&request.prompt));
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
        "parallel_tool_calls": true,
        "prompt_cache_key": prompt_cache_key,
        "prompt_cache_retention": EXTENDED_PROMPT_CACHE_RETENTION,
        "service_tier": "priority",
        "store": false,
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!("auto");
    }
    if let Some(max_tokens) = request.max_tokens {
        body["max_completion_tokens"] = json!(max_tokens);
    }
    Ok(body)
}

fn chat_prompt_messages(prompt: &PromptSections) -> Vec<Value> {
    let mut messages = Vec::new();
    if let Some(stable_prefix) = &prompt.stable_prefix {
        messages.push(json!({ "role": "system", "content": stable_prefix }));
    }
    if let Some(dynamic_context) = &prompt.dynamic_context {
        messages.push(json!({ "role": "system", "content": dynamic_context }));
    }
    messages
}

fn responses_body(request: ModelRequest) -> ProviderResult<Value> {
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
        "store": false,
        "stream": true,
        "include": [],
    });
    if let Some(prompt_cache_key) = request.prompt_cache_key {
        body["prompt_cache_key"] = json!(prompt_cache_key);
    }
    Ok(body)
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
            TranscriptItem::Injected(message) => {
                responses.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": message.content }],
                }));
            }
            TranscriptItem::AssistantMessage(message) => {
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

fn parse_responses_sse(text: &str) -> ProviderResult<AssistantMessage> {
    let mut items = Vec::new();
    for data in sse_data_events(text) {
        let event: Value = serde_json::from_str(data)?;
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    parse_response_output_item(item, &mut items)?;
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

fn parse_response_output_item(item: &Value, items: &mut Vec<AssistantItem>) -> ProviderResult<()> {
    match item.get("type").and_then(Value::as_str) {
        Some("message") => {
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
        Some("function_call") => {
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
        Some("reasoning") | Some("reasoning_summary") => {
            items.push(AssistantItem::ThinkingRedacted);
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};

    #[test]
    fn codex_auth_adds_account_and_residency_headers() {
        let provider = OpenAiProvider::codex("access-token", Some("account-id".to_string()));
        let request = provider
            .add_auth(provider.client.post("https://chatgpt.com/backend-api/codex/responses"))
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
    fn chat_completion_body_sets_openai_request_policy() {
        let body = chat_completion_body(ModelRequest {
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
        })
        .expect("chat body renders");

        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["service_tier"], "priority");
        assert_eq!(body["store"], false);
        assert_eq!(body["prompt_cache_key"], "pi-relay-test");
        assert_eq!(body["prompt_cache_retention"], "24h");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["max_completion_tokens"], 2048);
        assert_eq!(body["tools"][0]["function"]["name"], "read");
        assert_eq!(body["messages"][0]["content"], "static system");
        assert_eq!(body["messages"][1]["content"], "cwd: /tmp/project");
        assert_eq!(body["messages"][2]["role"], "user");
    }

    #[test]
    fn chat_completion_body_uses_default_cache_key() {
        let body = chat_completion_body(ModelRequest {
            model: "gpt-5.1".to_string(),
            prompt: PromptSections::default(),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello"))],
            tools: Vec::new(),
            max_tokens: None,
            prompt_cache_key: None,
        })
        .expect("chat body renders");

        assert_eq!(body["prompt_cache_key"], DEFAULT_PROMPT_CACHE_KEY);
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("max_completion_tokens").is_none());
    }

    #[test]
    fn responses_body_keeps_stable_prompt_before_dynamic_context() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.1-codex".to_string(),
            prompt: PromptSections::new(
                Some("stable agent rules".to_string()),
                Some("workspace: /tmp/pi".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello"))],
            tools: Vec::new(),
            max_tokens: None,
            prompt_cache_key: Some("cache-key".to_string()),
        })
        .expect("responses body renders");

        assert_eq!(
            body["instructions"],
            "stable agent rules\n\nworkspace: /tmp/pi"
        );
        assert_eq!(body["prompt_cache_key"], "cache-key");
    }

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

    #[test]
    fn responses_sse_parses_text_and_tool_calls() {
        let sse = r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"hello"}]}}
data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"README.md\"}"}}
data: {"type":"response.completed","response":{"id":"resp_1"}}
"#;

        let assistant = parse_responses_sse(sse).expect("sse parses");

        assert_eq!(assistant.text(), "hello");
        let calls = assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "call_1");
        assert_eq!(calls[0].tool_name, "read");
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
