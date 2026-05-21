use agent_tools::{tool_display, ProviderTool, ToolDisplayInput};
use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ProviderKind,
    ProviderReplayItem, ReasoningEffort, ReplayDisplay, ToolCall, ToolCallId, TranscriptItem,
    UserMessage,
};
use async_trait::async_trait;
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::{
    ModelProvider, ModelRequest, ModelResponse, ModelStopReason, ModelTranscriptEntry,
    ProviderError, ProviderResult, ProviderTokenCountRequest, ProviderTokenCountResponse,
    ProviderToolProfile, ProviderUsage,
};

const DEFAULT_MAX_TOKENS: u32 = 64_000;
const BASE_ANTHROPIC_BETA_HEADER: &str =
    "claude-code-20250219,fine-grained-tool-streaming-2025-05-14,extended-cache-ttl-2025-04-11,web-fetch-2025-09-10";
const CONTEXT_MANAGEMENT_BETA: &str = "context-management-2025-06-27";
const EFFORT_BETA: &str = "effort-2025-11-24";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const CLAUDE_CODE_VERSION: &str = "2.1.75";
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.75 (external, cli)";
const ATTRIBUTION_FINGERPRINT_SALT: &str = "59cf53e54c78";

// Anthropic's documented per-breakpoint backward lookback when matching a new
// request against existing cache entries. We use this to decide when the tail
// cache breakpoint alone can no longer cover the whole transcript history and a
// second deeper breakpoint is worth spending a slot on. Keep a small slack
// (18 vs 20) so the deep breakpoint stays inside the tail breakpoint's lookback
// window even after the conversation grows by a couple of blocks per turn.
//
// See: https://docs.claude.com/en/docs/build-with-claude/prompt-caching
const TRANSCRIPT_LOOKBACK_BLOCKS: usize = 18;

#[derive(Debug, Clone)]
pub struct AnthropicProvider {
    client: reqwest::Client,
    api_key: String,
    base_url: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AnthropicModelCapabilities {
    adaptive_thinking: bool,
    context_management: bool,
    effort: bool,
    interleaved_thinking: bool,
}

fn anthropic_capabilities(model: &str) -> AnthropicModelCapabilities {
    let model = model.to_ascii_lowercase();
    let is_claude_4 = model.contains("claude-")
        && (model.contains("opus-4") || model.contains("sonnet-4") || model.contains("haiku-4"));
    let is_adaptive =
        model.contains("opus-4-6") || model.contains("sonnet-4-6") || model.contains("opus-4-7");
    AnthropicModelCapabilities {
        adaptive_thinking: is_adaptive,
        context_management: is_claude_4,
        effort: is_adaptive,
        interleaved_thinking: is_claude_4 && !is_adaptive,
    }
}

fn anthropic_beta_header(model: &str) -> String {
    let capabilities = anthropic_capabilities(model);
    let mut betas = BASE_ANTHROPIC_BETA_HEADER
        .split(',')
        .map(str::to_string)
        .collect::<Vec<_>>();
    if capabilities.interleaved_thinking {
        betas.push(INTERLEAVED_THINKING_BETA.to_string());
    }
    if capabilities.context_management {
        betas.push(CONTEXT_MANAGEMENT_BETA.to_string());
    }
    if capabilities.effort {
        betas.push(EFFORT_BETA.to_string());
    }
    betas.join(",")
}

fn anthropic_wire_tool_name(canonical_name: &str) -> &str {
    match canonical_name {
        "Edit" => "str_replace_based_edit_tool",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        other => other,
    }
}

fn parse_anthropic_count_tokens(text: &str) -> ProviderResult<ProviderTokenCountResponse> {
    let response: Value = serde_json::from_str(text).map_err(|error| {
        ProviderError::Provider(format!(
            "failed to parse Anthropic count_tokens response JSON: {error}; body: {}",
            response_excerpt(text)
        ))
    })?;
    let input_tokens = response
        .get("input_tokens")
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            ProviderError::Provider(
                "Anthropic count_tokens response missing input_tokens".to_string(),
            )
        })?;
    Ok(ProviderTokenCountResponse {
        input_tokens: input_tokens as usize,
    })
}

impl AnthropicProvider {
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com/v1".to_string(),
        }
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let session_id = request
            .prompt_cache_key
            .clone()
            .unwrap_or_else(|| "pi-relay".to_string());
        let beta_header = anthropic_beta_header(&request.model);
        let body = messages_body(request)?;

        let response = self
            .client
            .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
            .header("accept", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta_header)
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .header("X-Claude-Code-Session-Id", session_id)
            .header("x-client-request-id", client_request_id())
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

    async fn count_tokens(
        &self,
        request: ProviderTokenCountRequest,
    ) -> ProviderResult<ProviderTokenCountResponse> {
        let session_id = request
            .session_id
            .clone()
            .unwrap_or_else(|| "pi-relay".to_string());
        let beta_header = anthropic_beta_header(&request.model);
        let body = count_tokens_body(request)?;

        let response = self
            .client
            .post(format!(
                "{}/messages/count_tokens",
                self.base_url.trim_end_matches('/')
            ))
            .header("accept", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", beta_header)
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .header("X-Claude-Code-Session-Id", session_id)
            .header("x-client-request-id", client_request_id())
            .json(&body)
            .send()
            .await?;
        let (status, text) = response_text(response).await?;
        ensure_success(status, &text)?;
        parse_anthropic_count_tokens(&text)
    }
}

fn messages_body(request: ModelRequest) -> ProviderResult<Value> {
    let tool_profile = request.tool_profile;
    anthropic_request_body(AnthropicRequestBodyInput {
        model: request.model,
        prompt: request.prompt,
        transcript: request.transcript,
        tool_profile,
        tools: crate::effective_provider_tools(tool_profile, request.tools),
        max_tokens: Some(request.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS)),
        reasoning_effort: Some(request.reasoning_effort),
        cache_transcript: true,
    })
}

fn count_tokens_body(request: ProviderTokenCountRequest) -> ProviderResult<Value> {
    // Keep this as close as possible to `messages_body`: Anthropic's token
    // count endpoint accepts the same input-shaping fields (system, tools,
    // thinking/output config) but does not need a generation budget.
    let tool_profile = request.tool_profile;
    anthropic_request_body(AnthropicRequestBodyInput {
        model: request.model,
        prompt: request.prompt,
        transcript: request.transcript,
        tool_profile,
        tools: crate::effective_provider_tools(tool_profile, request.tools),
        max_tokens: request.max_tokens,
        reasoning_effort: Some(request.reasoning_effort),
        cache_transcript: false,
    })
}

struct AnthropicRequestBodyInput {
    model: String,
    prompt: crate::PromptSections,
    transcript: Vec<ModelTranscriptEntry>,
    tool_profile: ProviderToolProfile,
    tools: Vec<ProviderTool>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
    cache_transcript: bool,
}

fn anthropic_request_body(input: AnthropicRequestBodyInput) -> ProviderResult<Value> {
    let capabilities = anthropic_capabilities(&input.model);
    let mut messages = transcript_to_messages(&input.prompt, &input.transcript)?;
    if input.cache_transcript {
        add_transcript_cache_breakpoints(&mut messages);
    }
    let mut body = json!({
        "model": input.model,
        "messages": messages,
    });
    if let Some(max_tokens) = input.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(reasoning_effort) = input
        .reasoning_effort
        .filter(|_| capabilities.adaptive_thinking)
    {
        let effort = anthropic_reasoning_effort(input.model.as_str(), reasoning_effort)?;
        // Adaptive thinking is intentionally hard-coded and must not become a
        // per-request toggle: Anthropic invalidates the message-content cache
        // whenever the `thinking` parameter changes (enabling/disabling or
        // budget changes). Reasoning effort lives in `output_config` instead,
        // which is documented not to affect the messages-level cache.
        // See: https://docs.claude.com/en/docs/build-with-claude/prompt-caching
        body["thinking"] = json!({ "type": "adaptive" });
        body["output_config"] = json!({ "effort": effort });
    }
    if let Some(system_blocks) = anthropic_system_blocks(&input.prompt, &input.transcript) {
        body["system"] = Value::Array(system_blocks);
    }
    let tools = anthropic_tools(input.tool_profile, &input.tools)?;
    if !tools.is_empty() {
        // Intentionally no tool-level `cache_control` breakpoint. Anthropic
        // hashes the cumulative prefix in `tools -> system -> messages` order,
        // so the breakpoint on the stable system block already covers the
        // tools array via the cumulative hash. Spending one of the 4 allowed
        // breakpoints on the last tool would buy zero additional caching and
        // costs us a slot we use for the deep-history transcript marker.
        body["tools"] = Value::Array(tools);
        body["tool_choice"] = json!({ "type": "auto" });
    }
    Ok(body)
}

fn client_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("pi-relay-{nanos}")
}

fn anthropic_reasoning_effort(
    model: &str,
    effort: ReasoningEffort,
) -> ProviderResult<&'static str> {
    match effort {
        ReasoningEffort::Low
        | ReasoningEffort::Medium
        | ReasoningEffort::High
        | ReasoningEffort::Max => Ok(effort.as_str()),
        ReasoningEffort::XHigh if model.to_ascii_lowercase().contains("opus-4-7") => {
            Ok(effort.as_str())
        }
        ReasoningEffort::XHigh => Ok("high"),
        ReasoningEffort::None | ReasoningEffort::Minimal => Err(ProviderError::Provider(
            "reasoning effort is not supported by Claude".to_string(),
        )),
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
            let message = value
                .pointer("/error/message")
                .or_else(|| value.pointer("/message"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)?;
            let error_type = value.pointer("/error/type").and_then(Value::as_str);
            let request_id = value.get("request_id").and_then(Value::as_str);
            Some(match (error_type, request_id) {
                (Some(error_type), Some(request_id)) => {
                    format!("{error_type}: {message} ({request_id})")
                }
                (Some(error_type), None) => format!("{error_type}: {message}"),
                (None, Some(request_id)) => format!("{message} ({request_id})"),
                (None, None) => message,
            })
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

fn anthropic_tools(
    profile: ProviderToolProfile,
    tools: &[ProviderTool],
) -> ProviderResult<Vec<Value>> {
    match profile {
        ProviderToolProfile::None => Ok(Vec::new()),
        ProviderToolProfile::CustomDefinitions | ProviderToolProfile::AnthropicCoding => {
            Ok(anthropic_provider_tools(tools))
        }
        ProviderToolProfile::OpenAiCoding => Err(ProviderError::Provider(
            "OpenAI coding tools cannot be sent to Claude".to_string(),
        )),
    }
}

fn anthropic_provider_tools(tools: &[ProviderTool]) -> Vec<Value> {
    let mut tools = tools.to_vec();
    tools.sort_by(|left, right| {
        left.name
            .to_ascii_lowercase()
            .cmp(&right.name.to_ascii_lowercase())
            .then_with(|| left.name.cmp(&right.name))
            .then_with(|| left.canonical_name.cmp(&right.canonical_name))
    });
    tools.iter().map(|tool| tool.declaration.clone()).collect()
}

/// 1-hour ephemeral cache control. Use only on prefixes that are stable enough
/// to outlive the 5-minute default window — currently the stable system block.
/// 1-hour writes cost 2x base input tokens (vs 1.25x for the 5-minute default),
/// so this is the wrong choice for any breakpoint that is regenerated each turn.
fn cache_control_1h() -> Value {
    json!({
        "type": "ephemeral",
        "ttl": "1h",
    })
}

/// 5-minute ephemeral cache control (Anthropic's default when `ttl` is omitted).
/// Use for short-lived breakpoints like the latest transcript block: these are
/// superseded by the next turn's breakpoint, so paying the 1-hour write
/// premium would be wasted.
fn cache_control_5m() -> Value {
    json!({
        "type": "ephemeral",
    })
}

fn anthropic_system_blocks(
    prompt: &crate::PromptSections,
    transcript: &[ModelTranscriptEntry],
) -> Option<Vec<Value>> {
    let mut blocks = vec![json!({
        "type": "text",
        "text": attribution_header(prompt, transcript),
    })];
    if let Some(stable) = &prompt.stable_prefix {
        blocks.push(json!({
            "type": "text",
            "text": stable,
            "cache_control": cache_control_1h(),
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

fn attribution_header(
    prompt: &crate::PromptSections,
    transcript: &[ModelTranscriptEntry],
) -> String {
    let fingerprint = attribution_fingerprint(prompt, transcript);
    format!(
        "x-anthropic-billing-header: cc_version={CLAUDE_CODE_VERSION}.{fingerprint}; cc_entrypoint=cli;"
    )
}

/// Derive the Claude-Code-style attribution fingerprint.
///
/// We intentionally derive this from the *stable system prompt* rather than
/// the first user message. The attribution header sits at `system[0]`, before
/// the stable-system cache breakpoint, so it is part of the cumulative cache
/// hash. Fingerprinting off the first user message — as Claude Code itself
/// does — would partition the cached system prefix per-conversation: two
/// sessions with identical system prompts but different opening messages would
/// never share the cache entry.
///
/// Deriving from `stable_prefix` instead means every pi-relay session with the
/// same stable system prompt produces the same fingerprint and therefore the
/// same cached prefix, enabling true cross-session reuse of the stable-system
/// cache. We fall back to a digest of the first user text only when no stable
/// prefix is configured (e.g. compaction calls), so the header is never empty.
fn attribution_fingerprint(
    prompt: &crate::PromptSections,
    transcript: &[ModelTranscriptEntry],
) -> String {
    let text = prompt
        .stable_prefix
        .as_deref()
        .or_else(|| first_user_text(transcript))
        .unwrap_or_default();
    let chars = [
        text.chars().nth(4).unwrap_or('0'),
        text.chars().nth(7).unwrap_or('0'),
        text.chars().nth(20).unwrap_or('0'),
    ]
    .iter()
    .collect::<String>();
    let input = format!("{ATTRIBUTION_FINGERPRINT_SALT}{chars}{CLAUDE_CODE_VERSION}");
    let mut hash = 0u32;
    for byte in input.bytes() {
        hash = hash.wrapping_mul(31).wrapping_add(u32::from(byte));
    }
    format!("{hash:08x}").chars().take(3).collect()
}

fn first_user_text(transcript: &[ModelTranscriptEntry]) -> Option<&str> {
    transcript.iter().find_map(|entry| match entry.item() {
        TranscriptItem::UserMessage(message) => message.as_text(),
        _ => None,
    })
}

/// Place message-level cache breakpoints on the transcript.
///
/// Strategy:
/// - Always mark the latest cacheable content block in the most recent message
///   (the "tail" breakpoint). Anthropic's backward lookup will find this on the
///   next turn and use it as the read prefix.
/// - When the transcript has grown past Anthropic's documented ~20-block
///   lookback ceiling, additionally mark a "deep" breakpoint roughly
///   `TRANSCRIPT_LOOKBACK_BLOCKS` content-blocks behind the tail. Without this,
///   long agentic sessions with many tool_use/tool_result blocks will silently
///   stop hitting their older cached prefix once the gap exceeds 20 blocks.
///
/// Both markers use the 5-minute (default) TTL: each is regenerated on the next
/// turn anyway, so the 1-hour write premium (2x base input vs 1.25x) would be
/// pure waste here. The 1-hour TTL is reserved for the stable system block.
fn add_transcript_cache_breakpoints(messages: &mut [Value]) {
    // 1. Tail breakpoint: walk the most recent message backwards and mark the
    //    latest eligible content block.
    let tail_block_index = mark_latest_cacheable_block(messages, cache_control_5m());
    let Some(tail_index) = tail_block_index else {
        return;
    };

    // 2. Deep-history breakpoint: only worth a slot if the total cacheable
    //    block count from the start to (but not including) the tail block is
    //    larger than the lookback window. Otherwise the tail marker's
    //    automatic ~20-block walk already covers the whole prefix.
    let total_cacheable = count_cacheable_blocks_through(messages, tail_index);
    if total_cacheable <= TRANSCRIPT_LOOKBACK_BLOCKS {
        return;
    }
    // Place the deep marker `TRANSCRIPT_LOOKBACK_BLOCKS` cacheable-blocks back
    // from the tail so it stays inside the tail's lookback window while
    // extending coverage to older history.
    let deep_target = total_cacheable.saturating_sub(TRANSCRIPT_LOOKBACK_BLOCKS);
    mark_cacheable_block_at_index(messages, deep_target, cache_control_5m());
}

/// Walk messages in reverse and stamp `cache_control` on the latest cacheable
/// content block. Returns the cumulative index (1-based) of that block in
/// cacheable-block-order from the front, or `None` if nothing was marked.
fn mark_latest_cacheable_block(messages: &mut [Value], cache_control: Value) -> Option<usize> {
    let mut total = 0usize;
    for message in messages.iter() {
        if let Some(content) = message.get("content").and_then(Value::as_array) {
            for block in content {
                if is_cacheable_transcript_block(block) {
                    total += 1;
                }
            }
        }
    }
    if total == 0 {
        return None;
    }
    for message in messages.iter_mut().rev() {
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        let Some(block) = latest_cacheable_content_block(content) else {
            continue;
        };
        if let Some(object) = block.as_object_mut() {
            object.insert("cache_control".to_string(), cache_control);
            return Some(total);
        }
    }
    None
}

/// Count cacheable blocks from the start up to and including the `tail_index`-th
/// cacheable block.
fn count_cacheable_blocks_through(messages: &[Value], tail_index: usize) -> usize {
    // `tail_index` is the count-of-cacheable-blocks up to and including the
    // tail, so the total cacheable blocks is exactly `tail_index`.
    let _ = messages;
    tail_index
}

/// Stamp `cache_control` on the `target`-th cacheable content block (1-based,
/// counted from the start), if it exists and isn't already marked.
fn mark_cacheable_block_at_index(messages: &mut [Value], target: usize, cache_control: Value) {
    if target == 0 {
        return;
    }
    let mut seen = 0usize;
    for message in messages.iter_mut() {
        let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
            continue;
        };
        for block in content.iter_mut() {
            if !is_cacheable_transcript_block(block) {
                continue;
            }
            seen += 1;
            if seen == target {
                if let Some(object) = block.as_object_mut() {
                    object.insert("cache_control".to_string(), cache_control);
                }
                return;
            }
        }
    }
}

fn latest_cacheable_content_block(content: &mut Value) -> Option<&mut Value> {
    let blocks = content.as_array_mut()?;
    blocks
        .iter_mut()
        .rev()
        .find(|block| is_cacheable_transcript_block(block))
}

fn is_cacheable_transcript_block(block: &Value) -> bool {
    let Some(object) = block.as_object() else {
        return false;
    };
    if object.contains_key("cache_control") {
        return false;
    }
    matches!(
        object.get("type").and_then(Value::as_str),
        Some("text" | "tool_use" | "tool_result")
    )
}

fn transcript_to_messages(
    prompt: &crate::PromptSections,
    items: &[ModelTranscriptEntry],
) -> ProviderResult<Vec<Value>> {
    let mut messages = Vec::new();
    for entry in items {
        match entry.item() {
            TranscriptItem::UserMessage(message) => {
                messages
                    .push(json!({ "role": "user", "content": anthropic_user_content(message) }));
            }
            TranscriptItem::CompactionSummary(summary) => {
                messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": compaction_summary_text(summary, prompt) }],
                }));
            }
            TranscriptItem::AssistantMessage(message) => {
                let mut content =
                    anthropic_replay_blocks(&entry.provider_replay_for(ProviderKind::Claude))?;
                if content.is_empty() {
                    for item in &message.items {
                        match item {
                            AssistantItem::Text(text) => {
                                content.push(json!({ "type": "text", "text": text }))
                            }
                            AssistantItem::ToolCall(call) => content.push(json!({
                                "type": "tool_use",
                                "id": call.id.as_str(),
                                "name": anthropic_wire_tool_name(&call.tool_name),
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

fn compaction_summary_text(summary: &CompactionSummary, prompt: &crate::PromptSections) -> String {
    match &prompt.stable_prefix {
        Some(pi_prompt) if !pi_prompt.trim().is_empty() => format!(
            "The active PI.md system prompt is included below because it still applies after this compaction.

{}

The conversation history before this point was compacted into this summary:

{}",
            pi_prompt, summary.summary
        ),
        _ => format!(
            "The conversation history before this point was compacted into this summary:

{}",
            summary.summary
        ),
    }
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
        let display = anthropic_provider_replay_display(block);
        provider_replay.push(ProviderReplayItem::new_with_display(
            ProviderKind::Claude,
            block,
            display,
        )?);

        match block_type {
            "text" => {
                if let Some(text) = block.get("text").and_then(Value::as_str) {
                    push_text_item(&mut items, text);
                }
            }
            "thinking" | "redacted_thinking" => {}
            "tool_use" => {
                let id = block.get("id").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::Provider("Claude tool_use missing id".to_string())
                })?;
                let name = block.get("name").and_then(Value::as_str).ok_or_else(|| {
                    ProviderError::Provider("Claude tool_use missing name".to_string())
                })?;
                let name = canonical_anthropic_tool_name(name);
                let input = block.get("input").cloned().ok_or_else(|| {
                    ProviderError::Provider("Claude tool_use missing input".to_string())
                })?;
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
        usage: response.get("usage").and_then(anthropic_usage),
        stop_reason: anthropic_stop_reason(response),
    })
}

fn anthropic_stop_reason(response: &Value) -> ModelStopReason {
    match response.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => ModelStopReason::MaxOutputTokens,
        _ => ModelStopReason::Complete,
    }
}

fn canonical_anthropic_tool_name(name: &str) -> &str {
    match name {
        // Anthropic currently accepts `name: "Edit"` in the request but still
        // returns its trained native text-editor name in tool_use blocks.
        "str_replace_based_edit_tool" => "Edit",
        // Server tools keep provider-native wire names in the actual Messages
        // request/replay, but pi-relay display and PI.md capabilities use the
        // pretty names.
        "web_search" => "WebSearch",
        "web_fetch" => "WebFetch",
        other => other,
    }
}

fn anthropic_provider_replay_display(block: &Value) -> Option<ReplayDisplay> {
    let name = canonical_anthropic_tool_name(block.get("name").and_then(Value::as_str)?);
    match block.get("type").and_then(Value::as_str)? {
        "server_tool_use" => tool_display(name, ToolDisplayInput::HostedTool, block.get("input")),
        "tool_use" => tool_display(name, ToolDisplayInput::LocalTool, block.get("input")),
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

fn anthropic_usage(value: &Value) -> Option<ProviderUsage> {
    Some(ProviderUsage {
        input_tokens: value
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        output_tokens: value
            .get("output_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        total_tokens: None,
        cache_read_input_tokens: value
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
        cache_creation_input_tokens: value
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptSections;

    fn test_tool(
        provider: ProviderKind,
        name: &str,
        description: &str,
        input_schema: Value,
    ) -> ProviderTool {
        ProviderTool::function_json_named(provider, name, description, input_schema)
    }

    fn first_party_tools(provider: ProviderKind) -> Vec<ProviderTool> {
        agent_tools::ToolRegistry::with_builtin_tools().provider_tools_for_provider(provider)
    }

    #[test]
    fn messages_body_omits_adaptive_thinking_for_non_adaptive_models() {
        let body = messages_body(ModelRequest {
            model: "claude-sonnet-4-5".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::CustomDefinitions,
            tools: vec![test_tool(
                ProviderKind::Claude,
                "read",
                "read a file",
                json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            )],
            max_tokens: Some(2048),
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        assert!(body["system"][0]["text"]
            .as_str()
            .expect("attribution text")
            .starts_with("x-anthropic-billing-header: cc_version="));
        assert!(body["system"][0].get("cache_control").is_none());
        assert_eq!(
            body["system"][1],
            json!({
                "type": "text",
                "text": "stable rules",
                "cache_control": {
                    "type": "ephemeral",
                    "ttl": "1h",
                },
            })
        );
        assert!(body.get("thinking").is_none());
        assert!(body.get("output_config").is_none());
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert_eq!(body["tools"][0]["name"], "read");
        // Tools must NOT carry a cache_control breakpoint: the stable system
        // block's breakpoint already covers tools via the cumulative prefix
        // hash, so a tools-level marker would waste a breakpoint slot.
        assert!(body["tools"][0].get("cache_control").is_none());
        // Latest transcript block uses 5m (default ephemeral, no `ttl` field):
        // it's regenerated each turn, so paying the 1h write premium is waste.
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({
                "type": "ephemeral",
            })
        );
    }

    #[test]
    fn messages_body_enables_adaptive_thinking_for_adaptive_models() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::CustomDefinitions,
            tools: vec![test_tool(
                ProviderKind::Claude,
                "read",
                "read a file",
                json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            )],
            max_tokens: Some(2048),
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["max_tokens"], 2048);
    }

    #[test]
    fn beta_header_is_gated_by_model_capabilities() {
        let legacy = anthropic_beta_header("claude-sonnet-4-5");
        assert!(legacy.contains(INTERLEAVED_THINKING_BETA));
        assert!(legacy.contains(CONTEXT_MANAGEMENT_BETA));
        assert!(!legacy.contains(EFFORT_BETA));

        let adaptive = anthropic_beta_header("claude-opus-4-7");
        assert!(!adaptive.contains(INTERLEAVED_THINKING_BETA));
        assert!(adaptive.contains(CONTEXT_MANAGEMENT_BETA));
        assert!(adaptive.contains(EFFORT_BETA));
    }

    #[test]
    fn count_tokens_body_matches_message_input_shape_without_generation_budget_by_default() {
        let request = ProviderTokenCountRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::CustomDefinitions,
            tools: vec![test_tool(
                ProviderKind::Claude,
                "read",
                "read a file",
                json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }),
            )],
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        };
        let body = count_tokens_body(request).expect("count body renders");

        assert_eq!(body["model"], "claude-opus-4-7");
        assert_eq!(body["messages"][0]["role"], "user");
        assert_eq!(body["system"][1]["text"], "stable rules");
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "medium");
        assert_eq!(body["tool_choice"]["type"], "auto");
        assert_eq!(body["tools"][0]["name"], "read");
        assert!(body.get("max_tokens").is_none());
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn messages_body_sorts_tools_for_cache_stability() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::CustomDefinitions,
            tools: vec![
                test_tool(
                    ProviderKind::Claude,
                    "write",
                    "write a file",
                    json!({ "type": "object" }),
                ),
                test_tool(
                    ProviderKind::Claude,
                    "read",
                    "read a file",
                    json!({ "type": "object" }),
                ),
            ],
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][1]["name"], "write");
        // No tools-level breakpoints regardless of how many tools there are.
        assert!(body["tools"][0].get("cache_control").is_none());
        assert!(body["tools"][1].get("cache_control").is_none());
    }

    #[test]
    fn count_tokens_body_counts_the_same_local_tool_surface() {
        let body = count_tokens_body(ProviderTokenCountRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hi")).into()],
            tool_profile: ProviderToolProfile::AnthropicCoding,
            tools: first_party_tools(ProviderKind::Claude),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("count body renders");

        let tools = body["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .filter_map(|tool| tool.get("name").and_then(Value::as_str))
            .collect();
        assert_eq!(
            names,
            [
                "Bash",
                "Grep",
                "LoadSkill",
                "str_replace_based_edit_tool",
                "web_fetch",
                "web_search"
            ]
        );
        for tool in tools {
            let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or("");
            assert!(
                !tool_type.starts_with("web_search_") && !tool_type.starts_with("web_fetch_"),
                "main-loop web tools must remain local JSON tools, not Anthropic server tools"
            );
        }
    }

    #[test]
    fn messages_body_renders_anthropic_native_coding_tools() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::AnthropicCoding,
            tools: first_party_tools(ProviderKind::Claude),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        assert_eq!(body["tools"][0]["name"], "Bash");
        assert!(body["tools"][0].get("type").is_none());
        assert_eq!(body["tools"][1]["name"], "Grep");
        assert!(body["tools"][1].get("type").is_none());
        assert_eq!(body["tools"][2]["name"], "LoadSkill");
        assert!(body["tools"][2].get("type").is_none());
        assert_eq!(body["tools"][3]["type"], "text_editor_20250728");
        assert_eq!(body["tools"][3]["name"], "str_replace_based_edit_tool");
        assert_eq!(body["tools"][4]["name"], "web_fetch");
        assert!(body["tools"][4].get("type").is_none());
        assert_eq!(body["tools"][5]["name"], "web_search");
        assert!(body["tools"][5].get("type").is_none());
        // Native coding tools also carry no per-tool cache_control: the
        // stable-system breakpoint covers them via the cumulative hash.
        for index in 0..6 {
            assert!(
                body["tools"][index].get("cache_control").is_none(),
                "tool {index} should not carry cache_control"
            );
        }
    }

    #[test]
    fn messages_body_marks_latest_transcript_block_for_cache() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![
                TranscriptItem::UserMessage(UserMessage::text("first")).into(),
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("second".to_string())],
                })
                .into(),
            ],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        // Latest transcript block carries a 5m (default ephemeral) breakpoint,
        // not 1h: the marker is regenerated next turn.
        assert_eq!(
            body["messages"][1]["content"][0]["cache_control"],
            json!({
                "type": "ephemeral",
            })
        );
    }

    #[test]
    fn anthropic_parser_preserves_thinking_and_tool_blocks() {
        let response = json!({
            "content": [
                { "type": "thinking", "thinking": "private", "signature": "sig" },
                { "type": "redacted_thinking", "data": "opaque" },
                { "type": "text", "text": "hello" },
                { "type": "tool_use", "id": "toolu_1", "name": "str_replace_based_edit_tool", "input": { "path": "README.md" } }
            ]
        });

        let response = parse_anthropic_message(&response).expect("message parses");
        let assistant = response.assistant;

        assert_eq!(assistant.text(), "hello");
        let calls = assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "toolu_1");
        assert_eq!(calls[0].tool_name, "Edit");
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
        assert_eq!(
            response.provider_replay[3]
                .display
                .as_ref()
                .map(|display| display.pretty_name.as_str()),
            Some("Edit")
        );
    }

    #[test]
    fn anthropic_parser_preserves_usage_cache_metrics() {
        let response = json!({
            "content": [
                { "type": "text", "text": "hello" }
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 20,
                "cache_read_input_tokens": 75,
                "cache_creation_input_tokens": 25
            }
        });

        let response = parse_anthropic_message(&response).expect("message parses");
        let usage = response.usage.expect("usage should be parsed");

        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.total_tokens, None);
        assert_eq!(usage.cache_read_input_tokens, Some(75));
        assert_eq!(usage.cache_creation_input_tokens, Some(25));
    }

    #[test]
    fn anthropic_parser_maps_max_tokens_stop_reason() {
        let response = json!({
            "content": [
                { "type": "text", "text": "partial" }
            ],
            "stop_reason": "max_tokens"
        });

        let response = parse_anthropic_message(&response).expect("message parses");

        assert_eq!(response.assistant.text(), "partial");
        assert_eq!(response.stop_reason, ModelStopReason::MaxOutputTokens);
    }

    #[test]
    fn anthropic_serializer_prefers_replay_blocks() {
        let raw = json!({ "type": "thinking", "thinking": "private", "signature": "sig" });
        let messages = transcript_to_messages(
            &crate::PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("visible".to_string())],
                }),
                provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &raw).unwrap()],
            }],
        )
        .expect("messages render");

        assert_eq!(messages[0]["content"], json!([raw]));
    }

    #[test]
    fn stable_system_block_keeps_one_hour_ttl() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        // Stable system block keeps the 1h TTL — it's long-lived across many
        // turns and benefits from the extended retention even at 2x write cost.
        assert_eq!(
            body["system"][1]["cache_control"],
            json!({
                "type": "ephemeral",
                "ttl": "1h",
            })
        );
    }

    #[test]
    fn short_transcript_uses_only_tail_breakpoint() {
        let transcript = vec![
            TranscriptItem::UserMessage(UserMessage::text("turn 1")).into(),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("response 1".to_string())],
            })
            .into(),
            TranscriptItem::UserMessage(UserMessage::text("turn 2")).into(),
        ];
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript,
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        // Only the LAST message carries cache_control; earlier ones are
        // covered by Anthropic's automatic ~20-block backward walk.
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert!(body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(
            body["messages"][2]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
    }

    #[test]
    fn long_transcript_adds_deep_history_breakpoint() {
        // Build a transcript with enough cacheable blocks to exceed
        // TRANSCRIPT_LOOKBACK_BLOCKS (18). Each pair contributes 2 blocks.
        let mut transcript = Vec::new();
        for index in 0..25 {
            transcript
                .push(TranscriptItem::UserMessage(UserMessage::text(format!("u{index}"))).into());
            transcript.push(
                TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text(format!("a{index}"))],
                })
                .into(),
            );
        }
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript,
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
        })
        .expect("body renders");

        let messages = body["messages"].as_array().expect("messages array");
        // Tail breakpoint: last message must carry cache_control.
        let last = messages.last().expect("at least one message");
        assert_eq!(
            last["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        // Deep breakpoint: exactly one earlier message also carries
        // cache_control, and it lives within the lookback window of the tail.
        let marked_indices: Vec<usize> = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| message["content"][0].get("cache_control").is_some())
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            marked_indices.len(),
            2,
            "expected exactly tail + deep breakpoints, got {marked_indices:?}"
        );
        let tail_index = marked_indices[1];
        let deep_index = marked_indices[0];
        assert!(
            deep_index < tail_index,
            "deep breakpoint must come before tail"
        );
        // Deep marker should be within the lookback window of the tail.
        assert!(
            tail_index - deep_index <= TRANSCRIPT_LOOKBACK_BLOCKS,
            "deep breakpoint at {deep_index} is too far from tail at {tail_index}"
        );
    }

    #[test]
    fn attribution_fingerprint_is_stable_across_different_first_user_messages() {
        // Two requests with identical stable system prompts but completely
        // different opening user messages must produce the same fingerprint —
        // that's the whole point of deriving it from `stable_prefix`.
        let make_body = |first_user: &str| {
            messages_body(ModelRequest {
                model: "claude-opus-4-7".to_string(),
                prompt: PromptSections::stable("a stable system prompt long enough to fingerprint"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text(first_user)).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: None,
            })
            .expect("body renders")
        };

        let body_a = make_body("Explain quantum tunneling like I'm five");
        let body_b = make_body("write me a haiku about ferrets");

        let header_a = body_a["system"][0]["text"].as_str().expect("text");
        let header_b = body_b["system"][0]["text"].as_str().expect("text");
        assert_eq!(
            header_a, header_b,
            "attribution headers must match across sessions with the same stable prompt"
        );
    }

    #[test]
    fn attribution_fingerprint_changes_with_stable_prompt() {
        // Sanity check: changing the stable system prompt SHOULD change the
        // fingerprint, otherwise it would be useless for routing.
        let make_body = |stable: &str| {
            messages_body(ModelRequest {
                model: "claude-opus-4-7".to_string(),
                prompt: PromptSections::stable(stable),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("anything")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: None,
            })
            .expect("body renders")
        };

        let body_a = make_body("you are a helpful coding assistant working on rust");
        let body_b = make_body("you are a research assistant focused on biology");
        assert_ne!(
            body_a["system"][0]["text"], body_b["system"][0]["text"],
            "different stable prompts must produce different fingerprints"
        );
    }
}
