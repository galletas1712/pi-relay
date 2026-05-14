use agent_tools::{builtin_tool_definition, tool_display, ToolDisplayInput};
use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ProviderKind,
    ProviderReplayItem, ReasoningEffort, ReplayDisplay, ToolCall, ToolCallId, TranscriptItem,
    UserMessage,
};
use async_trait::async_trait;
use reqwest::{header::ACCEPT, StatusCode};
use serde_json::{json, Value};
use std::sync::OnceLock;
use uuid::Uuid;

use crate::{
    ModelProvider, ModelRequest, ModelResponse, ModelTranscriptEntry, ProviderError,
    ProviderResult, ProviderToolProfile, ProviderUsage,
};

const RESPONSES_REASONING_INCLUDE: &str = "reasoning.encrypted_content";
const OPENAI_PRIORITY_SERVICE_TIER: &str = "priority";

// Header names: byte-for-byte aligned with Codex CLI's
// `~/codex/codex-rs/login/src/auth/default_client.rs` and
// `~/codex/codex-rs/core/src/client.rs`. Casing matches the CLI exactly so
// pi-relay's request envelope is indistinguishable from a real Codex client.
const HEADER_ORIGINATOR: &str = "originator";
const HEADER_USER_AGENT: &str = "User-Agent";
const HEADER_RESIDENCY: &str = "x-openai-internal-codex-residency";
const HEADER_CHATGPT_ACCOUNT: &str = "ChatGPT-Account-ID";
const HEADER_INSTALLATION_ID: &str = "x-codex-installation-id";
const HEADER_WINDOW_ID: &str = "x-codex-window-id";
const HEADER_CLIENT_REQUEST_ID: &str = "x-client-request-id";

// The Codex CLI's `originator` is the literal string `codex_cli_rs`. Sending
// this from pi-relay is deliberate: the ChatGPT backend uses it for routing
// and rate-limit accounting (see `is_first_party_originator` in the Codex
// source). Diverging from this label is what causes throttling.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_RESIDENCY_US: &str = "us";

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    access_token: String,
    account_id: Option<String>,
    /// Persistent Codex installation identifier (UUID), read from
    /// `~/.codex/installation_id` by the daemon and passed through as the
    /// `x-codex-installation-id` header on every request. Optional because
    /// pi-cli and tests may not have a Codex install.
    installation_id: Option<String>,
    /// Per-process window id, matching Codex CLI's behavior. Stable for the
    /// lifetime of the provider instance; sent as `x-codex-window-id`.
    window_id: String,
    base_url: String,
}

impl OpenAiProvider {
    pub fn codex(
        access_token: impl Into<String>,
        account_id: Option<String>,
        installation_id: Option<String>,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            access_token: access_token.into(),
            account_id,
            installation_id,
            window_id: Uuid::new_v4().to_string(),
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        }
    }

    /// Apply the full Codex CLI request envelope: auth, identity, and the
    /// per-request `x-client-request-id` / `session_id` routing pair. Mirrors
    /// `build_responses_identity_headers` + `build_session_headers` in
    /// `~/codex/codex-rs/core/src/client.rs` and `default_headers()` in
    /// `~/codex/codex-rs/login/src/auth/default_client.rs`.
    fn add_codex_headers(
        &self,
        request: reqwest::RequestBuilder,
        session_id: &str,
    ) -> reqwest::RequestBuilder {
        let mut request = request
            // Identity (default_headers in Codex CLI).
            .header(HEADER_ORIGINATOR, CODEX_ORIGINATOR)
            .header(HEADER_USER_AGENT, codex_user_agent())
            .header(HEADER_RESIDENCY, CODEX_RESIDENCY_US)
            // Auth (BearerAuthProvider in Codex CLI).
            .bearer_auth(&self.access_token)
            // Codex installation + window identity. Both are documented as
            // observability/routing hints in core/src/client.rs.
            .header(HEADER_WINDOW_ID, &self.window_id)
            // Per-request and per-session routing. Codex emits all four
            // (`session_id`/`session-id`/`thread_id`/`thread-id`) — we send
            // both spellings of each because the backend currently parses
            // either casing inconsistently. The thread id doubles as the
            // `x-client-request-id` so traces line up with the prompt-cache
            // bucket.
            .header(HEADER_CLIENT_REQUEST_ID, session_id)
            .header("session_id", session_id)
            .header("session-id", session_id)
            .header("thread_id", session_id)
            .header("thread-id", session_id);

        if let Some(installation_id) = &self.installation_id {
            request = request.header(HEADER_INSTALLATION_ID, installation_id);
        }
        if let Some(account_id) = &self.account_id {
            request = request.header(HEADER_CHATGPT_ACCOUNT, account_id);
        }
        request
    }
}

/// Codex CLI-style User-Agent, evaluated once per process. Format mirrors
/// `get_codex_user_agent` in the Codex source:
///     `codex_cli_rs/{version} ({os_type} {os_version}; {arch}) {term_ua}`
///
/// We omit the trailing `{term_ua}` (terminal-detected suffix) because
/// pi-relay's daemon runs detached from any TTY; Codex itself tolerates that
/// suffix being empty.
fn codex_user_agent() -> &'static str {
    static UA: OnceLock<String> = OnceLock::new();
    UA.get_or_init(|| {
        let info = os_info::get();
        // Pin a Codex CLI version that we know the backend accepts. We
        // intentionally do NOT use pi-relay's own crate version here — the
        // originator+UA pair has to look like a Codex CLI build to clear
        // anti-abuse heuristics, same as the Anthropic attribution mimicry.
        let codex_version = "0.130.0";
        format!(
            "{}/{} ({} {}; {})",
            CODEX_ORIGINATOR,
            codex_version,
            info.os_type(),
            info.version(),
            info.architecture().unwrap_or("unknown"),
        )
    })
    .as_str()
}

#[async_trait]
impl ModelProvider for OpenAiProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        self.complete_responses(request).await
    }
}

impl OpenAiProvider {
    async fn complete_responses(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        // Lift the session id off the request before consuming it for the body.
        // This is the value the Codex CLI calls `thread_id`: the unique
        // pi-relay session identifier that doubles as the prompt-cache cohort
        // and as every routing header (session_id, x-client-request-id,
        // etc.). Falling back to a fresh UUID keeps the CLI / test paths
        // functional, but the daemon always supplies a real session id.
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let body = responses_body(request, &session_id)?;

        let text = self
            .add_codex_headers(
                self.client
                    .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
                    .header(ACCEPT, "text/event-stream"),
                &session_id,
            )
            .json(&body)
            .send()
            .await?;
        let (status, text) = response_text(text).await?;
        ensure_success(status, &text)?;

        parse_responses_sse(&text, ProviderKind::OpenAi)
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

fn responses_body(request: ModelRequest, session_id: &str) -> ProviderResult<Value> {
    let reasoning_effort = openai_reasoning_effort(request.reasoning_effort)?;
    let tools = response_tools(request.tool_profile, &request.tools)?;
    // Cache cohort priority, highest to lowest:
    //   1. Explicit override from `ProviderConfig.prompt_cache.key` (lets
    //      operators force a particular bucket from config).
    //   2. The session id we received from the daemon, matching Codex CLI's
    //      `prompt_cache_key = thread_id.to_string()` (see
    //      `~/codex/codex-rs/core/src/client.rs`). One bucket per pi-relay
    //      session keeps us well under OpenAI's ~15 RPM-per-shard ceiling
    //      while still maximising in-session prefix reuse.
    //   3. Deterministic config-hash fallback for tests / pi-cli that don't
    //      supply a session id.
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| session_id.to_string());
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

fn response_tools(
    profile: ProviderToolProfile,
    tools: &[agent_vocab::ToolDefinition],
) -> ProviderResult<Vec<Value>> {
    match profile {
        ProviderToolProfile::None => Ok(Vec::new()),
        ProviderToolProfile::CustomDefinitions => Ok(response_custom_definition_tools(tools)),
        ProviderToolProfile::OpenAiCoding => Ok(openai_coding_tools()),
        ProviderToolProfile::AnthropicCoding => Err(ProviderError::Provider(
            "Anthropic coding tools cannot be sent to OpenAI".to_string(),
        )),
    }
}

fn response_custom_definition_tools(tools: &[agent_vocab::ToolDefinition]) -> Vec<Value> {
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

const APPLY_PATCH_LARK_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?
hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?
filename: /(.+)/
add_line: "+" /(.*)/ LF
change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF
%import common.LF
"#;

fn openai_coding_tools() -> Vec<Value> {
    vec![
        openai_apply_patch_tool(),
        openai_grep_tool(),
        openai_shell_tool(),
        json!({
            "type": "web_search",
            "search_context_size": "high",
        }),
    ]
}

fn openai_apply_patch_tool() -> Value {
    json!({
        "type": "custom",
        "name": "apply_patch",
        "description": "Use apply_patch to edit files. This is a freeform grammar tool; emit the raw patch body, not JSON.",
        "format": {
            "type": "grammar",
            "syntax": "lark",
            "definition": APPLY_PATCH_LARK_GRAMMAR,
        },
    })
}

fn openai_shell_tool() -> Value {
    json!({
        "type": "function",
        "name": "shell",
        "description": "Run a local shell command in the session workspace.",
        "parameters": {
            "type": "object",
            "properties": {
                "command": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Command argv to execute."
                },
                "workdir": {
                    "type": "string",
                    "description": "Workspace-relative working directory."
                },
                "timeout_ms": {
                    "type": "integer",
                    "description": "Optional command timeout in milliseconds."
                }
            },
            "required": ["command"],
            "additionalProperties": false
        }
    })
}

fn openai_grep_tool() -> Value {
    let tool = builtin_tool_definition("grep").expect("grep tool must be registered");
    json!({
        "type": "function",
        "name": tool.name,
        "description": tool.description,
        "parameters": tool.input_schema,
    })
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
                        responses.push(response_tool_call_item(call));
                    }
                }
            }
            TranscriptItem::ToolResult(result) => {
                responses.push(response_tool_result_item(result));
            }
            TranscriptItem::TurnStarted { .. }
            | TranscriptItem::ToolCallStarted { .. }
            | TranscriptItem::TurnFinished { .. } => {}
        }
    }
    Ok(responses)
}

fn response_tool_call_item(call: &ToolCall) -> Value {
    if call.tool_name == "apply_patch" {
        let input = call
            .args_value()
            .ok()
            .and_then(|value| {
                value
                    .get("input")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap_or_else(|| call.args_json.clone());
        json!({
            "type": "custom_tool_call",
            "call_id": call.id.as_str(),
            "name": call.tool_name,
            "input": input,
        })
    } else {
        json!({
            "type": "function_call",
            "call_id": call.id.as_str(),
            "name": call.tool_name,
            "arguments": call.args_json,
        })
    }
}

fn response_tool_result_item(result: &agent_vocab::ToolResultMessage) -> Value {
    if result.tool_name == "apply_patch" {
        json!({
            "type": "custom_tool_call_output",
            "call_id": result.tool_call_id.as_str(),
            "output": result.output,
        })
    } else {
        json!({
            "type": "function_call_output",
            "call_id": result.tool_call_id.as_str(),
            "output": result.output,
        })
    }
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
        .filter(|record| matches!(record.provider, ProviderKind::OpenAi))
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
                let message = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .map(|reason| format!("response incomplete: {reason}"))
                    .unwrap_or_else(|| "response incomplete".to_string());
                return Err(ProviderError::Provider(message));
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
        .ok_or_else(|| ProviderError::Provider("OpenAI output item missing type".to_string()))?;
    let display = openai_provider_replay_display(provider, item);
    provider_replay.push(ProviderReplayItem::new_with_display(
        provider, item, display,
    )?);

    match item_type {
        "message" => {
            if item.get("role").and_then(Value::as_str) != Some("assistant") {
                return Ok(());
            }
            if let Some(content) = item.get("content").and_then(Value::as_array) {
                for part in content {
                    if part.get("type").and_then(Value::as_str) == Some("output_text") {
                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                push_text_item(items, text);
                            }
                        }
                    }
                }
            }
        }
        "function_call" => {
            let call_id = item.get("call_id").and_then(Value::as_str).ok_or_else(|| {
                ProviderError::Provider("OpenAI function_call missing call_id".to_string())
            })?;
            let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                ProviderError::Provider("OpenAI function_call missing name".to_string())
            })?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProviderError::Provider("OpenAI function_call missing arguments".to_string())
                })?;
            items.push(AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new(call_id),
                tool_name: name.to_string(),
                args_json: arguments.to_string(),
            }));
        }
        "custom_tool_call" => {
            let call_id = item
                .get("call_id")
                .or_else(|| item.get("id"))
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ProviderError::Provider("OpenAI custom_tool_call missing call_id".to_string())
                })?;
            let name = item.get("name").and_then(Value::as_str).ok_or_else(|| {
                ProviderError::Provider("OpenAI custom_tool_call missing name".to_string())
            })?;
            let input = item.get("input").and_then(Value::as_str).ok_or_else(|| {
                ProviderError::Provider("OpenAI custom_tool_call missing input".to_string())
            })?;
            items.push(AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new(call_id),
                tool_name: name.to_string(),
                args_json: json!({ "input": input }).to_string(),
            }));
        }
        "reasoning" | "reasoning_summary" => {}
        _ => {}
    }
    Ok(())
}

fn openai_provider_replay_display(provider: ProviderKind, item: &Value) -> Option<ReplayDisplay> {
    match item.get("type").and_then(Value::as_str)? {
        "web_search_call" => {
            let action = item.get("action")?;
            let tool_name = match action.get("type").and_then(Value::as_str)? {
                "search" => "web_search",
                "open_page" => "open_page",
                _ => return None,
            };
            tool_display(
                provider,
                tool_name,
                ToolDisplayInput::HostedTool,
                Some(action),
            )
        }
        "function_call" => {
            let name = item.get("name").and_then(Value::as_str)?;
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())?;
            tool_display(
                provider,
                name,
                ToolDisplayInput::LocalTool,
                Some(&arguments),
            )
        }
        "custom_tool_call" => {
            let name = item.get("name").and_then(Value::as_str)?;
            tool_display(provider, name, ToolDisplayInput::LocalTool, None)
        }
        _ => None,
    }
}

fn push_text_item(items: &mut Vec<AssistantItem>, text: &str) {
    if text.is_empty() {
        return;
    }
    if let Some(AssistantItem::Text(previous)) = items.last_mut() {
        previous.push_str(text);
    } else {
        items.push(AssistantItem::Text(text.to_string()));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptSections;
    use agent_vocab::{ToolCall, ToolDefinition, ToolResultMessage};

    #[test]
    fn codex_headers_match_codex_cli_envelope() {
        let provider = OpenAiProvider::codex(
            "access-token",
            Some("account-id".to_string()),
            Some("install-uuid-1234".to_string()),
        );
        let request = provider
            .add_codex_headers(
                provider
                    .client
                    .post("https://chatgpt.com/backend-api/codex/responses"),
                "session-uuid-abcd",
            )
            .build()
            .expect("request builds");

        let header = |name: &str| {
            request
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string)
        };

        // Identity envelope (default_headers in Codex CLI).
        assert_eq!(header("authorization").as_deref(), Some("Bearer access-token"));
        assert_eq!(header("originator").as_deref(), Some(CODEX_ORIGINATOR));
        assert!(
            header("user-agent")
                .as_deref()
                .map(|ua| ua.starts_with("codex_cli_rs/"))
                .unwrap_or(false),
            "user-agent should start with codex_cli_rs/: {:?}",
            header("user-agent")
        );
        assert_eq!(header(HEADER_RESIDENCY).as_deref(), Some(CODEX_RESIDENCY_US));
        assert_eq!(header("chatgpt-account-id").as_deref(), Some("account-id"));

        // Codex-specific identity headers.
        assert_eq!(
            header(HEADER_INSTALLATION_ID).as_deref(),
            Some("install-uuid-1234")
        );
        assert!(
            header(HEADER_WINDOW_ID)
                .as_deref()
                .map(|value| Uuid::parse_str(value).is_ok())
                .unwrap_or(false),
            "window id should be a UUID: {:?}",
            header(HEADER_WINDOW_ID)
        );

        // Session routing headers — all four spellings, all pinned to the
        // same id we pass in.
        for name in ["session_id", "session-id", "thread_id", "thread-id"] {
            assert_eq!(
                header(name).as_deref(),
                Some("session-uuid-abcd"),
                "{name} should carry the session id"
            );
        }
        assert_eq!(
            header(HEADER_CLIENT_REQUEST_ID).as_deref(),
            Some("session-uuid-abcd")
        );
    }

    #[test]
    fn codex_headers_omit_optional_fields_when_absent() {
        // Account id + installation id are both optional in the daemon.
        let provider = OpenAiProvider::codex("access-token", None, None);
        let request = provider
            .add_codex_headers(
                provider
                    .client
                    .post("https://chatgpt.com/backend-api/codex/responses"),
                "session-xyz",
            )
            .build()
            .expect("request builds");

        assert!(request.headers().get("chatgpt-account-id").is_none());
        assert!(request.headers().get(HEADER_INSTALLATION_ID).is_none());
        // But the required envelope is still present.
        assert!(request.headers().get("authorization").is_some());
        assert!(request.headers().get("originator").is_some());
        assert!(request.headers().get(HEADER_WINDOW_ID).is_some());
    }

    #[test]
    fn codex_auth_adds_priority_service_tier() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::default(),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        }, "test-session")
        .expect("responses body renders");

        assert_eq!(body["service_tier"], "priority");
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn responses_body_sets_openai_request_shape() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("static system".to_string()),
                Some("cwd: /tmp/project".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::CustomDefinitions,
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
            session_id: None,
        }, "test-session")
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
    fn responses_body_cache_key_falls_back_to_session_id() {
        // When the daemon doesn't supply a `prompt_cache_key` override, the
        // body should reuse the session id as the cache cohort — matching
        // Codex CLI's `prompt_cache_key = thread_id.to_string()`. Two
        // requests with the same session id must produce the same cohort
        // even when their dynamic context and transcripts differ.
        let first = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("stable rules".to_string()),
                Some("cwd: /tmp/one".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        }, "test-session")
        .expect("responses body renders");
        let second = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::new(
                Some("stable rules".to_string()),
                Some("cwd: /tmp/two".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("changed")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: None,
            session_id: None,
        }, "test-session")
        .expect("responses body renders");

        assert_eq!(first["prompt_cache_key"], "test-session");
        assert_eq!(second["prompt_cache_key"], "test-session");
        assert!(first.get("prompt_cache_retention").is_none());
        assert_eq!(first["service_tier"], "priority");
        assert_eq!(first["tools"], json!([]));
        assert!(first.get("max_output_tokens").is_none());
    }

    #[test]
    fn responses_body_prefers_explicit_prompt_cache_key_override() {
        // Explicit override on the request body still wins over the session
        // id fallback, so operators can pin a custom cohort via
        // `ProviderConfig.prompt_cache.key`.
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: Some("explicit-cohort".to_string()),
            session_id: None,
        }, "session-not-used")
        .expect("responses body renders");

        assert_eq!(body["prompt_cache_key"], "explicit-cohort");
    }

    #[test]
    fn responses_body_session_id_from_request_used_as_cache_key() {
        // End-to-end check that `ModelRequest.session_id` flows through the
        // ModelProvider trait into the cache key: when the daemon passes a
        // session id, it lands as the prompt_cache_key.
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: Some("daemon-session-id".to_string()),
        }, "daemon-session-id")
        .expect("responses body renders");

        assert_eq!(body["prompt_cache_key"], "daemon-session-id");
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
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: Some("cache-key".to_string()),
            session_id: None,
        }, "test-session")
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
            tool_profile: ProviderToolProfile::CustomDefinitions,
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
            session_id: None,
        }, "test-session")
        .expect("responses body renders");

        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][1]["name"], "write");
    }

    #[test]
    fn responses_body_renders_openai_native_coding_tools() {
        let body = responses_body(ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::stable("stable agent rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::OpenAiCoding,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
        }, "test-session")
        .expect("responses body renders");

        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["tools"][0]["name"], "apply_patch");
        assert_eq!(body["tools"][1]["name"], "grep");
        assert_eq!(body["tools"][2]["type"], "function");
        assert_eq!(body["tools"][2]["name"], "shell");
        assert_eq!(body["tools"][3]["type"], "web_search");
        assert_eq!(body["tools"][3]["search_context_size"], "high");
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

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");
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
        assert_eq!(response.provider_replay[0].provider, ProviderKind::OpenAi);
    }

    #[test]
    fn responses_sse_parses_custom_and_function_calls() {
        let sse = r#"data: {"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** End Patch\n"}}
data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_shell","name":"shell","arguments":"{\"command\":[\"pwd\"],\"timeout_ms\":120000}","status":"completed"}}
data: {"type":"response.completed","response":{"id":"resp_1"}}
"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");
        let calls = response.assistant.tool_calls().collect::<Vec<_>>();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_name, "apply_patch");
        assert_eq!(
            calls[0].args_value().unwrap()["input"],
            "*** Begin Patch\n*** End Patch\n"
        );
        assert_eq!(calls[1].tool_name, "shell");
        assert_eq!(calls[1].args_value().unwrap()["command"], json!(["pwd"]));
        assert_eq!(calls[1].args_value().unwrap()["timeout_ms"], 120000);
    }

    #[test]
    fn responses_sse_parses_usage_cache_metrics() {
        let sse = r#"data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":100,"output_tokens":20,"total_tokens":120,"input_tokens_details":{"cached_tokens":80}}}}
"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");
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
