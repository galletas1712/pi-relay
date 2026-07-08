use agent_tools::{tool_display, ProviderTool, ToolDisplayInput};
use agent_vocab::{
    AssistantItem, AssistantMessage, ContentBlock, ProviderKind, ProviderReplayItem,
    ReasoningEffort, ReplayDisplay, ToolCall, ToolCallId, TranscriptItem, UserMessage,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::sync::{watch, Mutex};

#[cfg(test)]
use crate::sse::read_json_sse_text;
use crate::{
    common::{
        compaction_summary_text, ensure_success, push_text_item, response_excerpt, response_text,
    },
    http::send_provider_generation_request,
    sse::{read_provider_json_sse_response, SseControl, SseEvent},
    ModelProvider, ModelRequest, ModelResponse, ModelStopDetails, ModelStopReason,
    ModelTranscriptEntry, NativeCompactionErrorKind, ProviderCompactionRequest,
    ProviderCompactionResponse, ProviderError, ProviderModelMetadata, ProviderResult,
    ProviderTokenCountRequest, ProviderTokenCountResponse, ProviderToolProfile, ProviderUsage,
};

const DEFAULT_MAX_OUTPUT_BUDGET: u32 = 64_000;
const UNKNOWN_MODEL_MAX_OUTPUT_TOKENS: u32 = 64_000;
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
const COMPACTION_BETA: &str = "compact-2026-01-12";
const COMPACTION_TRIGGER_TOKENS: usize = 50_000;
const COMPACTION_TERMINAL_USER_INSTRUCTION: &str =
    "Proceed with the configured context-management compaction.";
const CLAUDE_CODE_VERSION: &str = "2.1.75";
const CLAUDE_CODE_USER_AGENT: &str = "claude-cli/2.1.75 (external, cli)";
const ATTRIBUTION_FINGERPRINT_SALT: &str = "59cf53e54c78";
const MODEL_CACHE_CAPACITY: usize = 64;
const MODEL_CACHE_SUCCESS_TTL: Duration = Duration::from_secs(6 * 60 * 60);
const MODEL_CACHE_FAILURE_TTL: Duration = Duration::from_secs(60);

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
    model_cache: AnthropicModelCache,
}

fn validate_anthropic_hosted_tool_result(block_type: &str, content: &Value) -> ProviderResult<()> {
    match (block_type, content) {
        ("web_search_tool_result", Value::Array(results)) => {
            for result in results {
                if anthropic_block_type(result, "Anthropic web search result")?
                    != "web_search_result"
                {
                    return Err(ProviderError::Provider(
                        "Anthropic web_search_tool_result contained unsupported result type"
                            .to_string(),
                    ));
                }
            }
        }
        ("web_search_tool_result", Value::Object(_)) => {
            if anthropic_block_type(content, "Anthropic web search result error")?
                != "web_search_tool_result_error"
            {
                return Err(ProviderError::Provider(
                    "Anthropic web_search_tool_result contained malformed error".to_string(),
                ));
            }
        }
        ("web_fetch_tool_result", Value::Object(_)) => {
            match anthropic_block_type(content, "Anthropic web fetch result")? {
                "web_fetch_result" | "web_fetch_tool_result_error" => {}
                _ => {
                    return Err(ProviderError::Provider(
                        "Anthropic web_fetch_tool_result contained malformed error".to_string(),
                    ))
                }
            }
        }
        _ => {
            return Err(ProviderError::Provider(format!(
                "Anthropic {block_type} content had invalid type"
            )))
        }
    }
    Ok(())
}

fn anthropic_stream_index(event: &Value, event_type: &str) -> ProviderResult<usize> {
    event
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .ok_or_else(|| {
            ProviderError::Provider(format!(
                "Anthropic {event_type} missing valid representable index"
            ))
        })
}

fn validate_anthropic_stream_content_start(block: &Value) -> ProviderResult<()> {
    let block_type = anthropic_block_type(block, "Anthropic content_block_start content_block")?;
    let required_string = |field: &str| -> ProviderResult<&str> {
        block.get(field).and_then(Value::as_str).ok_or_else(|| {
            ProviderError::Provider(format!(
                "Anthropic {block_type} content block missing string {field}"
            ))
        })
    };
    let required_nonempty_string = |field: &str| -> ProviderResult<&str> {
        required_string(field)?
            .is_empty()
            .then_some(())
            .map_or_else(
                || required_string(field),
                |_| {
                    Err(ProviderError::Provider(format!(
                        "Anthropic {block_type} content block had empty {field}"
                    )))
                },
            )
    };

    match block_type {
        "text" => {
            if !required_string("text")?.is_empty() {
                return Err(ProviderError::Provider(
                    "Anthropic streamed text content block had pre-populated text".to_string(),
                ));
            }
            if !matches!(
                block.get("citations"),
                None | Some(Value::Null | Value::Array(_))
            ) {
                return Err(ProviderError::Provider(
                    "Anthropic text content block citations was not an array or null".to_string(),
                ));
            }
            if block
                .get("citations")
                .and_then(Value::as_array)
                .is_some_and(|citations| !citations.is_empty())
            {
                return Err(ProviderError::Provider(
                    "Anthropic streamed text content block had pre-populated citations".to_string(),
                ));
            }
        }
        "thinking" => {
            if !required_string("thinking")?.is_empty() || !required_string("signature")?.is_empty()
            {
                return Err(ProviderError::Provider(
                    "Anthropic streamed thinking content block had pre-populated content"
                        .to_string(),
                ));
            }
        }
        "redacted_thinking" => {
            required_nonempty_string("data")?;
        }
        "tool_use" | "server_tool_use" => {
            required_nonempty_string("id")?;
            required_nonempty_string("name")?;
            match block.get("input").and_then(Value::as_object) {
                Some(input) if input.is_empty() => {}
                Some(_) => {
                    return Err(ProviderError::Provider(format!(
                        "Anthropic streamed {block_type} content block had pre-populated input"
                    )))
                }
                None => {
                    return Err(ProviderError::Provider(format!(
                        "Anthropic {block_type} content block missing input object"
                    )))
                }
            }
        }
        "web_search_tool_result" | "web_fetch_tool_result" => {
            required_nonempty_string("tool_use_id")?;
            validate_anthropic_hosted_tool_result(
                block_type,
                block.get("content").ok_or_else(|| {
                    ProviderError::Provider(format!(
                        "Anthropic {block_type} content block missing content"
                    ))
                })?,
            )?;
        }
        _ => {
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_start had unsupported content block type {block_type}"
            )))
        }
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AnthropicCompactionBlockError {
    WrongType,
    MissingContent,
    NullContent,
    EmptyContent,
    NonStringContent,
    InvalidEncryptedContent,
}

impl AnthropicCompactionBlockError {
    fn message(self) -> &'static str {
        match self {
            Self::WrongType => "type was not compaction",
            Self::MissingContent => "content was missing",
            Self::NullContent => "content was null",
            Self::EmptyContent => "content was empty",
            Self::NonStringContent => "content was not a string",
            Self::InvalidEncryptedContent => "encrypted_content was not string or null",
        }
    }
}

fn anthropic_auto_compact_limit(window: usize) -> usize {
    if window == 1_000_000 {
        500_000
    } else {
        window / 100 * 85 + window % 100 * 85 / 100
    }
}

/// The one validity rule used for persisted Anthropic compaction replay.
///
/// Unknown fields are deliberately allowed and retained verbatim.
fn validate_anthropic_compaction_block(block: &Value) -> Result<(), AnthropicCompactionBlockError> {
    if block.get("type").and_then(Value::as_str) != Some("compaction") {
        return Err(AnthropicCompactionBlockError::WrongType);
    }
    let content = block
        .get("content")
        .ok_or(AnthropicCompactionBlockError::MissingContent)?;
    if content.is_null() {
        return Err(AnthropicCompactionBlockError::NullContent);
    }
    let content = content
        .as_str()
        .ok_or(AnthropicCompactionBlockError::NonStringContent)?;
    if content.is_empty() {
        return Err(AnthropicCompactionBlockError::EmptyContent);
    }
    if !matches!(
        block.get("encrypted_content"),
        None | Some(Value::Null | Value::String(_))
    ) {
        return Err(AnthropicCompactionBlockError::InvalidEncryptedContent);
    }
    Ok(())
}

fn reject_ordinary_anthropic_compaction() -> ProviderError {
    ProviderError::Provider(
        "Anthropic ordinary response unexpectedly contained inline compaction; refusing to persist partial or opaque response content"
            .to_string(),
    )
}

fn apply_messages_compaction_replay_strategy(
    body: &mut Value,
    replays_compaction: bool,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<()> {
    if replays_compaction {
        let trigger = metadata.max_input_tokens.ok_or_else(|| {
            ProviderError::Provider(format!(
                "safe Anthropic compaction replay for {} requires a resolved max_input_tokens ceiling",
                metadata.id
            ))
        })?;
        // A paid Sonnet 5 continuation accepted the model-ceiling trigger and
        // resumed the blocked action after native compaction; see the worklog.
        // There is no documented apply-only mode.
        body["context_management"] = json!({
            "edits": [{
                "type": "compact_20260112",
                "trigger": {
                    "type": "input_tokens",
                    "value": trigger.max(COMPACTION_TRIGGER_TOKENS),
                },
                "pause_after_compaction": true,
            }],
        });
    }
    Ok(())
}

fn apply_count_compaction_replay_strategy(body: &mut Value, replays_compaction: bool) {
    if replays_compaction {
        // count_tokens applies existing compaction blocks but, unlike Messages,
        // is documented not to trigger new compactions. Retain the bare shape
        // proven by live count replay.
        body["context_management"] = json!({
            "edits": [{ "type": "compact_20260112" }],
        });
    }
}

#[cfg(test)]
fn compaction_body(request: ProviderCompactionRequest) -> ProviderResult<Value> {
    let metadata = static_anthropic_model_metadata(&request.model);
    compaction_body_with_metadata(request, &metadata)
}

fn compaction_body_with_metadata(
    request: ProviderCompactionRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<Value> {
    if !metadata.capabilities.native_compaction {
        return Err(ProviderError::native_compaction(
            NativeCompactionErrorKind::Unsupported,
            format!(
                "Anthropic model {} does not advertise compact_20260112 support",
                metadata.id
            ),
        ));
    }
    let instructions = request
        .compaction_instructions
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| {
            ProviderError::Provider(
                "Anthropic native compaction requires non-empty custom instructions".to_string(),
            )
        })?;
    let max_tokens = metadata.max_tokens.min(DEFAULT_MAX_OUTPUT_BUDGET);
    let mut rendered = anthropic_request_body(AnthropicRequestBodyInput {
        model: request.model,
        prompt: request.prompt,
        transcript: request.transcript,
        // Native compaction is a text-only sampling request. Supplying no
        // tools avoids Anthropic's documented null compaction-block failure.
        tool_profile: ProviderToolProfile::None,
        tools: Vec::new(),
        max_tokens: Some(max_tokens),
        reasoning_effort: Some(request.reasoning_effort),
        capabilities: metadata.capabilities,
        cache_transcript: true,
        transcript_cache_prefix_len: None,
    })?;
    ensure_compaction_terminal_user_message(&mut rendered.body);
    rendered.body["context_management"] = json!({
        "edits": [{
            "type": "compact_20260112",
            "trigger": {
                "type": "input_tokens",
                "value": COMPACTION_TRIGGER_TOKENS,
            },
            "pause_after_compaction": true,
            "instructions": instructions,
        }],
    });
    Ok(rendered.body)
}

fn ensure_compaction_terminal_user_message(body: &mut Value) {
    let messages = body["messages"]
        .as_array_mut()
        .expect("Anthropic request bodies always contain a messages array");
    let last_assistant = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) == Some("assistant"));
    let mut required_tool_results = last_assistant
        .and_then(|index| messages.get(index))
        .and_then(|message| message.get("content"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_use"))
        .filter_map(|block| block.get("id").and_then(Value::as_str))
        .map(ToOwned::to_owned)
        .collect::<Vec<_>>();
    let mut seen_tool_uses = HashSet::new();
    required_tool_results.retain(|id| seen_tool_uses.insert(id.clone()));

    let trailing_user_start = last_assistant.map(|index| index + 1).unwrap_or(0);
    let has_trailing_user = trailing_user_start < messages.len()
        && messages[trailing_user_start..]
            .iter()
            .all(|message| message.get("role").and_then(Value::as_str) == Some("user"));
    if has_trailing_user {
        if required_tool_results.is_empty() {
            return;
        }
        let mut existing_results = HashMap::new();
        let mut non_results = Vec::new();
        for message in messages.drain(trailing_user_start..) {
            for block in message
                .get("content")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if block.get("type").and_then(Value::as_str) == Some("tool_result") {
                    if let Some(id) = block
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .filter(|id| seen_tool_uses.contains(*id))
                    {
                        existing_results
                            .entry(id.to_string())
                            .or_insert_with(|| block.clone());
                    }
                } else {
                    non_results.push(block.clone());
                }
            }
        }
        let mut content = required_tool_results
            .into_iter()
            .map(|tool_use_id| {
                existing_results
                    .remove(&tool_use_id)
                    .unwrap_or_else(|| synthetic_tool_result(tool_use_id))
            })
            .collect::<Vec<_>>();
        content.append(&mut non_results);
        messages.push(json!({ "role": "user", "content": content }));
        return;
    }

    let mut content = required_tool_results
        .into_iter()
        .map(synthetic_tool_result)
        .collect::<Vec<_>>();
    content.push(json!({
        "type": "text",
        "text": COMPACTION_TERMINAL_USER_INSTRUCTION,
    }));
    messages.push(json!({
        "role": "user",
        "content": content,
    }));
}

fn synthetic_tool_result(tool_use_id: String) -> Value {
    json!({
        "type": "tool_result",
        "tool_use_id": tool_use_id,
        "content": "Tool result unavailable at the compaction boundary.",
        "is_error": true,
    })
}

fn anthropic_compaction_beta_header() -> String {
    format!("{CLAUDE_CODE_BETA},{COMPACTION_BETA}")
}

#[derive(Debug, Clone, Default)]
pub struct AnthropicModelCache {
    state: Arc<Mutex<AnthropicModelCacheState>>,
}

#[derive(Debug, Default)]
struct AnthropicModelCacheState {
    entries: HashMap<String, CachedAnthropicModel>,
    next_generation: u64,
    access_counter: u64,
}

#[derive(Debug, Clone)]
struct CachedAnthropicModel {
    fetched_at: Option<Instant>,
    retry_after: Option<Instant>,
    model: Option<AnthropicModelMetadata>,
    refresh: Option<ModelRefresh>,
    last_access: u64,
}

#[derive(Debug, Clone)]
struct ModelRefresh {
    generation: u64,
    receiver: watch::Receiver<ModelRefreshStatus>,
}

#[derive(Debug, Clone)]
enum ModelRefreshStatus {
    Pending,
    Finished(Option<AnthropicModelMetadata>),
}

enum ModelCacheDecision {
    Return(Option<AnthropicModelMetadata>),
    Wait(ModelRefresh),
    Start {
        refresh: ModelRefresh,
        sender: watch::Sender<ModelRefreshStatus>,
    },
}

impl AnthropicModelCache {
    async fn decision(&self, model: &str, now: Instant) -> ModelCacheDecision {
        let mut state = self.state.lock().await;
        state.access_counter = state.access_counter.saturating_add(1);
        let last_access = state.access_counter;

        if let Some(cached) = state.entries.get_mut(model) {
            cached.last_access = last_access;
            if cached
                .model
                .as_ref()
                .zip(cached.fetched_at)
                .is_some_and(|(_, fetched_at)| {
                    now.saturating_duration_since(fetched_at) < MODEL_CACHE_SUCCESS_TTL
                })
            {
                return ModelCacheDecision::Return(cached.model.clone());
            }
            if cached
                .retry_after
                .is_some_and(|retry_after| now < retry_after)
            {
                return ModelCacheDecision::Return(cached.model.clone());
            }
            if let Some(refresh) = cached.refresh.as_ref() {
                return ModelCacheDecision::Wait(refresh.clone());
            }
        } else {
            state.evict_for_insert();
            state.entries.insert(
                model.to_string(),
                CachedAnthropicModel {
                    fetched_at: None,
                    retry_after: None,
                    model: None,
                    refresh: None,
                    last_access,
                },
            );
        }

        state.next_generation = state.next_generation.saturating_add(1);
        let generation = state.next_generation;
        let (sender, receiver) = watch::channel(ModelRefreshStatus::Pending);
        let refresh = ModelRefresh {
            generation,
            receiver,
        };
        let cached = state
            .entries
            .get_mut(model)
            .expect("model cache entry exists before refresh");
        cached.retry_after = None;
        cached.refresh = Some(refresh.clone());
        ModelCacheDecision::Start { refresh, sender }
    }

    async fn commit_refresh(
        &self,
        model: &str,
        generation: u64,
        resolved: Option<AnthropicModelMetadata>,
        now: Instant,
    ) -> Option<AnthropicModelMetadata> {
        let mut state = self.state.lock().await;
        state.access_counter = state.access_counter.saturating_add(1);
        let last_access = state.access_counter;
        let Some(cached) = state.entries.get_mut(model) else {
            // An explicitly abandoned generation may have become eligible for
            // eviction. Do not recreate or overwrite newer cache state.
            return resolved;
        };
        if cached
            .refresh
            .as_ref()
            .is_none_or(|refresh| refresh.generation != generation)
        {
            return cached.model.clone();
        }

        cached.refresh = None;
        cached.last_access = last_access;
        if let Some(resolved) = resolved {
            cached.model = Some(resolved);
            cached.fetched_at = Some(now);
            cached.retry_after = None;
        } else {
            // Keep last-known-good metadata, including its original fetched_at,
            // but independently back off the failed refresh. If no success has
            // ever existed, this same timestamp is a cold negative entry.
            cached.retry_after = Some(now + MODEL_CACHE_FAILURE_TTL);
        }
        let effective = cached.model.clone();
        state.trim_to_capacity();
        effective
    }

    async fn abandon_refresh(&self, model: &str, generation: u64) {
        let mut state = self.state.lock().await;
        let Some(cached) = state.entries.get_mut(model) else {
            return;
        };
        if cached
            .refresh
            .as_ref()
            .is_some_and(|refresh| refresh.generation == generation)
        {
            cached.refresh = None;
            state.trim_to_capacity();
        }
    }
}

impl AnthropicModelCacheState {
    fn evict_for_insert(&mut self) {
        while self.entries.len() >= MODEL_CACHE_CAPACITY {
            if !self.evict_oldest_settled() {
                break;
            }
        }
    }

    fn trim_to_capacity(&mut self) {
        while self.entries.len() > MODEL_CACHE_CAPACITY {
            if !self.evict_oldest_settled() {
                break;
            }
        }
    }

    fn evict_oldest_settled(&mut self) -> bool {
        if let Some(oldest) = self
            .entries
            .iter()
            .filter(|(_, cached)| cached.refresh.is_none())
            .min_by_key(|(_, cached)| cached.last_access)
            .map(|(model, _)| model.clone())
        {
            self.entries.remove(&oldest);
            true
        } else {
            // Every entry is refreshing. Preserve their single-flight state
            // and allow temporary overflow until a refresh settles.
            false
        }
    }
}

async fn wait_for_model_refresh(
    mut refresh: ModelRefresh,
) -> Option<Option<AnthropicModelMetadata>> {
    loop {
        let status = refresh.receiver.borrow_and_update().clone();
        if let ModelRefreshStatus::Finished(metadata) = status {
            return Some(metadata);
        }
        if refresh.receiver.changed().await.is_err() {
            return None;
        }
    }
}

fn anthropic_error_message(
    error_type: Option<&str>,
    message: Option<&str>,
    event: &Value,
) -> String {
    let message = message
        .map(str::to_string)
        .unwrap_or_else(|| event.to_string());
    if let Some(error_type) = error_type {
        if !message.contains(error_type) {
            return format!("{error_type}: {message}");
        }
    }
    message
}

fn anthropic_stream_provider_error(error_type: Option<&str>, message: String) -> ProviderError {
    match error_type {
        Some("rate_limit_error") => ProviderError::Status {
            status: 429,
            message,
        },
        Some("api_error") => ProviderError::Status {
            status: 500,
            message,
        },
        Some("overloaded_error") => ProviderError::Status {
            status: 529,
            message,
        },
        Some(_) | None => ProviderError::Provider(format!("Anthropic error: {message}")),
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct AnthropicModelCapabilities {
    adaptive_thinking: bool,
    adaptive_thinking_default: bool,
    effort: bool,
    low_effort: bool,
    medium_effort: bool,
    high_effort: bool,
    xhigh_effort: bool,
    max_effort: bool,
    native_compaction: bool,
}

impl AnthropicModelCapabilities {
    fn adaptive_with_all_efforts(adaptive_thinking_default: bool) -> Self {
        Self {
            adaptive_thinking: true,
            adaptive_thinking_default,
            effort: true,
            low_effort: true,
            medium_effort: true,
            high_effort: true,
            xhigh_effort: true,
            max_effort: true,
            native_compaction: true,
        }
    }

    fn supports_effort(self, effort: ReasoningEffort) -> bool {
        match effort {
            ReasoningEffort::Low => self.low_effort,
            ReasoningEffort::Medium => self.medium_effort,
            ReasoningEffort::High => self.high_effort,
            ReasoningEffort::XHigh => self.xhigh_effort,
            ReasoningEffort::Max => self.max_effort,
            ReasoningEffort::None | ReasoningEffort::Minimal => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AnthropicModelMetadata {
    id: String,
    max_input_tokens: Option<usize>,
    max_tokens: u32,
    capabilities: AnthropicModelCapabilities,
}

#[derive(Debug, Deserialize)]
struct ModelsApiModel {
    id: String,
    max_input_tokens: Option<usize>,
    max_tokens: Option<u32>,
    #[serde(default)]
    capabilities: Option<ModelsApiCapabilities>,
}

#[derive(Debug, Deserialize)]
struct ModelsApiCapabilities {
    #[serde(default)]
    context_management: Option<ModelsApiContextManagementCapability>,
    effort: ModelsApiEffortCapability,
    thinking: ModelsApiThinkingCapability,
}

#[derive(Debug, Deserialize)]
struct ModelsApiContextManagementCapability {
    supported: bool,
    #[serde(default)]
    compact_20260112: Option<ModelsApiCapability>,
}

#[derive(Debug, Deserialize)]
struct ModelsApiCapability {
    supported: bool,
}

#[derive(Debug, Deserialize)]
struct ModelsApiEffortCapability {
    supported: bool,
    low: ModelsApiCapability,
    medium: ModelsApiCapability,
    high: ModelsApiCapability,
    xhigh: Option<ModelsApiCapability>,
    max: ModelsApiCapability,
}

#[derive(Debug, Deserialize)]
struct ModelsApiThinkingCapability {
    supported: bool,
    types: ModelsApiThinkingTypes,
}

#[derive(Debug, Deserialize)]
struct ModelsApiThinkingTypes {
    adaptive: ModelsApiCapability,
}

fn static_anthropic_model_metadata(model: &str) -> AnthropicModelMetadata {
    let normalized = model.to_ascii_lowercase();
    let (max_input_tokens, max_tokens, capabilities) = match normalized.as_str() {
        "claude-sonnet-5" | "claude-fable-5" => (
            Some(1_000_000),
            128_000,
            AnthropicModelCapabilities::adaptive_with_all_efforts(true),
        ),
        "claude-opus-4-8" | "claude-opus-4-7" => (
            Some(1_000_000),
            128_000,
            AnthropicModelCapabilities::adaptive_with_all_efforts(false),
        ),
        "claude-opus-4-6" | "claude-sonnet-4-6" => (
            None,
            UNKNOWN_MODEL_MAX_OUTPUT_TOKENS,
            AnthropicModelCapabilities::adaptive_with_all_efforts(false),
        ),
        "claude-sonnet-4-5" => (
            Some(200_000),
            UNKNOWN_MODEL_MAX_OUTPUT_TOKENS,
            AnthropicModelCapabilities::default(),
        ),
        _ => (
            None,
            UNKNOWN_MODEL_MAX_OUTPUT_TOKENS,
            AnthropicModelCapabilities {
                native_compaction: matches!(
                    normalized.as_str(),
                    "claude-mythos-5" | "claude-mythos-preview"
                ),
                ..AnthropicModelCapabilities::default()
            },
        ),
    };
    AnthropicModelMetadata {
        id: model.to_string(),
        max_input_tokens,
        max_tokens,
        capabilities,
    }
}

fn merge_models_api_metadata(
    fallback: AnthropicModelMetadata,
    discovered: ModelsApiModel,
) -> AnthropicModelMetadata {
    let mut capabilities = fallback.capabilities;
    if let Some(discovered_capabilities) = discovered.capabilities {
        if let Some(context_management) = discovered_capabilities.context_management {
            capabilities.native_compaction = context_management.supported
                && context_management
                    .compact_20260112
                    .is_some_and(|capability| capability.supported);
        }
        let effort = discovered_capabilities.effort;
        capabilities.effort = effort.supported;
        capabilities.low_effort = effort.low.supported;
        capabilities.medium_effort = effort.medium.supported;
        capabilities.high_effort = effort.high.supported;
        capabilities.xhigh_effort = effort.xhigh.is_some_and(|value| value.supported);
        capabilities.max_effort = effort.max.supported;

        let thinking = discovered_capabilities.thinking;
        capabilities.adaptive_thinking = thinking.supported && thinking.types.adaptive.supported;
    }
    AnthropicModelMetadata {
        id: discovered.id,
        max_input_tokens: discovered
            .max_input_tokens
            .filter(|value| *value > 0)
            .or(fallback.max_input_tokens),
        max_tokens: discovered
            .max_tokens
            .filter(|value| *value > 0)
            .unwrap_or(fallback.max_tokens),
        capabilities,
    }
}

fn anthropic_beta_header() -> &'static str {
    // Keep the Claude Code identity beta required by the existing API-key
    // transport. Effort, one-hour cache TTL, fine-grained streaming, adaptive
    // thinking, and the current web tools are GA and must not add stale betas.
    CLAUDE_CODE_BETA
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
        original_input_tokens: response
            .pointer("/context_management/original_input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize),
    })
}

impl AnthropicProvider {
    pub fn new_with_client(client: reqwest::Client, api_key: impl Into<String>) -> Self {
        Self::new_with_client_and_cache(client, api_key, AnthropicModelCache::default())
    }

    pub fn new_with_client_and_cache(
        client: reqwest::Client,
        api_key: impl Into<String>,
        model_cache: AnthropicModelCache,
    ) -> Self {
        Self {
            client,
            api_key: api_key.into(),
            base_url: "https://api.anthropic.com/v1".to_string(),
            model_cache,
        }
    }

    async fn resolved_model_metadata(&self, model: &str) -> AnthropicModelMetadata {
        let fallback = static_anthropic_model_metadata(model);
        loop {
            let decision = self.model_cache.decision(model, Instant::now()).await;
            let refresh = match decision {
                ModelCacheDecision::Return(metadata) => return metadata.unwrap_or(fallback),
                ModelCacheDecision::Wait(refresh) => refresh,
                ModelCacheDecision::Start { refresh, sender } => {
                    self.spawn_model_refresh(
                        model.to_string(),
                        fallback.clone(),
                        refresh.generation,
                        sender,
                    );
                    refresh
                }
            };
            if let Some(metadata) = wait_for_model_refresh(refresh.clone()).await {
                return metadata.unwrap_or(fallback);
            }
            // A refresh task that is aborted or panics closes its watch channel.
            // Clear only that generation and retry so waiters cannot remain
            // permanently attached to an abandoned in-flight entry.
            self.model_cache
                .abandon_refresh(model, refresh.generation)
                .await;
        }
    }

    fn spawn_model_refresh(
        &self,
        model: String,
        fallback: AnthropicModelMetadata,
        generation: u64,
        sender: watch::Sender<ModelRefreshStatus>,
    ) {
        let provider = self.clone();
        tokio::spawn(async move {
            let resolved = match provider.retrieve_model(&model).await {
                Ok(discovered) => Some(merge_models_api_metadata(fallback, discovered)),
                Err(error) => {
                    eprintln!(
                        "Anthropic Models API lookup failed for {model}; using cached/static fallback: {error}"
                    );
                    None
                }
            };
            let effective = provider
                .model_cache
                .commit_refresh(&model, generation, resolved, Instant::now())
                .await;
            let _ = sender.send(ModelRefreshStatus::Finished(effective));
        });
    }

    async fn retrieve_model(&self, model: &str) -> ProviderResult<ModelsApiModel> {
        let mut url =
            reqwest::Url::parse(&format!("{}/models", self.base_url.trim_end_matches('/')))
                .map_err(|error| {
                    ProviderError::Provider(format!("invalid Anthropic models URL: {error}"))
                })?;
        url.path_segments_mut()
            .map_err(|_| ProviderError::Provider("invalid Anthropic models URL".to_string()))?
            .push(model);
        let response = self
            .client
            .get(url)
            .header("accept", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .header("x-client-request-id", client_request_id())
            .timeout(Duration::from_secs(5))
            .send()
            .await?;
        let (status, text) = response_text(response).await?;
        ensure_success(status, &text, response_error_message)?;
        serde_json::from_str(&text).map_err(|error| {
            ProviderError::Provider(format!(
                "failed to parse Anthropic model response JSON: {error}; body: {}",
                response_excerpt(&text)
            ))
        })
    }
}

#[async_trait]
impl ModelProvider for AnthropicProvider {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        let session_id = request
            .session_id
            .clone()
            .or_else(|| request.prompt_cache_key.clone())
            .unwrap_or_else(|| "pi-relay".to_string());
        let metadata = self.resolved_model_metadata(&request.model).await;
        let prepared = prepare_messages_request(request, &metadata)?;

        let response = send_provider_generation_request(
            self.client
                .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
                .header("accept", "text/event-stream")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", prepared.beta_header)
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("User-Agent", CLAUDE_CODE_USER_AGENT)
                .header("x-app", "cli")
                .header("X-Claude-Code-Session-Id", session_id)
                .header("x-client-request-id", client_request_id())
                .json(&prepared.body),
            "Anthropic /messages",
        )
        .await?;
        parse_anthropic_stream(response).await
    }

    async fn model_metadata(&self, model: &str) -> ProviderResult<Option<ProviderModelMetadata>> {
        let metadata = self.resolved_model_metadata(model).await;
        Ok(Some(ProviderModelMetadata {
            max_input_tokens: metadata.max_input_tokens,
            recommended_auto_compact_tokens: metadata
                .max_input_tokens
                .map(anthropic_auto_compact_limit),
        }))
    }

    async fn compact(
        &self,
        request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        let session_id = request
            .session_id
            .clone()
            .or_else(|| request.prompt_cache_key.clone())
            .unwrap_or_else(|| "pi-relay".to_string());
        let metadata = self.resolved_model_metadata(&request.model).await;
        let body = compaction_body_with_metadata(request, &metadata)?;

        let response = send_provider_generation_request(
            self.client
                .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
                .header("accept", "text/event-stream")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", anthropic_compaction_beta_header())
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("User-Agent", CLAUDE_CODE_USER_AGENT)
                .header("x-app", "cli")
                .header("X-Claude-Code-Session-Id", session_id)
                .header("x-client-request-id", client_request_id())
                .json(&body),
            "Anthropic native compaction /messages",
        )
        .await?;
        parse_anthropic_compaction_stream(response).await
    }

    async fn count_tokens(
        &self,
        request: ProviderTokenCountRequest,
    ) -> ProviderResult<ProviderTokenCountResponse> {
        let session_id = request
            .session_id
            .clone()
            .or_else(|| request.prompt_cache_key.clone())
            .unwrap_or_else(|| "pi-relay".to_string());
        let metadata = self.resolved_model_metadata(&request.model).await;
        let prepared = prepare_count_tokens_request(request, &metadata)?;

        let response = self
            .client
            .post(format!(
                "{}/messages/count_tokens",
                self.base_url.trim_end_matches('/')
            ))
            .header("accept", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", prepared.beta_header)
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .header("X-Claude-Code-Session-Id", session_id)
            .header("x-client-request-id", client_request_id())
            .json(&prepared.body)
            .send()
            .await?;
        let (status, text) = response_text(response).await?;
        ensure_success(status, &text, response_error_message)?;
        parse_anthropic_count_tokens(&text)
    }
}

#[cfg(test)]
fn messages_body(request: ModelRequest) -> ProviderResult<Value> {
    let metadata = static_anthropic_model_metadata(&request.model);
    Ok(prepare_messages_request(request, &metadata)?.body)
}

#[cfg(test)]
fn messages_body_with_metadata(
    request: ModelRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<Value> {
    Ok(prepare_messages_request(request, metadata)?.body)
}

struct PreparedAnthropicRequest {
    body: Value,
    beta_header: String,
}

fn prepare_messages_request(
    request: ModelRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<PreparedAnthropicRequest> {
    let tool_profile = request.tool_profile;
    // The Messages API requires `max_tokens`. Keep 64k as the ordinary-turn
    // target recommended for xhigh/max agentic work, but clamp both defaults
    // and explicit overrides to the model's authoritative output ceiling.
    let max_tokens = request
        .max_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_BUDGET)
        .min(metadata.max_tokens);
    let mut rendered = anthropic_request_body(AnthropicRequestBodyInput {
        model: request.model,
        prompt: request.prompt,
        transcript: request.transcript,
        tool_profile,
        tools: crate::effective_provider_tools(tool_profile, request.tools),
        max_tokens: Some(max_tokens),
        reasoning_effort: Some(request.reasoning_effort),
        capabilities: metadata.capabilities,
        cache_transcript: true,
        transcript_cache_prefix_len: request.transcript_cache_prefix_len,
    })?;
    apply_messages_compaction_replay_strategy(
        &mut rendered.body,
        rendered.replays_compaction,
        metadata,
    )?;
    Ok(rendered.prepare())
}

#[cfg(test)]
fn count_tokens_body(request: ProviderTokenCountRequest) -> ProviderResult<Value> {
    let metadata = static_anthropic_model_metadata(&request.model);
    Ok(prepare_count_tokens_request(request, &metadata)?.body)
}

#[cfg(test)]
fn count_tokens_body_with_metadata(
    request: ProviderTokenCountRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<Value> {
    Ok(prepare_count_tokens_request(request, metadata)?.body)
}

fn prepare_count_tokens_request(
    request: ProviderTokenCountRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<PreparedAnthropicRequest> {
    // Keep this as close as possible to `messages_body`: Anthropic's token
    // count endpoint accepts the same input-shaping fields (system, tools,
    // thinking/output config) but does not need a generation budget.
    let tool_profile = request.tool_profile;
    let mut rendered = anthropic_request_body(AnthropicRequestBodyInput {
        model: request.model,
        prompt: request.prompt,
        transcript: request.transcript,
        tool_profile,
        tools: crate::effective_provider_tools(tool_profile, request.tools),
        // Anthropic's /messages/count_tokens endpoint accepts the same prompt,
        // message, thinking, and tool-shaping fields as /messages, but rejects
        // generation-only budgets such as max_tokens.
        max_tokens: None,
        reasoning_effort: Some(request.reasoning_effort),
        capabilities: metadata.capabilities,
        cache_transcript: false,
        transcript_cache_prefix_len: None,
    })?;
    apply_count_compaction_replay_strategy(&mut rendered.body, rendered.replays_compaction);
    Ok(rendered.prepare())
}

struct AnthropicRequestBodyInput {
    model: String,
    prompt: crate::PromptSections,
    transcript: Vec<ModelTranscriptEntry>,
    tool_profile: ProviderToolProfile,
    tools: Vec<ProviderTool>,
    max_tokens: Option<u32>,
    reasoning_effort: Option<ReasoningEffort>,
    capabilities: AnthropicModelCapabilities,
    cache_transcript: bool,
    transcript_cache_prefix_len: Option<usize>,
}

struct RenderedAnthropicRequest {
    body: Value,
    replays_compaction: bool,
}

impl RenderedAnthropicRequest {
    fn prepare(self) -> PreparedAnthropicRequest {
        PreparedAnthropicRequest {
            body: self.body,
            beta_header: if self.replays_compaction {
                anthropic_compaction_beta_header()
            } else {
                anthropic_beta_header().to_string()
            },
        }
    }
}

fn anthropic_request_body(
    input: AnthropicRequestBodyInput,
) -> ProviderResult<RenderedAnthropicRequest> {
    let capabilities = input.capabilities;
    let rendered = transcript_to_messages_for_request(&input)?;
    let mut body = json!({
        "model": input.model,
        "messages": rendered.messages,
    });
    if let Some(max_tokens) = input.max_tokens {
        body["max_tokens"] = json!(max_tokens);
    }
    if let Some(reasoning_effort) = input.reasoning_effort.filter(|_| capabilities.effort) {
        let effort = anthropic_reasoning_effort(capabilities, reasoning_effort)?;
        // Adaptive thinking is intentionally hard-coded and must not become a
        // per-request toggle: Anthropic invalidates the message-content cache
        // whenever the `thinking` parameter changes (enabling/disabling or
        // budget changes). Reasoning effort lives in `output_config` instead,
        // which is documented not to affect the messages-level cache.
        // See: https://docs.claude.com/en/docs/build-with-claude/prompt-caching
        // Fable 5 always thinks and Sonnet 5 defaults to adaptive thinking, so
        // their canonical shape omits the redundant `thinking` field. Opus
        // 4.8 requires an explicit adaptive mode; omission turns thinking off.
        if capabilities.adaptive_thinking && !capabilities.adaptive_thinking_default {
            body["thinking"] = json!({ "type": "adaptive" });
        }
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
    if input.max_tokens.is_some() {
        body["stream"] = json!(true);
    }
    Ok(RenderedAnthropicRequest {
        body,
        replays_compaction: rendered.replays_compaction,
    })
}

struct RenderedAnthropicMessages {
    messages: Vec<Value>,
    replays_compaction: bool,
}

fn transcript_to_messages_for_request(
    input: &AnthropicRequestBodyInput,
) -> ProviderResult<RenderedAnthropicMessages> {
    if !input.cache_transcript {
        let mut rendered = render_transcript_messages(&input.prompt, &input.transcript)?;
        append_dynamic_context_message(&input.prompt, &mut rendered.messages);
        return Ok(rendered);
    }
    let Some(prefix_len) = input.transcript_cache_prefix_len else {
        let mut rendered = render_transcript_messages(&input.prompt, &input.transcript)?;
        add_transcript_cache_breakpoints(&mut rendered.messages);
        append_dynamic_context_message(&input.prompt, &mut rendered.messages);
        return Ok(rendered);
    };

    let prefix_len = prefix_len.min(input.transcript.len());
    let (prefix, suffix) = input.transcript.split_at(prefix_len);
    let mut rendered = render_transcript_messages(&input.prompt, prefix)?;
    add_transcript_cache_breakpoints(&mut rendered.messages);
    let suffix = render_transcript_messages(&input.prompt, suffix)?;
    rendered.messages.extend(suffix.messages);
    rendered.replays_compaction |= suffix.replays_compaction;
    append_dynamic_context_message(&input.prompt, &mut rendered.messages);
    Ok(rendered)
}

fn append_dynamic_context_message(prompt: &crate::PromptSections, messages: &mut Vec<Value>) {
    if let Some(dynamic) = prompt
        .dynamic_context
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        messages.push(json!({
            "role": "user",
            "content": [{ "type": "text", "text": dynamic }],
        }));
    }
}

fn client_request_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    format!("pi-relay-{nanos}")
}

fn anthropic_reasoning_effort(
    capabilities: AnthropicModelCapabilities,
    effort: ReasoningEffort,
) -> ProviderResult<&'static str> {
    let effort = match effort {
        // Preserve the daemon's historical Claude normalization inside the
        // adapter now that provider-neutral core passes raw configured intent.
        ReasoningEffort::None | ReasoningEffort::Minimal => ReasoningEffort::Low,
        effort => effort,
    };
    if capabilities.supports_effort(effort) {
        return Ok(effort.as_str());
    }
    Err(ProviderError::Provider(format!(
        "reasoning effort {} is not supported by this Claude model",
        effort.as_str()
    )))
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
/// cache. We fall back to a digest of the first user text only when a caller
/// truly supplies no stable prefix; normal daemon and provider-native
/// compaction requests supply stable prompt sections.
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
    // `tail_index` is the count of cacheable blocks up to and including the tail,
    // so the total cacheable-block count is exactly `tail_index`.
    let total_cacheable = tail_index;
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

fn render_transcript_messages(
    prompt: &crate::PromptSections,
    items: &[ModelTranscriptEntry],
) -> ProviderResult<RenderedAnthropicMessages> {
    let mut messages = Vec::new();
    let mut replays_compaction = false;
    for entry in items {
        match entry.item() {
            TranscriptItem::UserMessage(message) => {
                messages
                    .push(json!({ "role": "user", "content": anthropic_user_content(message) }));
            }
            TranscriptItem::CompactionSummary(summary) => {
                let (replay, has_compaction) = emitted_anthropic_replay(entry, true)?;
                if !replay.is_empty() {
                    replays_compaction |= has_compaction;
                    messages.push(json!({ "role": "assistant", "content": replay }));
                }
                messages.push(json!({
                    "role": "user",
                    "content": [{ "type": "text", "text": compaction_summary_text(summary, prompt) }],
                }));
            }
            TranscriptItem::AssistantMessage(message) => {
                let (mut content, has_compaction) = emitted_anthropic_replay(entry, false)?;
                replays_compaction |= has_compaction;
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
            TranscriptItem::DaemonToolObservation(observation) => {
                let tool_use_id = anthropic_daemon_tool_use_id(observation.tool_call_id.as_str());
                messages.push(json!({
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": tool_use_id,
                        "name": anthropic_wire_tool_name(&observation.tool_name),
                        "input": observation.args_value().unwrap_or_else(|_| json!({})),
                    }],
                }));
                messages.push(json!({
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": tool_use_id,
                        "content": observation.result_text()?,
                        "is_error": matches!(observation.status, agent_vocab::ToolResultStatus::Error | agent_vocab::ToolResultStatus::Interrupted | agent_vocab::ToolResultStatus::Crashed),
                    }],
                }));
            }
            TranscriptItem::TurnStarted { .. }
            | TranscriptItem::ToolCallStarted { .. }
            | TranscriptItem::TurnFinished { .. } => {}
        }
    }
    Ok(RenderedAnthropicMessages {
        messages,
        replays_compaction,
    })
}

#[cfg(test)]
fn transcript_to_messages(
    prompt: &crate::PromptSections,
    items: &[ModelTranscriptEntry],
) -> ProviderResult<Vec<Value>> {
    Ok(render_transcript_messages(prompt, items)?.messages)
}

fn emitted_anthropic_replay(
    entry: &ModelTranscriptEntry,
    compaction_summary: bool,
) -> ProviderResult<(Vec<Value>, bool)> {
    let blocks = entry
        .provider_replay_values_for(ProviderKind::Claude)
        .map_err(ProviderError::Json)?;
    if compaction_summary {
        if blocks.len() != 1 {
            return Err(ProviderError::Provider(
                "refusing malformed persisted Anthropic compaction replay: expected exactly one Claude block"
                    .to_string(),
            ));
        }
        validate_anthropic_compaction_block(&blocks[0]).map_err(|error| {
            ProviderError::Provider(format!(
                "refusing malformed persisted Anthropic compaction replay: {}",
                error.message()
            ))
        })?;
        return Ok((blocks, true));
    }

    let mut has_compaction = false;
    for block in &blocks {
        let block_type = anthropic_block_type(block, "persisted Anthropic replay block")?;
        if block_type == "compaction" {
            validate_anthropic_compaction_block(block).map_err(|error| {
                ProviderError::Provider(format!(
                    "refusing malformed persisted Anthropic compaction replay: {}",
                    error.message()
                ))
            })?;
            has_compaction = true;
        }
    }
    Ok((blocks, has_compaction))
}

fn anthropic_block_type<'a>(block: &'a Value, context: &str) -> ProviderResult<&'a str> {
    let object = block
        .as_object()
        .ok_or_else(|| ProviderError::Provider(format!("{context} was not an object")))?;
    object
        .get("type")
        .and_then(Value::as_str)
        .filter(|block_type| !block_type.is_empty())
        .ok_or_else(|| ProviderError::Provider(format!("{context} missing nonempty string type")))
}

fn anthropic_daemon_tool_use_id(tool_call_id: &str) -> String {
    if tool_call_id.starts_with("toolu_") {
        return tool_call_id.to_string();
    }
    let sanitized = tool_call_id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    format!("toolu_{sanitized}")
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

async fn parse_anthropic_stream(response: reqwest::Response) -> ProviderResult<ModelResponse> {
    let mut state = AnthropicStreamState::default();
    read_provider_json_sse_response(
        response,
        "Anthropic response stream",
        response_error_message,
        |event| state.process_sse_event(event),
    )
    .await?;
    state.finish()
}

async fn parse_anthropic_compaction_stream(
    response: reqwest::Response,
) -> ProviderResult<ProviderCompactionResponse> {
    let mut state = AnthropicCompactionStreamState::default();
    read_provider_json_sse_response(
        response,
        "Anthropic native compaction response stream",
        response_error_message,
        |event| state.process_sse_event(event),
    )
    .await?;
    state.finish()
}

#[cfg(test)]
fn parse_anthropic_sse(text: &str) -> ProviderResult<ModelResponse> {
    let mut state = AnthropicStreamState::default();
    read_json_sse_text(text, |event| state.process_sse_event(event))?;
    state.finish()
}

#[cfg(test)]
fn parse_anthropic_compaction_sse(text: &str) -> ProviderResult<ProviderCompactionResponse> {
    let mut state = AnthropicCompactionStreamState::default();
    read_json_sse_text(text, |event| state.process_sse_event(event))?;
    state.finish()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum AnthropicCompactionFrame {
    #[default]
    MessageStart,
    ContentBlockStart,
    ContentBlockDelta,
    ContentBlockStop,
    MessageDelta,
    Terminal,
}

struct AnthropicCompactionStreamState {
    expected: AnthropicCompactionFrame,
    block_index: Option<usize>,
    compaction_block: Option<Value>,
    usage: Option<ProviderUsage>,
    saw_compaction_stop_reason: bool,
}

impl Default for AnthropicCompactionStreamState {
    fn default() -> Self {
        Self {
            expected: AnthropicCompactionFrame::MessageStart,
            block_index: None,
            compaction_block: None,
            usage: None,
            saw_compaction_stop_reason: false,
        }
    }
}

impl AnthropicCompactionStreamState {
    fn malformed(message: impl Into<String>) -> ProviderError {
        ProviderError::native_compaction(NativeCompactionErrorKind::MalformedStream, message)
    }

    fn process_sse_event(&mut self, event: SseEvent) -> ProviderResult<SseControl> {
        match event {
            SseEvent::Json(event) => self.process_event(&event),
            SseEvent::MalformedJson => Err(Self::malformed(
                "stream contained malformed JSON event data",
            )),
            SseEvent::Done => Err(Self::malformed(
                "stream used [DONE] instead of the required message_stop event",
            )),
        }
    }

    fn process_event(&mut self, event: &Value) -> ProviderResult<SseControl> {
        let event_type = event
            .get("type")
            .and_then(Value::as_str)
            .ok_or_else(|| Self::malformed("stream event missing string type"))?;
        if event_type == "error" {
            let error_type = event.pointer("/error/type").and_then(Value::as_str);
            let message = anthropic_error_message(
                error_type,
                event
                    .pointer("/error/message")
                    .or_else(|| event.get("message"))
                    .and_then(Value::as_str),
                event,
            );
            return Err(anthropic_stream_provider_error(error_type, message));
        }
        if event_type == "ping"
            || !matches!(
                event_type,
                "message_start"
                    | "content_block_start"
                    | "content_block_delta"
                    | "content_block_stop"
                    | "message_delta"
                    | "message_stop"
            )
        {
            // Anthropic may intersperse pings and add new event types. They do
            // not advance the structural state of this deliberately strict
            // parser.
            return Ok(SseControl::Continue);
        }
        match (self.expected, event_type) {
            (AnthropicCompactionFrame::MessageStart, "message_start") => {
                let message = event
                    .get("message")
                    .and_then(Value::as_object)
                    .ok_or_else(|| Self::malformed("message_start missing message object"))?;
                self.usage = message.get("usage").and_then(anthropic_usage);
                self.expected = AnthropicCompactionFrame::ContentBlockStart;
            }
            (AnthropicCompactionFrame::ContentBlockStart, "content_block_start") => {
                let index = event
                    .get("index")
                    .and_then(Value::as_u64)
                    .and_then(|index| usize::try_from(index).ok())
                    .ok_or_else(|| {
                        Self::malformed("compaction content_block_start missing valid index")
                    })?;
                if index != 0 {
                    return Err(Self::malformed(format!(
                        "sole compaction content block must use index 0, received {index}"
                    )));
                }
                let block = event
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        Self::malformed("compaction content_block_start missing content_block")
                    })?;
                if block.get("type").and_then(Value::as_str) != Some("compaction") {
                    return Err(ProviderError::native_compaction(
                        NativeCompactionErrorKind::UnexpectedContent,
                        "compaction stream started a non-compaction content block",
                    ));
                }
                match block.get("content") {
                    None | Some(Value::Null) => {}
                    Some(Value::String(value)) if value.is_empty() => {}
                    Some(_) => {
                        return Err(Self::malformed(
                            "compaction content_block_start contained pre-populated content",
                        ))
                    }
                }
                if !matches!(
                    block.get("encrypted_content"),
                    None | Some(Value::Null) | Some(Value::String(_))
                ) {
                    return Err(Self::malformed(
                        "compaction content_block_start encrypted_content was not string or null",
                    ));
                }
                self.block_index = Some(index);
                self.compaction_block = Some(Value::Object(block.clone()));
                self.expected = AnthropicCompactionFrame::ContentBlockDelta;
            }
            (AnthropicCompactionFrame::ContentBlockDelta, "content_block_delta") => {
                self.require_matching_index(event, "content_block_delta")?;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| Self::malformed("compaction delta missing delta object"))?;
                if delta.get("type").and_then(Value::as_str) != Some("compaction_delta") {
                    return Err(Self::malformed(
                        "compaction stream contained a non-compaction or unknown delta type",
                    ));
                }
                let block = self
                    .compaction_block
                    .as_mut()
                    .and_then(Value::as_object_mut)
                    .expect("compaction block exists after content_block_start");
                let content = delta
                    .get("content")
                    .ok_or_else(|| Self::malformed("compaction_delta missing required content"))?;
                if !matches!(content, Value::Null | Value::String(_)) {
                    return Err(Self::malformed(
                        "compaction_delta content was not string or null",
                    ));
                }
                block.insert("content".to_string(), content.clone());
                if let Some(encrypted_content) = delta.get("encrypted_content") {
                    if !matches!(encrypted_content, Value::Null | Value::String(_)) {
                        return Err(Self::malformed(
                            "compaction_delta encrypted_content was not string or null",
                        ));
                    }
                    block.insert("encrypted_content".to_string(), encrypted_content.clone());
                }
                for (field, value) in delta {
                    if !matches!(field.as_str(), "type" | "content" | "encrypted_content") {
                        block.insert(field.clone(), value.clone());
                    }
                }
                self.expected = AnthropicCompactionFrame::ContentBlockStop;
            }
            (AnthropicCompactionFrame::ContentBlockStop, "content_block_stop") => {
                self.require_matching_index(event, "content_block_stop")?;
                self.expected = AnthropicCompactionFrame::MessageDelta;
            }
            (AnthropicCompactionFrame::MessageDelta, "message_delta") => {
                let delta = event
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| Self::malformed("message_delta missing delta object"))?;
                match delta.get("stop_reason") {
                    None | Some(Value::Null) => {}
                    Some(Value::String(reason))
                        if reason == "compaction" && !self.saw_compaction_stop_reason =>
                    {
                        self.saw_compaction_stop_reason = true;
                    }
                    Some(Value::String(reason)) => {
                        return Err(ProviderError::native_compaction(
                            NativeCompactionErrorKind::UnexpectedStopReason,
                            format!(
                                "conflicting or duplicate message_delta stop_reason {reason:?}"
                            ),
                        ))
                    }
                    Some(other) => {
                        return Err(Self::malformed(format!(
                            "message_delta stop_reason was not string or null: {other}"
                        )))
                    }
                }
                if let Some(usage) = event.get("usage").and_then(anthropic_usage) {
                    merge_anthropic_usage(&mut self.usage, usage);
                }
            }
            (AnthropicCompactionFrame::MessageDelta, "message_stop")
                if self.saw_compaction_stop_reason =>
            {
                self.expected = AnthropicCompactionFrame::Terminal;
            }
            (AnthropicCompactionFrame::MessageDelta, "message_stop") => {
                return Err(ProviderError::native_compaction(
                    NativeCompactionErrorKind::UnexpectedStopReason,
                    "compaction stream ended without stop_reason compaction",
                ))
            }
            (AnthropicCompactionFrame::Terminal, _) => {
                return Err(Self::malformed(format!(
                    "stream contained trailing {event_type} after message_stop"
                )))
            }
            (expected, actual) => {
                return Err(Self::malformed(format!(
                    "expected {expected:?}, received {actual}"
                )))
            }
        }
        // Unlike ordinary generation, consume through EOF so trailing frames
        // after message_stop cannot be hidden by early termination.
        Ok(SseControl::Continue)
    }

    fn require_matching_index(&self, event: &Value, event_type: &str) -> ProviderResult<()> {
        let index = event
            .get("index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| Self::malformed(format!("{event_type} missing valid index")))?;
        if Some(index) != self.block_index {
            return Err(Self::malformed(format!(
                "{event_type} index {index} did not match content_block_start index {:?}",
                self.block_index
            )));
        }
        Ok(())
    }

    fn finish(self) -> ProviderResult<ProviderCompactionResponse> {
        if self.expected != AnthropicCompactionFrame::Terminal {
            return Err(Self::malformed(format!(
                "truncated compaction stream; expected {:?}",
                self.expected
            )));
        }
        let block = self
            .compaction_block
            .expect("terminal compaction frame requires a constructed block");
        validate_anthropic_compaction_block(&block).map_err(|error| {
            let kind = match error {
                AnthropicCompactionBlockError::NullContent => NativeCompactionErrorKind::NullBlock,
                AnthropicCompactionBlockError::EmptyContent => {
                    NativeCompactionErrorKind::EmptyBlock
                }
                AnthropicCompactionBlockError::WrongType
                | AnthropicCompactionBlockError::MissingContent
                | AnthropicCompactionBlockError::NonStringContent
                | AnthropicCompactionBlockError::InvalidEncryptedContent => {
                    NativeCompactionErrorKind::MalformedStream
                }
            };
            ProviderError::native_compaction(
                kind,
                format!("invalid Anthropic compaction block: {}", error.message()),
            )
        })?;
        let replay = ProviderReplayItem::new(ProviderKind::Claude, &block)?;
        Ok(ProviderCompactionResponse {
            summary: None,
            provider_replay: vec![replay],
            usage: self.usage,
        })
    }
}

struct AnthropicStreamState {
    active_content_block: Option<(usize, Value)>,
    next_content_block_index: usize,
    provider_replay: Vec<ProviderReplayItem>,
    items: Vec<AssistantItem>,
    usage: Option<ProviderUsage>,
    stop_reason: ModelStopReason,
    stop_details: Option<ModelStopDetails>,
    message_started: bool,
    terminal_stop_reason: Option<String>,
    message_stopped: bool,
}

impl Default for AnthropicStreamState {
    fn default() -> Self {
        Self {
            active_content_block: None,
            next_content_block_index: 0,
            provider_replay: Vec::new(),
            items: Vec::new(),
            usage: None,
            stop_reason: ModelStopReason::Complete,
            stop_details: None,
            message_started: false,
            terminal_stop_reason: None,
            message_stopped: false,
        }
    }
}

impl AnthropicStreamState {
    fn require_content_phase(&self, event_type: &str) -> ProviderResult<()> {
        if !self.message_started {
            return Err(ProviderError::Provider(format!(
                "Anthropic {event_type} arrived before message_start"
            )));
        }
        if self.terminal_stop_reason.is_some() {
            return Err(ProviderError::Provider(format!(
                "Anthropic {event_type} arrived after terminal stop_reason"
            )));
        }
        Ok(())
    }

    fn process_sse_event(&mut self, event: SseEvent) -> ProviderResult<SseControl> {
        match event {
            SseEvent::Json(event) => self.process_event(&event),
            SseEvent::MalformedJson => Err(ProviderError::Provider(
                "Anthropic response stream contained malformed JSON event data".to_string(),
            )),
            SseEvent::Done => Ok(SseControl::Continue),
        }
    }

    fn process_event(&mut self, event: &Value) -> ProviderResult<SseControl> {
        reject_ordinary_compaction_event(event)?;
        match event.get("type").and_then(Value::as_str) {
            Some("message_start") => {
                if self.message_started {
                    return Err(ProviderError::Provider(
                        "Anthropic response stream contained duplicate message_start".to_string(),
                    ));
                }
                self.message_started = true;
                self.usage = event
                    .get("message")
                    .and_then(|message| message.get("usage"))
                    .and_then(anthropic_usage);
                Ok(SseControl::Continue)
            }
            Some("content_block_start") => {
                self.require_content_phase("content_block_start")?;
                let index = anthropic_stream_index(event, "content_block_start")?;
                let content_block = event
                    .get("content_block")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ProviderError::Provider(
                            "Anthropic content_block_start missing content_block object"
                                .to_string(),
                        )
                    })?;
                self.start_content_block(index, &Value::Object(content_block.clone()))?;
                Ok(SseControl::Continue)
            }
            Some("content_block_delta") => {
                self.require_content_phase("content_block_delta")?;
                let index = anthropic_stream_index(event, "content_block_delta")?;
                let delta = event
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ProviderError::Provider(
                            "Anthropic content_block_delta missing delta object".to_string(),
                        )
                    })?;
                self.apply_content_delta(index, &Value::Object(delta.clone()))?;
                Ok(SseControl::Continue)
            }
            Some("content_block_stop") => {
                self.require_content_phase("content_block_stop")?;
                let index = anthropic_stream_index(event, "content_block_stop")?;
                self.finish_content_block(index)?;
                Ok(SseControl::Continue)
            }
            Some("message_delta") => {
                if !self.message_started {
                    return Err(ProviderError::Provider(
                        "Anthropic message_delta arrived before message_start".to_string(),
                    ));
                }
                if let Some((index, _)) = self.active_content_block.as_ref() {
                    return Err(ProviderError::Provider(format!(
                        "Anthropic message_delta arrived before content_block_stop for index {index}"
                    )));
                }
                let delta = event
                    .get("delta")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ProviderError::Provider(
                            "Anthropic message_delta missing delta object".to_string(),
                        )
                    })?;
                let has_usage = event.get("usage").is_some();
                if let Some(usage) = anthropic_message_delta_usage(event.get("usage"))? {
                    merge_anthropic_usage(&mut self.usage, usage);
                }
                let stop_details = anthropic_stop_details(delta.get("stop_details"))?;
                let Some(stop_reason) = delta.get("stop_reason") else {
                    if stop_details.is_some() {
                        return Err(ProviderError::Provider(
                            "Anthropic message_delta had stop_details without a terminal stop_reason"
                                .to_string(),
                        ));
                    }
                    if !has_usage {
                        return Err(ProviderError::Provider(
                            "Anthropic message_delta had neither stop_reason nor usage".to_string(),
                        ));
                    }
                    return Ok(SseControl::Continue);
                };
                let Value::String(stop_reason) = stop_reason else {
                    if stop_reason.is_null() {
                        if stop_details.is_some() {
                            return Err(ProviderError::Provider(
                                "Anthropic message_delta had stop_details without a terminal stop_reason"
                                    .to_string(),
                            ));
                        }
                        return Ok(SseControl::Continue);
                    }
                    return Err(ProviderError::Provider(format!(
                        "Anthropic message_delta stop_reason was not a string or null: {stop_reason}"
                    )));
                };
                if stop_reason.is_empty() {
                    return Err(ProviderError::Provider(
                        "Anthropic message_delta stop_reason was empty".to_string(),
                    ));
                }
                if let Some(existing) = self.terminal_stop_reason.as_deref() {
                    if existing != stop_reason {
                        return Err(ProviderError::Provider(format!(
                            "Anthropic response stream contained conflicting terminal stop reasons: {existing:?} and {stop_reason:?}"
                        )));
                    }
                }
                if let Some(details) = stop_details {
                    merge_anthropic_stop_details(&mut self.stop_details, details)?;
                }
                self.stop_reason = match stop_reason.as_str() {
                    "end_turn" | "stop_sequence" | "tool_use" => ModelStopReason::Complete,
                    "max_tokens" => ModelStopReason::MaxOutputTokens,
                    "refusal" => ModelStopReason::Refusal,
                    "pause_turn" | "model_context_window_exceeded" => {
                        return Err(ProviderError::Incomplete {
                            status: "incomplete".to_string(),
                            reason: stop_reason.clone(),
                        });
                    }
                    _ => {
                        return Err(ProviderError::Incomplete {
                            status: "unknown_stop_reason".to_string(),
                            reason: stop_reason.clone(),
                        });
                    }
                };
                if self.terminal_stop_reason.is_none() {
                    self.terminal_stop_reason = Some(stop_reason.clone());
                }
                Ok(SseControl::Continue)
            }
            Some("message_stop") => {
                if !self.message_started {
                    return Err(ProviderError::Provider(
                        "Anthropic message_stop arrived before message_start".to_string(),
                    ));
                }
                if let Some((index, _)) = self.active_content_block.as_ref() {
                    return Err(ProviderError::Provider(format!(
                        "Anthropic message_stop arrived before content_block_stop for index {index}"
                    )));
                }
                if self.terminal_stop_reason.is_none() {
                    return Err(ProviderError::Provider(
                        "Anthropic message_stop arrived without a recognized terminal stop_reason"
                            .to_string(),
                    ));
                }
                self.message_stopped = true;
                Ok(SseControl::Stop)
            }
            Some("error") => {
                let error_type = event.pointer("/error/type").and_then(Value::as_str);
                let message = anthropic_error_message(
                    error_type,
                    event
                        .pointer("/error/message")
                        .or_else(|| event.get("message"))
                        .and_then(Value::as_str),
                    event,
                );
                Err(anthropic_stream_provider_error(error_type, message))
            }
            Some("ping") | None => Ok(SseControl::Continue),
            Some(_) => Ok(SseControl::Continue),
        }
    }

    fn start_content_block(&mut self, index: usize, block: &Value) -> ProviderResult<()> {
        if let Some((active_index, _)) = self.active_content_block.as_ref() {
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_start for index {index} arrived before content_block_stop for index {active_index}"
            )));
        }
        if index != self.next_content_block_index {
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_start index was not contiguous: expected {}, found {index}",
                self.next_content_block_index
            )));
        }
        validate_anthropic_stream_content_start(block)?;
        self.active_content_block = Some((index, normalize_stream_content_start(block)));
        Ok(())
    }

    fn apply_content_delta(&mut self, index: usize, delta: &Value) -> ProviderResult<()> {
        let Some((active_index, block)) = self.active_content_block.as_mut() else {
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_delta referenced nonexistent block index {index}"
            )));
        };
        if index != *active_index {
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_delta index {index} did not match active block index {active_index}"
            )));
        }
        let block_type = anthropic_block_type(block, "Anthropic streamed content block")?;
        let delta_type = delta
            .get("type")
            .and_then(Value::as_str)
            .filter(|delta_type| !delta_type.is_empty())
            .ok_or_else(|| {
                ProviderError::Provider(
                    "Anthropic content_block_delta missing nonempty delta type".to_string(),
                )
            })?;
        match delta_type {
            "input_json_delta" if matches!(block_type, "tool_use" | "server_tool_use") => {
                append_required_json_string_field(block, "input", delta, "partial_json")?;
            }
            "text_delta" if block_type == "text" => {
                append_required_json_string_field(block, "text", delta, "text")?;
            }
            "thinking_delta" if block_type == "thinking" => {
                if block
                    .get("signature")
                    .and_then(Value::as_str)
                    .is_some_and(|signature| !signature.is_empty())
                {
                    return Err(ProviderError::Provider(
                        "Anthropic thinking_delta arrived after signature_delta".to_string(),
                    ));
                }
                append_required_json_string_field(block, "thinking", delta, "thinking")?;
            }
            "signature_delta" if block_type == "thinking" => {
                let signature = required_anthropic_delta_string(delta, "signature")?;
                if signature.is_empty() {
                    return Err(ProviderError::Provider(
                        "Anthropic signature_delta contained an empty signature".to_string(),
                    ));
                }
                if block
                    .get("signature")
                    .and_then(Value::as_str)
                    .is_some_and(|signature| !signature.is_empty())
                {
                    return Err(ProviderError::Provider(
                        "Anthropic thinking block received duplicate signature_delta".to_string(),
                    ));
                }
                block["signature"] = Value::String(signature.to_string());
            }
            "citations_delta" if block_type == "text" => {
                let citation = delta
                    .get("citation")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ProviderError::Provider(
                            "Anthropic citations_delta missing citation object".to_string(),
                        )
                    })?;
                anthropic_block_type(
                    &Value::Object(citation.clone()),
                    "Anthropic citations_delta citation",
                )?;
                match block.get_mut("citations") {
                    Some(Value::Array(citations)) => {
                        citations.push(Value::Object(citation.clone()));
                    }
                    Some(Value::Null) | None => {
                        block["citations"] = Value::Array(vec![Value::Object(citation.clone())]);
                    }
                    Some(_) => {
                        return Err(ProviderError::Provider(
                            "Anthropic text content block citations was not an array or null"
                                .to_string(),
                        ))
                    }
                }
            }
            "input_json_delta" | "text_delta" | "thinking_delta" | "signature_delta"
            | "citations_delta" => {
                return Err(ProviderError::Provider(format!(
                    "Anthropic {delta_type} was invalid for content block type {block_type}"
                )))
            }
            _ => {
                return Err(ProviderError::Provider(format!(
                    "Anthropic content_block_delta had unsupported delta type {delta_type}"
                )))
            }
        };
        Ok(())
    }

    fn finish_content_block(&mut self, index: usize) -> ProviderResult<()> {
        let Some((active_index, block)) = self.active_content_block.take() else {
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_stop referenced nonexistent block index {index}"
            )));
        };
        if index != active_index {
            self.active_content_block = Some((active_index, block));
            return Err(ProviderError::Provider(format!(
                "Anthropic content_block_stop index {index} did not match active block index {active_index}"
            )));
        }
        let block = finalize_stream_content_block(block)?;
        push_anthropic_content_block(&block, &mut self.items, &mut self.provider_replay)
            .map(|()| self.next_content_block_index += 1)
    }

    fn finish(mut self) -> ProviderResult<ModelResponse> {
        if !self.message_started {
            return Err(ProviderError::Provider(
                "Anthropic response stream ended without message_start".to_string(),
            ));
        }
        if !self.message_stopped {
            return Err(ProviderError::Provider(
                "Anthropic response stream ended before message_stop".to_string(),
            ));
        }
        if self.terminal_stop_reason.is_none() {
            return Err(ProviderError::Provider(
                "Anthropic response stream ended without a recognized stop_reason".to_string(),
            ));
        }
        if self.stop_reason == ModelStopReason::Refusal {
            // Anthropic can classify a Fable response after streaming partial
            // text, thinking, or tool blocks. The entire partial attempt is
            // incomplete and must not be persisted or replayed.
            self.active_content_block = None;
            self.items.clear();
            self.provider_replay.clear();
        }
        if self.stop_reason == ModelStopReason::Compaction
            || self
                .provider_replay
                .iter()
                .any(|item| item.raw_type().as_deref() == Some("compaction"))
        {
            return Err(reject_ordinary_anthropic_compaction());
        }
        Ok(ModelResponse {
            assistant: AssistantMessage { items: self.items },
            provider_replay: self.provider_replay,
            usage: self.usage,
            stop_reason: self.stop_reason,
            stop_details: self.stop_details,
        })
    }
}

fn reject_ordinary_compaction_event(event: &Value) -> ProviderResult<()> {
    let is_compaction = match event.get("type").and_then(Value::as_str) {
        Some("message_start") => event
            .pointer("/message/content")
            .and_then(Value::as_array)
            .is_some_and(|content| {
                content
                    .iter()
                    .any(|block| block.get("type").and_then(Value::as_str) == Some("compaction"))
            }),
        Some("content_block_start") => {
            event.pointer("/content_block/type").and_then(Value::as_str) == Some("compaction")
        }
        Some("content_block_delta") => {
            event.pointer("/delta/type").and_then(Value::as_str) == Some("compaction_delta")
        }
        Some("content_block_stop") => false,
        Some("message_delta") => {
            event.pointer("/delta/stop_reason").and_then(Value::as_str) == Some("compaction")
        }
        _ => false,
    };
    if is_compaction {
        Err(reject_ordinary_anthropic_compaction())
    } else {
        Ok(())
    }
}

fn normalize_stream_content_start(block: &Value) -> Value {
    let mut block = block.clone();
    match block.get("type").and_then(Value::as_str) {
        Some("tool_use") | Some("server_tool_use") => {
            block["input"] = Value::String(String::new());
        }
        Some("text") => {
            block["text"] = Value::String(String::new());
        }
        Some("thinking") => {
            block["thinking"] = Value::String(String::new());
            block["signature"] = Value::String(String::new());
        }
        _ => {}
    }
    block
}

fn finalize_stream_content_block(mut block: Value) -> ProviderResult<Value> {
    if let Some("tool_use" | "server_tool_use") = block.get("type").and_then(Value::as_str) {
        let input = block.get("input").and_then(Value::as_str).ok_or_else(|| {
            ProviderError::Provider(
                "Anthropic streamed tool content block input was not a string".to_string(),
            )
        })?;
        block["input"] = parse_streamed_json_object(input)?;
    }
    if block.get("type").and_then(Value::as_str) == Some("thinking")
        && block
            .get("signature")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(ProviderError::Provider(
            "Anthropic thinking content block ended without signature_delta".to_string(),
        ));
    }
    Ok(block)
}

fn parse_streamed_json_object(input: &str) -> ProviderResult<Value> {
    if input.is_empty() {
        return Ok(json!({}));
    }
    let value = serde_json::from_str::<Value>(input).map_err(|error| {
        ProviderError::Provider(format!(
            "Anthropic streamed tool input was malformed JSON: {error}"
        ))
    })?;
    if !value.is_object() {
        return Err(ProviderError::Provider(
            "Anthropic streamed tool input was not a JSON object".to_string(),
        ));
    }
    Ok(value)
}

fn required_anthropic_delta_string<'a>(delta: &'a Value, field: &str) -> ProviderResult<&'a str> {
    delta.get(field).and_then(Value::as_str).ok_or_else(|| {
        ProviderError::Provider(format!(
            "Anthropic {} missing string {field}",
            delta
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("content block delta")
        ))
    })
}

fn append_required_json_string_field(
    block: &mut Value,
    block_field: &str,
    delta: &Value,
    delta_field: &str,
) -> ProviderResult<()> {
    let value = required_anthropic_delta_string(delta, delta_field)?;
    match block.get_mut(block_field) {
        Some(Value::String(current)) => current.push_str(value),
        _ => block[block_field] = Value::String(value.to_string()),
    }
    Ok(())
}

fn push_anthropic_content_block(
    block: &Value,
    items: &mut Vec<AssistantItem>,
    provider_replay: &mut Vec<ProviderReplayItem>,
) -> ProviderResult<()> {
    let block_type = anthropic_block_type(block, "Anthropic response content block")?;
    let display = anthropic_provider_replay_display(block);
    provider_replay.push(ProviderReplayItem::new_with_display(
        ProviderKind::Claude,
        block,
        display,
    )?);

    match block_type {
        "text" => {
            if let Some(text) = block.get("text").and_then(Value::as_str) {
                push_text_item(items, text);
            }
        }
        "thinking" | "redacted_thinking" => {}
        "tool_use" => {
            let id = block
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| ProviderError::Provider("Claude tool_use missing id".to_string()))?;
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
    Ok(())
}

fn anthropic_stop_details(value: Option<&Value>) -> ProviderResult<Option<ModelStopDetails>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    let value = value.as_object().ok_or_else(|| {
        ProviderError::Provider(
            "Anthropic message_delta stop_details was not an object or null".to_string(),
        )
    })?;
    let optional_string = |field| -> ProviderResult<Option<String>> {
        match value.get(field) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            Some(value) => Err(ProviderError::Provider(format!(
                "Anthropic message_delta stop_details.{field} was not a string or null: {value}"
            ))),
        }
    };
    Ok(Some(ModelStopDetails {
        category: optional_string("category")?,
        explanation: optional_string("explanation")?,
    }))
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

fn anthropic_usage(value: &Value) -> Option<ProviderUsage> {
    let provider_input_tokens = value
        .get("input_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let output_tokens = value
        .get("output_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let cache_read_input_tokens = value
        .get("cache_read_input_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let cache_creation_input_tokens = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64)
        .map(|value| value as usize);
    let input_tokens = provider_input_tokens.map(|input| {
        input
            .saturating_add(cache_read_input_tokens.unwrap_or_default())
            .saturating_add(cache_creation_input_tokens.unwrap_or_default())
    });
    Some(ProviderUsage {
        input_tokens,
        output_tokens,
        // Anthropic's top-level input components exclude compaction
        // iterations. Normalize message input as uncached + cache read +
        // cache creation, while retaining each provider-native field below.
        total_tokens: input_tokens
            .zip(output_tokens)
            .map(|(input, output)| input.saturating_add(output)),
        cache_read_input_tokens,
        cache_creation_input_tokens,
        raw_provider_usage: Some(value.clone()),
        ..ProviderUsage::default()
    })
}

fn anthropic_message_delta_usage(value: Option<&Value>) -> ProviderResult<Option<ProviderUsage>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let usage = value.as_object().ok_or_else(|| {
        ProviderError::Provider("Anthropic message_delta usage was not an object".to_string())
    })?;
    let mut has_token_count = false;
    for field in [
        "input_tokens",
        "output_tokens",
        "cache_read_input_tokens",
        "cache_creation_input_tokens",
    ] {
        if let Some(value) = usage.get(field) {
            has_token_count = true;
            let tokens = value.as_u64().ok_or_else(|| {
                ProviderError::Provider(format!(
                    "Anthropic message_delta usage.{field} was not an unsigned integer"
                ))
            })?;
            usize::try_from(tokens).map_err(|_| {
                ProviderError::Provider(format!(
                    "Anthropic message_delta usage.{field} exceeded the platform limit"
                ))
            })?;
        }
    }
    if !has_token_count {
        return Err(ProviderError::Provider(
            "Anthropic message_delta usage contained no cumulative token counts".to_string(),
        ));
    }
    Ok(anthropic_usage(value))
}

fn merge_anthropic_stop_details(
    current: &mut Option<ModelStopDetails>,
    update: ModelStopDetails,
) -> ProviderResult<()> {
    let current = current.get_or_insert_with(ModelStopDetails::default);
    for (field, current, update) in [
        ("category", &mut current.category, update.category),
        ("explanation", &mut current.explanation, update.explanation),
    ] {
        match (current.as_ref(), update) {
            (Some(existing), Some(update)) if existing != &update => {
                return Err(ProviderError::Provider(format!(
                    "Anthropic response stream contained conflicting terminal stop_details.{field}"
                )));
            }
            (None, Some(update)) => *current = Some(update),
            _ => {}
        }
    }
    Ok(())
}

fn merge_anthropic_usage(current: &mut Option<ProviderUsage>, update: ProviderUsage) {
    let current = current.get_or_insert_with(ProviderUsage::default);
    let previous_raw = current.raw_provider_usage.as_ref();
    let previous_input_tokens = previous_raw
        .and_then(|raw| raw.get("input_tokens"))
        .and_then(Value::as_u64);
    let previous_cache_read = previous_raw
        .and_then(|raw| raw.get("cache_read_input_tokens"))
        .and_then(Value::as_u64);
    let previous_cache_creation = previous_raw
        .and_then(|raw| raw.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64);
    let update_raw = update.raw_provider_usage.as_ref();
    let update_input_tokens = update_raw
        .and_then(|raw| raw.get("input_tokens"))
        .and_then(Value::as_u64);
    let update_cache_read = update_raw
        .and_then(|raw| raw.get("cache_read_input_tokens"))
        .and_then(Value::as_u64);
    let update_cache_creation = update_raw
        .and_then(|raw| raw.get("cache_creation_input_tokens"))
        .and_then(Value::as_u64);
    if let Some(update) = update.raw_provider_usage {
        merge_json_object(&mut current.raw_provider_usage, update);
    }
    if let Some(raw) = current
        .raw_provider_usage
        .as_mut()
        .and_then(Value::as_object_mut)
    {
        for (key, previous, update) in [
            ("input_tokens", previous_input_tokens, update_input_tokens),
            (
                "cache_read_input_tokens",
                previous_cache_read,
                update_cache_read,
            ),
            (
                "cache_creation_input_tokens",
                previous_cache_creation,
                update_cache_creation,
            ),
        ] {
            // Input accounting is cumulative across stream fragments. A zero
            // in a later delta does not erase a nonzero component reported at
            // message_start; a present nonzero value replaces the old value.
            if update == Some(0) && previous.is_some_and(|value| value > 0) {
                raw.insert(key.to_string(), json!(previous.expect("checked")));
            }
        }
        let provider_input_tokens = raw
            .get("input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        current.output_tokens = raw
            .get("output_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        current.cache_read_input_tokens = raw
            .get("cache_read_input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        current.cache_creation_input_tokens = raw
            .get("cache_creation_input_tokens")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        current.input_tokens = provider_input_tokens.map(|input| {
            input
                .saturating_add(current.cache_read_input_tokens.unwrap_or_default())
                .saturating_add(current.cache_creation_input_tokens.unwrap_or_default())
        });
    }
    current.total_tokens = current
        .input_tokens
        .zip(current.output_tokens)
        .map(|(input, output)| input.saturating_add(output));
}

fn merge_json_object(current: &mut Option<Value>, update: Value) {
    let current = current.get_or_insert_with(|| json!({}));
    merge_json_value(current, update);
}

fn merge_json_value(current: &mut Value, update: Value) {
    match update {
        Value::Object(update) => {
            let Some(current) = current.as_object_mut() else {
                *current = Value::Object(update);
                return;
            };
            for (key, update) in update {
                match current.get_mut(&key) {
                    Some(value) => merge_json_value(value, update),
                    None => {
                        current.insert(key, update);
                    }
                }
            }
        }
        update => *current = update,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptSections;
    use agent_vocab::ToolResultMessage;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn metadata_threshold_preserves_one_million_special_case_and_generic_policy() {
        assert_eq!(anthropic_auto_compact_limit(1_000_000), 500_000);
        assert_eq!(anthropic_auto_compact_limit(200_000), 170_000);
    }

    #[test]
    fn sonnet_45_static_metadata_preserves_input_output_and_capability_semantics() {
        let metadata = static_anthropic_model_metadata("claude-sonnet-4-5");

        assert_eq!(metadata.max_input_tokens, Some(200_000));
        assert_eq!(metadata.max_tokens, UNKNOWN_MODEL_MAX_OUTPUT_TOKENS);
        assert_eq!(metadata.capabilities, AnthropicModelCapabilities::default());
        assert_eq!(
            metadata.max_input_tokens.map(anthropic_auto_compact_limit),
            Some(170_000)
        );
    }

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

    fn test_compaction_request(transcript: Vec<ModelTranscriptEntry>) -> ProviderCompactionRequest {
        ProviderCompactionRequest {
            model: "claude-opus-4-8".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript,
            tool_profile: ProviderToolProfile::AnthropicCoding,
            tools: first_party_tools(ProviderKind::Claude),
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: None,
            session_id: Some("session-1".to_string()),
            compaction_instructions: Some(
                "Preserve actionable state. Do not call tools; respond with summary text only."
                    .to_string(),
            ),
        }
    }

    fn test_model_request(model: &str, transcript: Vec<ModelTranscriptEntry>) -> ModelRequest {
        ModelRequest {
            model: model.to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript,
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: Some(1024),
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: None,
            session_id: Some("session-1".to_string()),
            turn_id: None,
        }
    }

    async fn read_http_request(socket: &mut tokio::net::TcpStream) -> (String, Value) {
        let mut request = Vec::new();
        let mut buffer = [0u8; 4096];
        let (header_end, content_length) = loop {
            let read = socket.read(&mut buffer).await.expect("request reads");
            assert!(read > 0, "request closed before headers");
            request.extend_from_slice(&buffer[..read]);
            let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
            else {
                continue;
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, value)| {
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            break (header_end, content_length);
        };
        let body_end = header_end + 4 + content_length;
        while request.len() < body_end {
            let read = socket.read(&mut buffer).await.expect("request body reads");
            assert!(read > 0, "request closed before body");
            request.extend_from_slice(&buffer[..read]);
        }
        let headers = String::from_utf8(request[..header_end].to_vec()).expect("headers are utf8");
        let body = if content_length == 0 {
            Value::Null
        } else {
            serde_json::from_slice(&request[header_end + 4..body_end])
                .expect("request body is JSON")
        };
        (headers, body)
    }

    async fn write_json_response(socket: &mut tokio::net::TcpStream, body: &Value) {
        let body = body.to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("JSON response writes");
    }

    async fn write_ordinary_sse_response(socket: &mut tokio::net::TcpStream) {
        let sse = concat!(
            "data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":10,\"output_tokens\":0}}}\n\n",
            "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"ok\"}}\n\n",
            "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"output_tokens\":1}}\n\n",
            "data: {\"type\":\"message_stop\"}\n\n",
        );
        let response = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
            sse.len()
        );
        socket
            .write_all(response.as_bytes())
            .await
            .expect("SSE response writes");
    }

    fn native_compaction_error_kind(error: &ProviderError) -> NativeCompactionErrorKind {
        match error {
            ProviderError::NativeCompaction { kind, .. } => *kind,
            other => panic!("expected typed native compaction error, got {other}"),
        }
    }

    #[test]
    fn compaction_body_uses_paused_minimum_trigger_and_no_tools() {
        let body = compaction_body(test_compaction_request(vec![TranscriptItem::UserMessage(
            UserMessage::text("history"),
        )
        .into()]))
        .expect("compaction body renders");

        assert_eq!(body["model"], "claude-opus-4-8");
        assert_eq!(body["stream"], true);
        assert_eq!(
            body["context_management"]["edits"],
            json!([{
                "type": "compact_20260112",
                "trigger": {
                    "type": "input_tokens",
                    "value": 50_000,
                },
                "pause_after_compaction": true,
                "instructions": "Preserve actionable state. Do not call tools; respond with summary text only.",
            }])
        );
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "history",
                    "cache_control": { "type": "ephemeral" },
                }],
            }])
        );
        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "high");
    }

    #[test]
    fn compaction_body_never_sends_an_assistant_prefill() {
        let assistant_ended = compaction_body(test_compaction_request(vec![
            TranscriptItem::UserMessage(UserMessage::text("question")).into(),
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("answer".to_string())],
            })
            .into(),
        ]))
        .expect("assistant-ended compaction body renders");
        assert_eq!(
            assistant_ended["messages"],
            json!([
                {
                    "role": "user",
                    "content": [{ "type": "text", "text": "question" }],
                },
                {
                    "role": "assistant",
                    "content": [{
                        "type": "text",
                        "text": "answer",
                        "cache_control": { "type": "ephemeral" },
                    }],
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": COMPACTION_TERMINAL_USER_INSTRUCTION,
                    }],
                },
            ])
        );

        let user_ended =
            compaction_body(test_compaction_request(vec![TranscriptItem::UserMessage(
                UserMessage::text("already user-ended"),
            )
            .into()]))
            .expect("user-ended compaction body renders");
        assert_eq!(user_ended["messages"].as_array().unwrap().len(), 1);
        assert_eq!(
            user_ended["messages"][0]["content"][0]["text"],
            "already user-ended"
        );

        let tool_call = ToolCall {
            id: ToolCallId::new("toolu_1"),
            tool_name: "read".to_string(),
            args_json: r#"{"path":"README.md"}"#.to_string(),
        };
        let tool_ended = compaction_body(test_compaction_request(vec![
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
        ]))
        .expect("tool-ended compaction body renders");
        assert_eq!(
            tool_ended["messages"],
            json!([
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "read",
                        "input": { "path": "README.md" },
                    }],
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_1",
                        "content": "contents",
                        "is_error": false,
                        "cache_control": { "type": "ephemeral" },
                    }],
                },
            ])
        );

        let unmatched_tool = ToolCall {
            id: ToolCallId::new("toolu_unmatched"),
            tool_name: "read".to_string(),
            args_json: r#"{"path":"missing.md"}"#.to_string(),
        };
        let unmatched_tool_ended = compaction_body(test_compaction_request(vec![
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(unmatched_tool)],
            })
            .into(),
        ]))
        .expect("unmatched tool-ended compaction body renders");
        assert_eq!(
            unmatched_tool_ended["messages"],
            json!([
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_unmatched",
                        "name": "read",
                        "input": { "path": "missing.md" },
                        "cache_control": { "type": "ephemeral" },
                    }],
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_unmatched",
                            "content": "Tool result unavailable at the compaction boundary.",
                            "is_error": true,
                        },
                        {
                            "type": "text",
                            "text": COMPACTION_TERMINAL_USER_INSTRUCTION,
                        },
                    ],
                },
            ])
        );

        for transcript in [
            Vec::new(),
            vec![
                TranscriptItem::TurnStarted {
                    turn_id: agent_vocab::TurnId(1),
                }
                .into(),
                TranscriptItem::TurnFinished {
                    turn_id: agent_vocab::TurnId(1),
                    outcome: agent_vocab::TurnOutcome::Graceful,
                }
                .into(),
            ],
        ] {
            let degenerate = compaction_body(test_compaction_request(transcript))
                .expect("degenerate compaction body renders");
            assert_eq!(
                degenerate["messages"],
                json!([{
                    "role": "user",
                    "content": [{
                        "type": "text",
                        "text": COMPACTION_TERMINAL_USER_INSTRUCTION,
                    }],
                }])
            );
        }
    }

    #[test]
    fn compaction_body_repairs_missing_tool_results_in_existing_user_tail() {
        let tool_call = |id: &str| ToolCall {
            id: ToolCallId::new(id),
            tool_name: "read".to_string(),
            args_json: r#"{"path":"README.md"}"#.to_string(),
        };

        let user_text = compaction_body(test_compaction_request(vec![
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call("toolu_missing"))],
            })
            .into(),
            TranscriptItem::UserMessage(UserMessage::text("Keep this user text.")).into(),
        ]))
        .expect("user-ended missing result is repaired");
        assert_eq!(
            user_text["messages"][1]["content"],
            json!([
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_missing",
                    "content": "Tool result unavailable at the compaction boundary.",
                    "is_error": true,
                },
                {
                    "type": "text",
                    "text": "Keep this user text.",
                    "cache_control": { "type": "ephemeral" },
                },
            ])
        );

        let partial = compaction_body(test_compaction_request(vec![
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![
                    AssistantItem::ToolCall(tool_call("toolu_present")),
                    AssistantItem::ToolCall(tool_call("toolu_missing")),
                ],
            })
            .into(),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId::new("toolu_present"),
                "read",
                "existing result",
            ))
            .into(),
            TranscriptItem::UserMessage(UserMessage::text("Preserved after results.")).into(),
        ]))
        .expect("partial results are repaired");
        let rendered = partial["messages"].as_array().unwrap();
        let all_results = rendered
            .iter()
            .filter_map(|message| message.get("content").and_then(Value::as_array))
            .flatten()
            .filter(|block| block.get("type").and_then(Value::as_str) == Some("tool_result"))
            .filter_map(|block| block.get("tool_use_id").and_then(Value::as_str))
            .collect::<Vec<_>>();
        assert_eq!(all_results, vec!["toolu_present", "toolu_missing"]);
        assert_eq!(
            rendered[1]["content"][2]["text"],
            "Preserved after results."
        );

        let mut out_of_order = json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [
                        { "type": "tool_use", "id": "toolu_a" },
                        { "type": "tool_use", "id": "toolu_b" }
                    ]
                },
                {
                    "role": "user",
                    "content": [{ "type": "text", "text": "text before results" }]
                },
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": "toolu_a",
                        "content": "real A"
                    }]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "toolu_a",
                            "content": "duplicate A"
                        },
                        {
                            "type": "tool_result",
                            "tool_use_id": "unmatched",
                            "content": "unmatched"
                        },
                        { "type": "text", "text": "dynamic context" }
                    ]
                }
            ]
        });
        ensure_compaction_terminal_user_message(&mut out_of_order);
        assert_eq!(out_of_order["messages"].as_array().unwrap().len(), 2);
        assert_eq!(
            out_of_order["messages"][1]["content"],
            json!([
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_a",
                    "content": "real A"
                },
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_b",
                    "content": "Tool result unavailable at the compaction boundary.",
                    "is_error": true
                },
                { "type": "text", "text": "text before results" },
                { "type": "text", "text": "dynamic context" }
            ])
        );

        let mut dynamic_request =
            test_compaction_request(vec![TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call("toolu_dynamic"))],
            })
            .into()]);
        dynamic_request.prompt = PromptSections::new(
            Some("stable rules".to_string()),
            Some("volatile dynamic context".to_string()),
        );
        let dynamic = compaction_body(dynamic_request).expect("dynamic user tail is repaired");
        assert_eq!(
            dynamic["messages"][1]["content"],
            json!([
                {
                    "type": "tool_result",
                    "tool_use_id": "toolu_dynamic",
                    "content": "Tool result unavailable at the compaction boundary.",
                    "is_error": true,
                },
                { "type": "text", "text": "volatile dynamic context" },
            ])
        );

        let matched = compaction_body(test_compaction_request(vec![
            TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::ToolCall(tool_call("toolu_matched"))],
            })
            .into(),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId::new("toolu_matched"),
                "read",
                "existing result",
            ))
            .into(),
            TranscriptItem::UserMessage(UserMessage::text("No repair needed.")).into(),
        ]))
        .expect("fully matched tail remains valid");
        assert_eq!(matched["messages"].as_array().unwrap().len(), 2);
        let matched_json = matched["messages"].to_string();
        assert_eq!(matched_json.matches("toolu_matched").count(), 2);
        assert!(!matched_json.contains("unavailable"));
        assert!(matched_json.contains("No repair needed."));
    }

    #[test]
    fn compaction_capability_is_model_specific_and_typed() {
        for supported in [
            "claude-fable-5",
            "claude-mythos-5",
            "claude-mythos-preview",
            "claude-opus-4-8",
            "claude-opus-4-7",
            "claude-opus-4-6",
            "claude-sonnet-5",
            "claude-sonnet-4-6",
        ] {
            assert!(
                static_anthropic_model_metadata(supported)
                    .capabilities
                    .native_compaction,
                "{supported}"
            );
        }

        for unsupported in ["claude-sonnet-4-5", "claude-unknown"] {
            let mut request = test_compaction_request(vec![TranscriptItem::UserMessage(
                UserMessage::text("history"),
            )
            .into()]);
            request.model = unsupported.to_string();
            let error = compaction_body(request).expect_err("unsupported model must fail locally");
            assert_eq!(
                native_compaction_error_kind(&error),
                NativeCompactionErrorKind::Unsupported,
                "{unsupported}"
            );
        }

        let fallback = static_anthropic_model_metadata("claude-opus-4-8");
        let discovered: ModelsApiModel = serde_json::from_value(json!({
            "id": "claude-opus-4-8",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": models_api_capabilities(json!({ "supported": true }))
        }))
        .unwrap();
        assert!(
            !merge_models_api_metadata(fallback, discovered)
                .capabilities
                .native_compaction,
            "known Models API unsupported capability overrides the static list"
        );
    }

    #[test]
    fn messages_body_omits_adaptive_thinking_for_non_adaptive_models() {
        let body = messages_body(ModelRequest {
            model: "claude-sonnet-4-5".to_string(),
            transcript_cache_prefix_len: None,
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
            turn_id: None,
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
        assert_eq!(body["stream"], true);
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
    fn messages_body_enables_adaptive_thinking_for_opus_48() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-8".to_string(),
            transcript_cache_prefix_len: None,
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
            turn_id: None,
        })
        .expect("body renders");

        assert_eq!(body["thinking"]["type"], "adaptive");
        assert_eq!(body["output_config"]["effort"], "xhigh");
        assert_eq!(body["max_tokens"], 2048);
        assert_eq!(body["stream"], true);
    }

    #[test]
    fn adaptive_effort_normalization_is_adapter_owned_for_ordinary_sidecar_compact_and_count() {
        for effort in [ReasoningEffort::None, ReasoningEffort::Minimal] {
            // Sidecars call the same `complete` path and therefore use this
            // ordinary Messages body builder without daemon-side shaping.
            let ordinary = messages_body(ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: effort,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            })
            .expect("ordinary adaptive request renders");
            assert_eq!(ordinary["output_config"]["effort"], "low");

            let mut compact_request = test_compaction_request(vec![TranscriptItem::UserMessage(
                UserMessage::text("history"),
            )
            .into()]);
            compact_request.reasoning_effort = effort;
            let compact =
                compaction_body(compact_request).expect("compact adaptive request renders");
            assert_eq!(compact["output_config"]["effort"], "low");

            let count = count_tokens_body(ProviderTokenCountRequest {
                model: "claude-opus-4-8".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: effort,
                prompt_cache_key: None,
                session_id: None,
            })
            .expect("count adaptive request renders");
            assert_eq!(count["output_config"]["effort"], "low");
        }
    }

    #[test]
    fn sonnet_5_and_fable_5_use_default_on_adaptive_thinking_and_all_efforts() {
        for model in ["claude-sonnet-5", "claude-fable-5"] {
            for effort in [ReasoningEffort::XHigh, ReasoningEffort::Max] {
                let body =
                    messages_body(ModelRequest {
                        model: model.to_string(),
                        transcript_cache_prefix_len: None,
                        prompt: PromptSections::stable("stable rules"),
                        transcript: vec![
                            TranscriptItem::UserMessage(UserMessage::text("hello")).into()
                        ],
                        tool_profile: ProviderToolProfile::None,
                        tools: Vec::new(),
                        max_tokens: None,
                        reasoning_effort: effort,
                        prompt_cache_key: None,
                        session_id: None,
                        turn_id: None,
                    })
                    .expect("body renders");

                assert!(
                    body.get("thinking").is_none(),
                    "{model} defaults to adaptive thinking and should omit redundant configuration"
                );
                assert_eq!(body["output_config"]["effort"], effort.as_str());
                assert_eq!(body["max_tokens"], DEFAULT_MAX_OUTPUT_BUDGET);
            }
        }
    }

    #[test]
    fn discovered_output_limit_clamps_default_and_explicit_budgets() {
        let mut metadata = static_anthropic_model_metadata("claude-sonnet-5");
        metadata.max_tokens = 32_000;
        let request = |max_tokens| ModelRequest {
            model: "claude-sonnet-5".to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens,
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        };

        assert_eq!(
            messages_body_with_metadata(request(None), &metadata).unwrap()["max_tokens"],
            32_000
        );
        assert_eq!(
            messages_body_with_metadata(request(Some(100_000)), &metadata).unwrap()["max_tokens"],
            32_000
        );
        assert_eq!(
            messages_body_with_metadata(request(Some(8_192)), &metadata).unwrap()["max_tokens"],
            8_192
        );
    }

    fn models_api_capabilities(xhigh: Value) -> Value {
        json!({
            "batch": { "supported": true },
            "citations": { "supported": true },
            "code_execution": { "supported": true },
            "context_management": {
                "clear_thinking_20251015": null,
                "clear_tool_uses_20250919": null,
                "compact_20260112": null,
                "supported": false
            },
            "effort": {
                "supported": true,
                "low": { "supported": true },
                "medium": { "supported": true },
                "high": { "supported": true },
                "xhigh": xhigh,
                "max": { "supported": true }
            },
            "image_input": { "supported": true },
            "pdf_input": { "supported": true },
            "structured_outputs": { "supported": true },
            "thinking": {
                "supported": true,
                "types": {
                    "adaptive": { "supported": true },
                    "enabled": { "supported": false }
                }
            }
        })
    }

    #[tokio::test]
    async fn compact_wire_request_scopes_beta_to_special_messages_call() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("model request accepted");
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let read = socket.read(&mut buffer).await.expect("model request reads");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("model request is utf8");
            assert!(request.starts_with("GET /v1/models/claude-opus-4-8 HTTP/1.1\r\n"));
            assert!(!request.to_ascii_lowercase().contains("anthropic-beta:"));
            let mut capabilities = models_api_capabilities(json!({ "supported": true }));
            capabilities["context_management"] = json!({
                "compact_20260112": { "supported": true },
                "supported": true
            });
            let model = json!({
                "id": "claude-opus-4-8",
                "max_input_tokens": 1_000_000,
                "max_tokens": 128_000,
                "capabilities": capabilities
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{model}",
                model.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("model response writes");

            let (mut socket, _) = listener
                .accept()
                .await
                .expect("compaction request accepted");
            let mut request = Vec::new();
            loop {
                let read = socket
                    .read(&mut buffer)
                    .await
                    .expect("compaction request reads");
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("compaction request is utf8");
            assert!(request.starts_with("POST /v1/messages HTTP/1.1\r\n"));
            let lower = request.to_ascii_lowercase();
            let beta = lower
                .lines()
                .find(|line| line.starts_with("anthropic-beta:"))
                .expect("compaction beta header present");
            assert!(beta.contains(CLAUDE_CODE_BETA));
            assert!(beta.contains(COMPACTION_BETA));

            let sse = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"content\":[],\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"iterations\":[]}}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"compaction\",\"content\":null,\"encrypted_content\":null}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"compaction_delta\",\"content\":\"wire summary\",\"encrypted_content\":\"wire opaque\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: message_delta\n",
                "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"compaction\"},\"usage\":{\"input_tokens\":0,\"output_tokens\":0,\"iterations\":[{\"type\":\"compaction\",\"input_tokens\":60000,\"output_tokens\":10}]}}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n"
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                sse.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("compaction response writes");
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let response = provider
            .compact(test_compaction_request(vec![TranscriptItem::UserMessage(
                UserMessage::text("history"),
            )
            .into()]))
            .await
            .expect("wire compaction succeeds");
        server.await.expect("server completes");

        assert_eq!(
            response.provider_replay[0].raw_value().unwrap()["content"],
            "wire summary"
        );
        assert_eq!(
            response.provider_replay[0].raw_value().unwrap()["encrypted_content"],
            "wire opaque"
        );
    }

    #[tokio::test]
    async fn ordinary_replay_wire_request_pairs_strategy_with_beta_and_precompaction_omits_both() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for expect_replay in [true, false] {
                let (mut socket, _) = listener.accept().await.expect("model lookup accepted");
                let (headers, body) = read_http_request(&mut socket).await;
                assert!(headers.starts_with("GET /v1/models/claude-opus-4-8 HTTP/1.1\r\n"));
                assert_eq!(body, Value::Null);
                let mut capabilities = models_api_capabilities(json!({ "supported": true }));
                capabilities["context_management"] = json!({
                    "compact_20260112": { "supported": true },
                    "supported": true
                });
                write_json_response(
                    &mut socket,
                    &json!({
                        "id": "claude-opus-4-8",
                        "max_input_tokens": 444_444,
                        "max_tokens": 128_000,
                        "capabilities": capabilities
                    }),
                )
                .await;

                let (mut socket, _) = listener.accept().await.expect("messages request accepted");
                let (headers, body) = read_http_request(&mut socket).await;
                assert!(headers.starts_with("POST /v1/messages HTTP/1.1\r\n"));
                let beta = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find_map(|(name, value)| {
                        name.eq_ignore_ascii_case("anthropic-beta")
                            .then(|| value.trim())
                    })
                    .expect("beta header present");
                if expect_replay {
                    assert!(beta.contains(COMPACTION_BETA));
                    assert_eq!(
                        body["context_management"],
                        json!({
                            "edits": [{
                                "type": "compact_20260112",
                                "trigger": {
                                    "type": "input_tokens",
                                    "value": 444_444,
                                },
                                "pause_after_compaction": true,
                            }]
                        })
                    );
                    assert_eq!(body["messages"][0]["content"][0]["type"], "compaction");
                } else {
                    assert!(!beta.contains(COMPACTION_BETA));
                    assert!(body.get("context_management").is_none());
                    assert!(!body["messages"].to_string().contains("\"compaction\""));
                }
                write_ordinary_sse_response(&mut socket).await;
            }
        });

        let block = json!({
            "type": "compaction",
            "content": "opaque summary",
            "encrypted_content": "opaque",
        });
        let checkpoint = ModelTranscriptEntry {
            item: TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "session-1",
                "leaf-1",
                "checkpoint",
                Some(80_000),
                agent_vocab::TurnId(7),
            )),
            provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &block).unwrap()],
        };

        let mut replay_provider =
            AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        replay_provider.base_url = base_url.clone();
        replay_provider
            .complete(test_model_request("claude-opus-4-8", vec![checkpoint]))
            .await
            .expect("wire replay request succeeds");

        let mut ordinary_provider =
            AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        ordinary_provider.base_url = base_url;
        ordinary_provider
            .complete(test_model_request(
                "claude-opus-4-8",
                vec![TranscriptItem::UserMessage(UserMessage::text("ordinary")).into()],
            ))
            .await
            .expect("wire precompaction request succeeds");
        server.await.expect("server completes");
    }

    fn test_model_metadata(id: &str, max_tokens: u32) -> AnthropicModelMetadata {
        let mut metadata = static_anthropic_model_metadata(id);
        metadata.max_tokens = max_tokens;
        metadata
    }

    #[test]
    fn models_api_null_capabilities_preserves_authoritative_limits() {
        let discovered: ModelsApiModel = serde_json::from_value(json!({
            "id": "claude-future",
            "max_input_tokens": 200_000,
            "max_tokens": 8_192,
            "capabilities": null
        }))
        .expect("nullable capabilities parse");
        let metadata =
            merge_models_api_metadata(static_anthropic_model_metadata("claude-future"), discovered);

        assert_eq!(metadata.max_input_tokens, Some(200_000));
        assert_eq!(metadata.max_tokens, 8_192);
        assert!(!metadata.capabilities.effort);
    }

    #[test]
    fn authoritative_null_xhigh_is_unsupported_and_rejected_locally() {
        let discovered: ModelsApiModel = serde_json::from_value(json!({
            "id": "claude-sonnet-5",
            "max_input_tokens": 1_000_000,
            "max_tokens": 128_000,
            "capabilities": models_api_capabilities(Value::Null)
        }))
        .expect("nullable xhigh parses");
        let metadata = merge_models_api_metadata(
            static_anthropic_model_metadata("claude-sonnet-5"),
            discovered,
        );

        assert!(!metadata
            .capabilities
            .supports_effort(ReasoningEffort::XHigh));
        assert!(metadata.capabilities.supports_effort(ReasoningEffort::Low));
        assert!(metadata
            .capabilities
            .supports_effort(ReasoningEffort::Medium));
        assert!(metadata.capabilities.supports_effort(ReasoningEffort::High));
        assert!(metadata.capabilities.supports_effort(ReasoningEffort::Max));

        let request = ModelRequest {
            model: "claude-sonnet-5".to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        };
        let error = messages_body_with_metadata(request, &metadata)
            .expect_err("authoritative unsupported xhigh must fail locally");
        assert!(error.to_string().contains("xhigh"));
        assert!(error.to_string().contains("not supported"));
    }

    #[test]
    fn beta_header_keeps_identity_only_and_drops_ga_feature_betas() {
        let header = anthropic_beta_header();
        assert_eq!(header, CLAUDE_CODE_BETA);
        assert!(!header.contains(COMPACTION_BETA));
        assert!(anthropic_compaction_beta_header().contains(COMPACTION_BETA));
        for stale in [
            "effort-",
            "extended-cache-ttl-",
            "fine-grained-tool-streaming-",
            "web-fetch-",
            "interleaved-thinking-",
        ] {
            assert!(!header.contains(stale));
        }
    }

    #[tokio::test]
    async fn models_api_metadata_is_authoritative_and_cached() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request accepted");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.expect("request reads");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("request is utf8");
            assert!(request.starts_with("GET /v1/models/claude-sonnet-5 HTTP/1.1\r\n"));
            let lower = request.to_ascii_lowercase();
            assert!(lower.contains("anthropic-version: 2023-06-01\r\n"));
            assert!(!lower.contains("anthropic-beta:"));
            assert!(lower.contains("x-api-key: test-key\r\n"));

            let mut capabilities = models_api_capabilities(json!({ "supported": true }));
            capabilities["effort"]["max"] = json!({ "supported": false });
            let body = json!({
                "id": "claude-sonnet-5",
                "type": "model",
                "display_name": "Claude Sonnet 5",
                "created_at": "2026-06-30T00:00:00Z",
                "max_input_tokens": 444_444,
                "max_tokens": 32_000,
                "capabilities": capabilities
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response writes");
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let first = provider.resolved_model_metadata("claude-sonnet-5").await;
        server.await.expect("server completes");
        // The listener is now gone. A second successful authoritative result
        // proves that no second network request was attempted.
        let second = provider.resolved_model_metadata("claude-sonnet-5").await;

        assert_eq!(first, second);
        assert_eq!(first.max_input_tokens, Some(444_444));
        assert_eq!(first.max_tokens, 32_000);
        assert!(first.capabilities.supports_effort(ReasoningEffort::XHigh));
        assert!(!first.capabilities.supports_effort(ReasoningEffort::Max));

        let request = |effort| ModelRequest {
            model: "claude-sonnet-5".to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: effort,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        };
        let xhigh = messages_body_with_metadata(request(ReasoningEffort::XHigh), &first)
            .expect("discovered xhigh is accepted");
        assert_eq!(xhigh["max_tokens"], 32_000);
        assert_eq!(xhigh["output_config"]["effort"], "xhigh");
        assert!(messages_body_with_metadata(request(ReasoningEffort::Max), &first).is_err());
    }

    #[tokio::test]
    async fn models_api_failure_uses_and_negative_caches_static_safety_metadata() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request accepted");
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.expect("request reads");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let body = r#"{"type":"error","error":{"type":"api_error","message":"nope"}}"#;
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response writes");
            drop(socket);

            // A negative cache entry must suppress an immediate second fetch.
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err()
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let first = provider.resolved_model_metadata("claude-fable-5").await;
        let second = provider.resolved_model_metadata("claude-fable-5").await;

        assert!(
            server.await.expect("server completes"),
            "failure result should be negative-cached"
        );
        assert_eq!(first, second);
        assert_eq!(first.max_input_tokens, Some(1_000_000));
        assert_eq!(first.max_tokens, 128_000);
        assert!(first.capabilities.adaptive_thinking_default);
        assert!(first.capabilities.supports_effort(ReasoningEffort::Max));
    }

    #[tokio::test]
    async fn sonnet_45_models_api_failure_projects_200k_window_and_170k_recommendation() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request accepted");
            let _ = read_http_request(&mut socket).await;
            let body = r#"{"type":"error","error":{"type":"api_error","message":"nope"}}"#;
            let response = format!(
                "HTTP/1.1 500 Internal Server Error\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response writes");
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let metadata = provider
            .model_metadata("claude-sonnet-4-5")
            .await
            .expect("static fallback metadata is returned")
            .expect("Anthropic always projects model metadata");
        server.await.expect("server completes");

        assert_eq!(metadata.max_input_tokens, Some(200_000));
        assert_eq!(metadata.recommended_auto_compact_tokens, Some(170_000));
    }

    #[tokio::test]
    async fn concurrent_cold_model_cache_callers_issue_one_get() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::Barrier;

        const CALLERS: usize = 16;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.expect("request accepted");
                server_requests.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0u8; 1024];
                    loop {
                        let read = socket.read(&mut buffer).await.expect("request reads");
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&buffer[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let body = json!({
                        "id": "claude-future",
                        "max_input_tokens": 200_000,
                        "max_tokens": 8_192,
                        "capabilities": null
                    })
                    .to_string();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    socket
                        .write_all(response.as_bytes())
                        .await
                        .expect("response writes");
                });
            }
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let provider = Arc::new(provider);
        let barrier = Arc::new(Barrier::new(CALLERS));
        let mut callers = Vec::new();
        for _ in 0..CALLERS {
            let provider = Arc::clone(&provider);
            let barrier = Arc::clone(&barrier);
            callers.push(tokio::spawn(async move {
                barrier.wait().await;
                provider.resolved_model_metadata("claude-future").await
            }));
        }
        for caller in callers {
            let metadata = caller.await.expect("caller completes");
            assert_eq!(metadata.max_input_tokens, Some(200_000));
            assert_eq!(metadata.max_tokens, 8_192);
        }
        server.abort();
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelling_refresh_leader_does_not_strand_waiters() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::oneshot;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let (request_started_tx, request_started_rx) = oneshot::channel();
        let (respond_tx, respond_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request accepted");
            server_requests.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.expect("request reads");
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let _ = request_started_tx.send(());
            let _ = respond_rx.await;
            let body = json!({
                "id": "claude-future",
                "max_input_tokens": 200_000,
                "max_tokens": 8_192,
                "capabilities": null
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response writes");
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let provider = Arc::new(provider);
        let leader_provider = Arc::clone(&provider);
        let leader = tokio::spawn(async move {
            leader_provider
                .resolved_model_metadata("claude-future")
                .await
        });
        request_started_rx
            .await
            .expect("detached refresh starts its GET");
        leader.abort();
        let _ = leader.await;

        let waiter_provider = Arc::clone(&provider);
        let waiter = tokio::spawn(async move {
            waiter_provider
                .resolved_model_metadata("claude-future")
                .await
        });
        respond_tx.send(()).expect("server may finish response");
        let metadata = waiter.await.expect("waiter completes");
        server.await.expect("server completes");

        assert_eq!(metadata.max_input_tokens, Some(200_000));
        assert_eq!(metadata.max_tokens, 8_192);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn late_failed_generation_cannot_overwrite_newer_success() {
        let cache = AnthropicModelCache::default();
        let now = Instant::now();
        let first = match cache.decision("model", now).await {
            ModelCacheDecision::Start { refresh, .. } => refresh,
            _ => panic!("cold cache should start a refresh"),
        };
        cache.abandon_refresh("model", first.generation).await;
        let second = match cache.decision("model", now).await {
            ModelCacheDecision::Start { refresh, .. } => refresh,
            _ => panic!("abandoned refresh should allow a newer generation"),
        };
        let success = test_model_metadata("model", 8_192);
        assert_eq!(
            cache
                .commit_refresh("model", second.generation, Some(success.clone()), now)
                .await,
            Some(success.clone())
        );
        assert_eq!(
            cache
                .commit_refresh("model", first.generation, None, now)
                .await,
            Some(success.clone())
        );
        match cache.decision("model", now).await {
            ModelCacheDecision::Return(Some(cached)) => assert_eq!(cached, success),
            _ => panic!("newer success must remain cached"),
        }
    }

    #[tokio::test]
    async fn expired_success_survives_failed_refresh_with_retry_backoff() {
        let cache = AnthropicModelCache::default();
        let now = Instant::now();
        let initial = match cache.decision("model", now).await {
            ModelCacheDecision::Start { refresh, .. } => refresh,
            _ => panic!("cold cache should start a refresh"),
        };
        let success = test_model_metadata("model", 8_192);
        cache
            .commit_refresh("model", initial.generation, Some(success.clone()), now)
            .await;

        let expired = now + MODEL_CACHE_SUCCESS_TTL;
        let refresh = match cache.decision("model", expired).await {
            ModelCacheDecision::Start { refresh, .. } => refresh,
            _ => panic!("expired success should refresh"),
        };
        assert_eq!(
            cache
                .commit_refresh("model", refresh.generation, None, expired)
                .await,
            Some(success.clone())
        );
        match cache
            .decision("model", expired + MODEL_CACHE_FAILURE_TTL / 2)
            .await
        {
            ModelCacheDecision::Return(Some(cached)) => assert_eq!(cached, success),
            _ => panic!("failed refresh should serve stale success during backoff"),
        }
        assert!(matches!(
            cache
                .decision(
                    "model",
                    expired + MODEL_CACHE_FAILURE_TTL + Duration::from_nanos(1)
                )
                .await,
            ModelCacheDecision::Start { .. }
        ));
    }

    #[tokio::test]
    async fn cold_failure_is_negative_cached_until_retry_backoff_expires() {
        let cache = AnthropicModelCache::default();
        let now = Instant::now();
        let initial = match cache.decision("model", now).await {
            ModelCacheDecision::Start { refresh, .. } => refresh,
            _ => panic!("cold cache should start a refresh"),
        };
        assert_eq!(
            cache
                .commit_refresh("model", initial.generation, None, now)
                .await,
            None
        );
        assert!(matches!(
            cache
                .decision("model", now + MODEL_CACHE_FAILURE_TTL / 2)
                .await,
            ModelCacheDecision::Return(None)
        ));
        assert!(matches!(
            cache
                .decision(
                    "model",
                    now + MODEL_CACHE_FAILURE_TTL + Duration::from_nanos(1)
                )
                .await,
            ModelCacheDecision::Start { .. }
        ));
    }

    #[tokio::test]
    async fn model_cache_remains_bounded_to_64_entries() {
        let cache = AnthropicModelCache::default();
        let now = Instant::now();
        for index in 0..=MODEL_CACHE_CAPACITY {
            let model = format!("model-{index}");
            let refresh = match cache.decision(&model, now).await {
                ModelCacheDecision::Start { refresh, .. } => refresh,
                _ => panic!("new model should start a refresh"),
            };
            cache
                .commit_refresh(
                    &model,
                    refresh.generation,
                    Some(test_model_metadata(&model, 8_192)),
                    now,
                )
                .await;
        }

        let state = cache.state.lock().await;
        assert_eq!(state.entries.len(), MODEL_CACHE_CAPACITY);
        assert!(!state.entries.contains_key("model-0"));
        assert!(state
            .entries
            .contains_key(&format!("model-{MODEL_CACHE_CAPACITY}")));
    }

    #[tokio::test]
    async fn capacity_pressure_preserves_in_flight_refresh_and_waiters() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use tokio::sync::oneshot;

        const MODEL: &str = "claude-capacity-pressure";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let server_requests = Arc::clone(&requests);
        let (request_started_tx, request_started_rx) = oneshot::channel();
        let (respond_tx, respond_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("request accepted");
            server_requests.fetch_add(1, Ordering::SeqCst);
            let mut request = Vec::new();
            let mut buffer = [0u8; 1024];
            loop {
                let read = socket.read(&mut buffer).await.expect("request reads");
                if read == 0 {
                    return false;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).expect("request is utf8");
            assert!(request.starts_with(&format!("GET /v1/models/{MODEL} HTTP/1.1\r\n")));
            let _ = request_started_tx.send(());
            let _ = respond_rx.await;

            let body = json!({
                "id": MODEL,
                "max_input_tokens": 321_000,
                "max_tokens": 12_345,
                "capabilities": null
            })
            .to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("response writes");
            drop(socket);

            match tokio::time::timeout(Duration::from_millis(200), listener.accept()).await {
                Err(_) => true,
                Ok(Ok((_socket, _))) => {
                    server_requests.fetch_add(1, Ordering::SeqCst);
                    false
                }
                Ok(Err(error)) => panic!("duplicate request check failed: {error}"),
            }
        });

        let mut provider = AnthropicProvider::new_with_client(reqwest::Client::new(), "test-key");
        provider.base_url = base_url;
        let provider = Arc::new(provider);
        let cache = provider.model_cache.clone();
        let leader_provider = Arc::clone(&provider);
        let leader =
            tokio::spawn(async move { leader_provider.resolved_model_metadata(MODEL).await });
        request_started_rx
            .await
            .expect("model refresh starts its GET");

        let original_generation = {
            let state = cache.state.lock().await;
            state.entries[MODEL]
                .refresh
                .as_ref()
                .expect("model refresh remains in flight")
                .generation
        };

        // All pressure entries remain in flight, so there is no settled entry
        // to evict. The cache must temporarily hold 65 entries rather than
        // discard MODEL's refresh state.
        let mut pressure_refreshes = Vec::new();
        for index in 0..MODEL_CACHE_CAPACITY {
            let pressure_model = format!("pressure-{index}");
            match cache.decision(&pressure_model, Instant::now()).await {
                ModelCacheDecision::Start { refresh, sender } => {
                    pressure_refreshes.push((pressure_model, refresh, sender));
                }
                _ => panic!("new pressure model should start a refresh"),
            }
        }
        {
            let state = cache.state.lock().await;
            assert_eq!(state.entries.len(), MODEL_CACHE_CAPACITY + 1);
            assert!(state.entries.values().all(|entry| entry.refresh.is_some()));
        }

        let before_waiter = {
            let state = cache.state.lock().await;
            state.access_counter
        };
        let waiter_provider = Arc::clone(&provider);
        let waiter =
            tokio::spawn(async move { waiter_provider.resolved_model_metadata(MODEL).await });

        // Wait until the second provider request has consulted the cache, then
        // prove it attached to the original generation rather than starting a
        // replacement GET.
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let attached = {
                    let state = cache.state.lock().await;
                    let cached = state.entries.get(MODEL).expect("in-flight model retained");
                    cached.last_access > before_waiter
                        && cached
                            .refresh
                            .as_ref()
                            .is_some_and(|refresh| refresh.generation == original_generation)
                };
                if attached {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("second request attaches to original refresh");
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        respond_tx.send(()).expect("server may finish response");
        let first = leader.await.expect("leader completes");
        let second = waiter.await.expect("waiter completes");
        assert!(
            server.await.expect("server completes"),
            "no duplicate model GET should be accepted"
        );
        for metadata in [&first, &second] {
            assert_eq!(metadata.id, MODEL);
            assert_eq!(metadata.max_input_tokens, Some(321_000));
            assert_eq!(metadata.max_tokens, 12_345);
        }
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        for (model, refresh, sender) in pressure_refreshes {
            let effective = cache
                .commit_refresh(
                    &model,
                    refresh.generation,
                    Some(test_model_metadata(&model, 8_192)),
                    Instant::now(),
                )
                .await;
            let _ = sender.send(ModelRefreshStatus::Finished(effective));
        }
        let state = cache.state.lock().await;
        assert_eq!(state.entries.len(), MODEL_CACHE_CAPACITY);
        assert!(state.entries.values().all(|entry| entry.refresh.is_none()));
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
        assert!(body.get("stream").is_none());
        assert!(body["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert!(body.get("context_management").is_none());
    }

    #[test]
    fn count_tokens_body_omits_generation_budget_even_when_configured() {
        let request = ProviderTokenCountRequest {
            model: "claude-sonnet-4-5".to_string(),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: Some(80),
            reasoning_effort: ReasoningEffort::None,
            prompt_cache_key: None,
            session_id: None,
        };

        let body = count_tokens_body(request).expect("count body renders");

        assert!(body.get("max_tokens").is_none());
        assert!(body.get("stream").is_none());
    }

    #[test]
    fn ordinary_precompaction_messages_and_count_omit_replay_strategy() {
        let transcript =
            vec![TranscriptItem::UserMessage(UserMessage::text("ordinary turn")).into()];
        let metadata = static_anthropic_model_metadata("claude-opus-4-8");
        let ordinary = prepare_messages_request(
            ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: transcript.clone(),
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            &metadata,
        )
        .expect("ordinary body renders");
        let count = prepare_count_tokens_request(
            ProviderTokenCountRequest {
                model: "claude-opus-4-8".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: transcript.clone(),
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
            },
            &metadata,
        )
        .expect("count body renders");

        assert_eq!(ordinary.beta_header, CLAUDE_CODE_BETA);
        assert_eq!(count.beta_header, CLAUDE_CODE_BETA);
        assert!(ordinary.body.get("context_management").is_none());
        assert!(count.body.get("context_management").is_none());
    }

    #[test]
    fn compaction_checkpoint_replays_exact_block_and_applies_strategy_consistently() {
        let raw = json!({
            "type": "compaction",
            "content": "opaque summary",
            "name": "str_replace_based_edit_tool",
            "provider_extension": {
                "must_survive": ["byte", "for", "byte"]
            }
        });
        let entry = ModelTranscriptEntry {
            item: TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "session-1",
                "leaf-1",
                "Provider-native compaction checkpoint.\n\nFresh delegation ledger.",
                Some(80_000),
                agent_vocab::TurnId(7),
            )),
            provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &raw).unwrap()],
        };

        let metadata = static_anthropic_model_metadata("claude-opus-4-8");
        let ordinary = prepare_messages_request(
            ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry.clone()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: Some(1024),
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            &metadata,
        )
        .expect("ordinary replay body renders");
        assert!(ordinary.beta_header.contains(COMPACTION_BETA));
        let ordinary = ordinary.body;

        assert_eq!(ordinary["messages"][0]["role"], "assistant");
        assert_eq!(ordinary["messages"][0]["content"][0], raw);
        assert!(ordinary["messages"][0]["content"][0]
            .get("cache_control")
            .is_none());
        assert_eq!(ordinary["messages"][1]["role"], "user");
        assert!(ordinary["messages"][1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Fresh delegation ledger."));
        assert_eq!(
            ordinary["context_management"]["edits"],
            json!([{
                "type": "compact_20260112",
                "trigger": {
                    "type": "input_tokens",
                    "value": 1_000_000,
                },
                "pause_after_compaction": true,
            }])
        );

        let count = prepare_count_tokens_request(
            ProviderTokenCountRequest {
                model: "claude-opus-4-8".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
            },
            &metadata,
        )
        .expect("count replay body renders");
        assert!(count.beta_header.contains(COMPACTION_BETA));
        let count = count.body;
        assert_eq!(count["messages"][0]["content"][0], raw);
        assert_eq!(
            count["context_management"]["edits"],
            json!([{ "type": "compact_20260112" }])
        );

        let parsed = parse_anthropic_count_tokens(
            r#"{"input_tokens":23456,"context_management":{"original_input_tokens":187654}}"#,
        )
        .expect("count response parses");
        assert_eq!(parsed.input_tokens, 23_456);
        assert_eq!(parsed.original_input_tokens, Some(187_654));
    }

    #[test]
    fn compacted_model_request_renders_summary_before_exact_user_instruction() {
        let replay = json!({
            "type": "compaction",
            "content": "Claude's opaque compacted context",
            "encrypted_content": "opaque-ciphertext",
        });
        let summary = ModelTranscriptEntry {
            item: TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "session-1",
                "large-open-turn-leaf",
                "Visible checkpoint preserving prior work.",
                Some(180_000),
                agent_vocab::TurnId(7),
            )),
            provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &replay).unwrap()],
        };
        let instruction = "Return exactly: RETAINED-USER-INSTRUCTION";
        let request = test_model_request(
            "claude-opus-4-8",
            vec![
                summary,
                TranscriptItem::UserMessage(UserMessage::text(instruction)).into(),
            ],
        );

        let body = messages_body(request).expect("compacted model request renders");
        assert_eq!(body["messages"][0]["role"], "assistant");
        assert_eq!(body["messages"][0]["content"][0], replay);
        assert_eq!(body["messages"][1]["role"], "user");
        assert!(body["messages"][1]["content"][0]["text"]
            .as_str()
            .is_some_and(|text| text.contains("Visible checkpoint")));
        assert_eq!(body["messages"][2]["role"], "user");
        assert_eq!(body["messages"][2]["content"][0]["text"], instruction);
    }

    #[test]
    fn messages_replay_uses_resolved_model_ceiling_while_count_stays_bare() {
        let raw = json!({
            "type": "compaction",
            "content": "opaque summary",
        });
        let entry = ModelTranscriptEntry {
            item: TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "session-1",
                "leaf-1",
                "checkpoint",
                Some(80_000),
                agent_vocab::TurnId(7),
            )),
            provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &raw).unwrap()],
        };
        let mut metadata = static_anthropic_model_metadata("claude-sonnet-4-6");
        metadata.max_input_tokens = Some(200_000);
        let ordinary = messages_body_with_metadata(
            ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry.clone()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            &metadata,
        )
        .expect("non-1M ordinary replay renders");
        assert_eq!(
            ordinary["context_management"],
            json!({
                "edits": [{
                    "type": "compact_20260112",
                    "trigger": {
                        "type": "input_tokens",
                        "value": 200_000,
                    },
                    "pause_after_compaction": true,
                }]
            })
        );

        metadata.max_input_tokens = Some(20_000);
        let clamped = messages_body_with_metadata(
            ModelRequest {
                model: "claude-sonnet-4-6".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry.clone()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            &metadata,
        )
        .expect("defensively clamped ordinary replay renders");
        assert_eq!(
            clamped["context_management"]["edits"][0]["trigger"]["value"],
            50_000
        );

        let count = count_tokens_body_with_metadata(
            ProviderTokenCountRequest {
                model: "claude-sonnet-4-6".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
            },
            &metadata,
        )
        .expect("count replay renders");
        assert_eq!(
            count["context_management"],
            json!({ "edits": [{ "type": "compact_20260112" }] })
        );
    }

    #[test]
    fn compaction_replay_detection_ignores_wrong_provider_and_nonemitted_sidecars() {
        for provider_replay in [
            vec![ProviderReplayItem::new(
                ProviderKind::OpenAi,
                &json!({ "type": "compaction", "content": "opaque" }),
            )
            .unwrap()],
            vec![ProviderReplayItem::new(
                ProviderKind::Claude,
                &json!({ "type": "text", "text": "ordinary replay" }),
            )
            .unwrap()],
        ] {
            let entry = ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("visible".to_string())],
                }),
                provider_replay,
            };
            let ordinary = messages_body(ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry.clone()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            })
            .expect("ordinary body renders");
            let count = count_tokens_body(ProviderTokenCountRequest {
                model: "claude-opus-4-8".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
            })
            .expect("count body renders");

            assert!(ordinary.get("context_management").is_none());
            assert!(count.get("context_management").is_none());
        }

        for item in [
            TranscriptItem::UserMessage(UserMessage::text("user")),
            TranscriptItem::ToolResult(ToolResultMessage::success(
                ToolCallId::new("toolu_1"),
                "read",
                "result",
            )),
            TranscriptItem::TurnFinished {
                turn_id: agent_vocab::TurnId(1),
                outcome: agent_vocab::TurnOutcome::Graceful,
            },
        ] {
            let entry = ModelTranscriptEntry {
                item,
                provider_replay: vec![ProviderReplayItem::new(
                    ProviderKind::Claude,
                    &json!({ "type": "compaction", "content": "opaque" }),
                )
                .unwrap()],
            };
            let body = messages_body(ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            })
            .expect("non-emitted sidecar is ignored");
            assert!(body.get("context_management").is_none());
            assert!(!body["messages"].to_string().contains("\"compaction\""));
        }
    }

    #[test]
    fn malformed_emitted_compaction_replay_fails_request_building_locally() {
        for raw in [
            json!({ "type": "compaction" }),
            json!({ "type": "compaction", "content": null }),
            json!({ "type": "compaction", "content": "" }),
            json!({ "type": "compaction", "content": { "malformed": true } }),
            json!({
                "type": "compaction",
                "content": "opaque",
                "encrypted_content": { "malformed": true },
            }),
        ] {
            let entry = ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("visible".to_string())],
                }),
                provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &raw).unwrap()],
            };
            let error = messages_body(ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            })
            .expect_err("malformed replay must not render");
            assert!(error.to_string().contains("malformed persisted"));
        }
    }

    #[test]
    fn compaction_summary_requires_exactly_one_valid_compaction_replay_block() {
        let summary = || {
            TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                "session-1",
                "leaf-1",
                "provider-native checkpoint",
                Some(80_000),
                agent_vocab::TurnId(7),
            ))
        };
        let request = |entry| ModelRequest {
            model: "claude-opus-4-8".to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![entry],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::High,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        };

        let block = json!({
            "type": "compaction",
            "content": "opaque",
            "encrypted_content": "ciphertext",
        });
        let valid = prepare_messages_request(
            request(ModelTranscriptEntry {
                item: summary(),
                provider_replay: vec![
                    ProviderReplayItem::new(ProviderKind::Claude, &block).unwrap()
                ],
            }),
            &static_anthropic_model_metadata("claude-opus-4-8"),
        )
        .expect("summary with exactly one valid compaction replay renders");
        assert!(valid.beta_header.contains(CLAUDE_CODE_BETA));
        assert!(valid.beta_header.contains(COMPACTION_BETA));
        assert_eq!(valid.body["messages"][0]["content"][0], block);

        let invalid_replays = [
            Vec::new(),
            vec![ProviderReplayItem {
                provider: ProviderKind::Claude,
                raw_json: "{".to_string(),
                display: None,
            }],
            vec![ProviderReplayItem::new(
                ProviderKind::Claude,
                &json!({ "content": "missing type" }),
            )
            .unwrap()],
            vec![ProviderReplayItem::new(
                ProviderKind::Claude,
                &json!({ "type": "text", "text": "wrong type" }),
            )
            .unwrap()],
            vec![ProviderReplayItem::new(
                ProviderKind::Claude,
                &json!({ "type": 7, "content": "non-string type" }),
            )
            .unwrap()],
            vec![
                ProviderReplayItem::new(
                    ProviderKind::Claude,
                    &json!({ "type": "compaction", "content": "first" }),
                )
                .unwrap(),
                ProviderReplayItem::new(
                    ProviderKind::Claude,
                    &json!({ "type": "compaction", "content": "second" }),
                )
                .unwrap(),
            ],
        ];
        for provider_replay in invalid_replays {
            assert!(messages_body(request(ModelTranscriptEntry {
                item: summary(),
                provider_replay,
            }))
            .is_err());
        }
    }

    #[test]
    fn assistant_replay_parses_every_sidecar_and_validates_exact_compaction() {
        for replay in [
            ProviderReplayItem {
                provider: ProviderKind::Claude,
                raw_json: "{".to_string(),
                display: None,
            },
            ProviderReplayItem::new(ProviderKind::Claude, &Value::Null).unwrap(),
            ProviderReplayItem::new(ProviderKind::Claude, &json!(7)).unwrap(),
            ProviderReplayItem::new(ProviderKind::Claude, &json!({})).unwrap(),
            ProviderReplayItem::new(ProviderKind::Claude, &json!({ "type": null })).unwrap(),
            ProviderReplayItem::new(ProviderKind::Claude, &json!({ "type": 7 })).unwrap(),
            ProviderReplayItem::new(ProviderKind::Claude, &json!({ "type": "" })).unwrap(),
            ProviderReplayItem::new(
                ProviderKind::Claude,
                &json!({ "type": "compaction", "content": null }),
            )
            .unwrap(),
        ] {
            let entry = ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("visible".to_string())],
                }),
                provider_replay: vec![replay],
            };
            assert!(
                render_transcript_messages(&PromptSections::default(), &[entry]).is_err(),
                "corrupt or malformed exact compaction replay must fail locally"
            );
        }
    }

    #[test]
    fn messages_body_sorts_tools_for_cache_stability() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            transcript_cache_prefix_len: None,
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
            turn_id: None,
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
                "cancel_delegation",
                "delegate_readonly_tasks",
                "delegate_writing_task",
                "inspect_delegation",
                "interrupt_subagent",
                "LoadSkill",
                "steer_subagent",
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
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::AnthropicCoding,
            tools: first_party_tools(ProviderKind::Claude),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        })
        .expect("body renders");

        assert_eq!(body["tools"][0]["name"], "Bash");
        assert!(body["tools"][0].get("type").is_none());
        assert_eq!(body["tools"][1]["name"], "cancel_delegation");
        assert!(body["tools"][1].get("type").is_none());
        assert_eq!(body["tools"][2]["name"], "delegate_readonly_tasks");
        assert!(body["tools"][2].get("type").is_none());
        assert_eq!(body["tools"][3]["name"], "delegate_writing_task");
        assert!(body["tools"][3].get("type").is_none());
        assert_eq!(body["tools"][4]["name"], "inspect_delegation");
        assert!(body["tools"][4].get("type").is_none());
        assert_eq!(body["tools"][5]["name"], "interrupt_subagent");
        assert!(body["tools"][5].get("type").is_none());
        assert_eq!(body["tools"][6]["name"], "LoadSkill");
        assert!(body["tools"][6].get("type").is_none());
        assert_eq!(body["tools"][7]["name"], "steer_subagent");
        assert!(body["tools"][7].get("type").is_none());
        assert_eq!(body["tools"][8]["type"], "text_editor_20250728");
        assert_eq!(body["tools"][8]["name"], "str_replace_based_edit_tool");
        assert_eq!(body["tools"][9]["name"], "web_fetch");
        assert!(body["tools"][9].get("type").is_none());
        assert_eq!(body["tools"][10]["name"], "web_search");
        assert!(body["tools"][10].get("type").is_none());
        // Native coding tools also carry no per-tool cache_control: the
        // stable-system breakpoint covers them via the cumulative hash.
        for index in 0..11 {
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
            transcript_cache_prefix_len: None,
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
            turn_id: None,
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
    fn messages_body_keeps_sidecar_suffix_out_of_cache_prefix() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            transcript_cache_prefix_len: Some(1),
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![
                TranscriptItem::UserMessage(UserMessage::text("normal user turn")).into(),
                TranscriptItem::UserMessage(UserMessage::text("sidecar title prompt")).into(),
            ],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::XHigh,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        })
        .expect("body renders");

        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({
                "type": "ephemeral",
            })
        );
        assert!(body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn messages_body_tail_positions_dynamic_context_out_of_system() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::new(
                Some("stable rules".to_string()),
                Some("volatile context".to_string()),
            ),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
        })
        .expect("body renders");

        assert_eq!(body["system"][1]["text"], "stable rules");
        assert!(!body["system"].to_string().contains("volatile context"));
        assert_eq!(body["messages"][0]["content"][0]["text"], "hello");
        assert_eq!(
            body["messages"][0]["content"][0]["cache_control"],
            json!({ "type": "ephemeral" })
        );
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            "volatile context"
        );
        assert!(body["messages"][1]["content"][0]
            .get("cache_control")
            .is_none());
    }

    #[test]
    fn ordinary_sse_parser_rejects_inline_and_paused_compaction() {
        for sse in [
            r#"
data: {"type":"message_start","message":{"content":[{"type":"compaction","content":"inline"}],"usage":{"input_tokens":150000,"output_tokens":0}}}
"#,
            r#"
data: {"type":"message_start","message":{"content":[],"usage":{"input_tokens":150000,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","content":"unexpected"}}
"#,
            r#"
data: {"type":"message_start","message":{"content":[],"usage":{"input_tokens":150000,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"compaction","content":null,"encrypted_content":null,"provider_extension":{"future":true}}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","content":"automatic summary","encrypted_content":"opaque"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"continued answer"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

data: {"type":"message_stop"}
"#,
            r#"
data: {"type":"message_start","message":{"content":[],"usage":{"input_tokens":150000,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"compaction","content":null}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"compaction_delta","content":"automatic summary"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"read","input":{}}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{}"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"message_delta","delta":{"stop_reason":"tool_use"}}

data: {"type":"message_stop"}
"#,
            r#"
data: {"type":"message_start","message":{"content":[],"usage":{"input_tokens":150000,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"compaction","content":null}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"compaction"}}

data: {"type":"message_stop"}
"#,
            r#"
data: {"type":"message_start","message":{"content":[],"usage":{"input_tokens":150000,"output_tokens":0}}}

data: {"type":"message_delta","delta":{"stop_reason":"compaction"}}

data: {"type":"message_stop"}
"#,
        ] {
            let error = parse_anthropic_sse(sse)
                .expect_err("ordinary compaction stream must not return a persistable response");
            assert!(error.to_string().contains("compaction"));
            assert!(error.to_string().contains("refusing to persist"));
        }
    }

    #[test]
    fn anthropic_parser_preserves_usage_cache_metrics() {
        let usage = anthropic_usage(&json!({
            "input_tokens": 100,
            "output_tokens": 20,
            "cache_read_input_tokens": 75,
            "cache_creation_input_tokens": 25
        }))
        .expect("usage should be parsed");

        assert_eq!(usage.input_tokens, Some(200));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(220));
        assert_eq!(usage.cache_read_input_tokens, Some(75));
        assert_eq!(usage.cache_creation_input_tokens, Some(25));
        assert_eq!(
            usage
                .raw_provider_usage
                .as_ref()
                .and_then(|usage| usage.get("input_tokens"))
                .and_then(Value::as_u64),
            Some(100)
        );
    }

    #[test]
    fn anthropic_usage_keeps_iterations_without_double_counting_normalized_total() {
        let raw = json!({
            "input_tokens": 23_000,
            "output_tokens": 1_000,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 11,
                "ephemeral_1h_input_tokens": 22
            },
            "output_tokens_details": {
                "thinking_tokens": 333
            },
            "iterations": [
                {
                    "type": "compaction",
                    "input_tokens": 180_000,
                    "output_tokens": 3_500,
                    "cache_creation": {
                        "ephemeral_5m_input_tokens": 44
                    },
                    "output_tokens_details": {
                        "thinking_tokens": 555
                    }
                },
                {
                    "type": "message",
                    "input_tokens": 23_000,
                    "output_tokens": 1_000
                }
            ]
        });

        let usage = anthropic_usage(&raw).expect("usage parses");

        // Top-level counts exclude compaction; normalized accounting must not
        // add the 183,500 compaction tokens a second time.
        assert_eq!(usage.input_tokens, Some(23_000));
        assert_eq!(usage.output_tokens, Some(1_000));
        assert_eq!(usage.total_tokens, Some(24_000));
        assert_eq!(usage.raw_provider_usage, Some(raw));
    }

    #[test]
    fn anthropic_usage_merges_nested_stream_details_and_final_iterations() {
        let mut usage = anthropic_usage(&json!({
            "input_tokens": 60_000,
            "output_tokens": 0,
            "cache_creation": {
                "ephemeral_5m_input_tokens": 11,
                "ephemeral_1h_input_tokens": 22
            },
            "iterations": []
        }));
        merge_anthropic_usage(
            &mut usage,
            anthropic_usage(&json!({
                "input_tokens": 0,
                "output_tokens": 1_000,
                "cache_creation": {
                    "ephemeral_5m_input_tokens": 33
                },
                "output_tokens_details": {
                    "thinking_tokens": 444
                },
                "iterations": [
                    {
                        "type": "compaction",
                        "input_tokens": 180_000,
                        "output_tokens": 3_500
                    },
                    {
                        "type": "message",
                        "input_tokens": 60_000,
                        "output_tokens": 1_000
                    }
                ]
            }))
            .expect("usage update parses"),
        );

        let usage = usage.expect("usage remains present");
        assert_eq!(usage.total_tokens, Some(61_000));
        let raw = usage.raw_provider_usage.expect("raw usage remains present");
        assert_eq!(raw.get("input_tokens"), Some(&json!(60_000)));
        assert_eq!(raw.get("output_tokens"), Some(&json!(1_000)));
        assert_eq!(
            raw.pointer("/cache_creation/ephemeral_5m_input_tokens"),
            Some(&json!(33))
        );
        assert_eq!(
            raw.pointer("/cache_creation/ephemeral_1h_input_tokens"),
            Some(&json!(22))
        );
        assert_eq!(
            raw.pointer("/output_tokens_details/thinking_tokens"),
            Some(&json!(444))
        );
        assert_eq!(
            raw.get("iterations")
                .and_then(Value::as_array)
                .map(Vec::len),
            Some(2)
        );
    }

    fn valid_compaction_sse_events() -> Vec<Value> {
        vec![
            json!({
                "type": "message_start",
                "message": {
                    "id": "msg_compact",
                    "type": "message",
                    "role": "assistant",
                    "model": "claude-opus-4-8",
                    "content": [],
                    "stop_reason": null,
                    "usage": {
                        "input_tokens": 0,
                        "output_tokens": 0,
                        "iterations": []
                    }
                }
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": {
                    "type": "compaction",
                    "content": null,
                    "encrypted_content": null,
                    "provider_extension": { "must_survive": true }
                }
            }),
            json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "compaction_delta",
                    "content": "opaque streamed summary",
                    "encrypted_content": "opaque+/= ciphertext exactly",
                    "delta_extension": ["also", "preserved"]
                }
            }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({
                "type": "message_delta",
                "delta": {
                    "stop_reason": "compaction",
                    "stop_sequence": null
                },
                "usage": {
                    "input_tokens": 0,
                    "output_tokens": 0,
                    "iterations": [{
                        "type": "compaction",
                        "input_tokens": 180000,
                        "output_tokens": 3500
                    }]
                }
            }),
            json!({ "type": "message_stop" }),
        ]
    }

    fn compaction_sse(events: &[Value]) -> String {
        events
            .iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect()
    }

    #[test]
    fn streamed_compaction_round_trips_all_encrypted_metadata_shapes_through_requests() {
        for (name, encrypted_content) in [
            ("string", Some(json!("opaque+/= ciphertext exactly"))),
            ("explicit null", Some(Value::Null)),
            ("omitted", None),
        ] {
            let mut events = valid_compaction_sse_events();
            match encrypted_content.as_ref() {
                Some(value) => {
                    events[1]["content_block"]
                        .as_object_mut()
                        .unwrap()
                        .insert("encrypted_content".to_string(), Value::Null);
                    events[2]["delta"]
                        .as_object_mut()
                        .unwrap()
                        .insert("encrypted_content".to_string(), value.clone());
                }
                None => {
                    events[1]["content_block"]
                        .as_object_mut()
                        .unwrap()
                        .remove("encrypted_content");
                    events[2]["delta"]
                        .as_object_mut()
                        .unwrap()
                        .remove("encrypted_content");
                }
            }
            let response = parse_anthropic_compaction_sse(&compaction_sse(&events))
                .unwrap_or_else(|error| panic!("{name} compaction response parses: {error}"));
            let mut expected = json!({
                "type": "compaction",
                "content": "opaque streamed summary",
                "provider_extension": { "must_survive": true },
                "delta_extension": ["also", "preserved"]
            });
            if let Some(value) = encrypted_content {
                expected
                    .as_object_mut()
                    .unwrap()
                    .insert("encrypted_content".to_string(), value);
            }
            assert_eq!(
                response.provider_replay[0].raw_value().unwrap(),
                expected,
                "{name}"
            );
            let usage = response.usage.expect("usage retained");
            assert_eq!(usage.total_tokens, Some(0), "{name}");
            assert_eq!(
                usage
                    .raw_provider_usage
                    .as_ref()
                    .and_then(|raw| raw.pointer("/iterations/0/output_tokens")),
                Some(&json!(3500)),
                "{name}"
            );

            // Simulate the durable provider_replay JSONB round trip before
            // building either kind of subsequent Anthropic request.
            let persisted = serde_json::to_string(&response.provider_replay).unwrap();
            let restored: Vec<ProviderReplayItem> = serde_json::from_str(&persisted).unwrap();
            let entry = ModelTranscriptEntry {
                item: TranscriptItem::CompactionSummary(agent_vocab::CompactionSummary::new(
                    "session-1",
                    "leaf-1",
                    "visible checkpoint",
                    Some(180_000),
                    agent_vocab::TurnId(7),
                )),
                provider_replay: restored,
            };

            let ordinary = messages_body(ModelRequest {
                model: "claude-opus-4-8".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry.clone()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: Some(1024),
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            })
            .expect("ordinary continuation body renders");
            let count = count_tokens_body(ProviderTokenCountRequest {
                model: "claude-opus-4-8".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![entry],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: None,
            })
            .expect("count_tokens body renders");

            for body in [&ordinary, &count] {
                let replayed = &body["messages"][0]["content"][0];
                assert_eq!(replayed, &expected, "{name}");
                assert!(
                    replayed.get("cache_control").is_none(),
                    "provider-returned replay must not be decorated"
                );
            }
        }

        let mut whitespace = valid_compaction_sse_events();
        whitespace[2]["delta"]["content"] = json!(" ");
        assert_eq!(
            parse_anthropic_compaction_sse(&compaction_sse(&whitespace))
                .expect("non-empty whitespace content is valid")
                .provider_replay[0]
                .raw_value()
                .unwrap()["content"],
            " "
        );
    }

    #[test]
    fn compaction_sse_ignores_ping_and_future_events_and_merges_message_deltas() {
        let mut events = valid_compaction_sse_events();
        events.insert(1, json!({ "type": "ping" }));
        events.insert(3, json!({ "type": "future_progress", "opaque": true }));
        events[6]["delta"]["stop_reason"] = Value::Null;
        events[6]["usage"] = json!({
            "output_tokens": 0,
            "future_usage": { "first": true }
        });
        events.insert(
            7,
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "compaction" },
                "usage": {
                    "output_tokens": 0,
                    "iterations": [{
                        "type": "compaction",
                        "input_tokens": 180000,
                        "output_tokens": 3500
                    }],
                    "future_usage": { "second": true }
                }
            }),
        );
        events.insert(8, json!({ "type": "ping" }));

        let response = parse_anthropic_compaction_sse(&compaction_sse(&events))
            .expect("forward-compatible stream parses");
        let raw = response
            .usage
            .and_then(|usage| usage.raw_provider_usage)
            .expect("usage is merged");
        assert_eq!(raw.pointer("/future_usage/first"), Some(&json!(true)));
        assert_eq!(raw.pointer("/future_usage/second"), Some(&json!(true)));
        assert_eq!(
            raw.pointer("/iterations/0/input_tokens"),
            Some(&json!(180000))
        );
    }

    #[test]
    fn compaction_sse_rejects_malformed_frame_sequences() {
        let valid = valid_compaction_sse_events();
        let mut cases: Vec<(&str, Vec<Value>, NativeCompactionErrorKind)> = Vec::new();

        let mut events = valid.clone();
        events.remove(0);
        cases.push((
            "missing message_start",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[1]["index"] = json!(7);
        cases.push((
            "start index is not zero",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.insert(1, valid[0].clone());
        cases.push((
            "duplicate message_start",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[0].as_object_mut().unwrap().remove("message");
        cases.push((
            "message_start missing message",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));

        let mut events = valid.clone();
        events.swap(1, 2);
        cases.push((
            "delta before start",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.insert(2, valid[1].clone());
        cases.push((
            "duplicate content start",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[1].as_object_mut().unwrap().remove("index");
        cases.push((
            "start missing index",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[1].as_object_mut().unwrap().remove("content_block");
        cases.push((
            "start missing block",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[1]["content_block"] = json!({ "type": "text", "text": "" });
        cases.push((
            "unexpected content block",
            events,
            NativeCompactionErrorKind::UnexpectedContent,
        ));
        let mut events = valid.clone();
        events[1]["content_block"]["content"] = json!("already populated");
        cases.push((
            "pre-populated start content",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[1]["content_block"]["encrypted_content"] = json!({ "invalid": true });
        cases.push((
            "start encrypted content wrong type",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));

        let mut events = valid.clone();
        events.remove(2);
        cases.push((
            "missing compaction delta",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.insert(3, valid[2].clone());
        cases.push((
            "duplicate compaction delta",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2].as_object_mut().unwrap().remove("index");
        cases.push((
            "delta missing index",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2]["index"] = json!(8);
        cases.push((
            "delta wrong index",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2].as_object_mut().unwrap().remove("delta");
        cases.push((
            "delta missing object",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2]["delta"]["type"] = json!("text_delta");
        cases.push((
            "wrong delta type",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2]["delta"]["type"] = json!("future_delta");
        cases.push((
            "unknown delta type",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2]["delta"]
            .as_object_mut()
            .unwrap()
            .remove("content");
        cases.push((
            "delta missing content",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2]["delta"]["content"] = json!(["not", "a", "string"]);
        cases.push((
            "delta content wrong type",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[2]["delta"]["content"] = Value::Null;
        cases.push((
            "delta content null",
            events,
            NativeCompactionErrorKind::NullBlock,
        ));
        let mut events = valid.clone();
        events[2]["delta"]["content"] = json!("");
        cases.push((
            "delta content empty",
            events,
            NativeCompactionErrorKind::EmptyBlock,
        ));
        let mut events = valid.clone();
        events[2]["delta"]["encrypted_content"] = json!({ "invalid": true });
        cases.push((
            "delta encrypted content wrong type",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));

        let mut events = valid.clone();
        events.remove(3);
        cases.push((
            "missing block stop",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.insert(4, valid[3].clone());
        cases.push((
            "duplicate block stop",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[3].as_object_mut().unwrap().remove("index");
        cases.push((
            "stop missing index",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[3]["index"] = json!(8);
        cases.push((
            "stop wrong index",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.insert(4, valid[1].clone());
        cases.push((
            "multiple compaction blocks",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.insert(
            4,
            json!({
                "type": "content_block_start",
                "index": 8,
                "content_block": { "type": "text", "text": "" }
            }),
        );
        cases.push((
            "mixed content blocks",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));

        let mut events = valid.clone();
        events.remove(4);
        cases.push((
            "missing message_delta",
            events,
            NativeCompactionErrorKind::UnexpectedStopReason,
        ));
        let mut events = valid.clone();
        events[4].as_object_mut().unwrap().remove("delta");
        cases.push((
            "message_delta missing delta",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events[4]["delta"]
            .as_object_mut()
            .unwrap()
            .remove("stop_reason");
        cases.push((
            "missing stop reason",
            events,
            NativeCompactionErrorKind::UnexpectedStopReason,
        ));
        let mut events = valid.clone();
        events[4]["delta"]["stop_reason"] = json!("end_turn");
        cases.push((
            "wrong stop reason",
            events,
            NativeCompactionErrorKind::UnexpectedStopReason,
        ));
        let mut events = valid.clone();
        events.insert(5, valid[4].clone());
        cases.push((
            "duplicate terminal stop reason",
            events,
            NativeCompactionErrorKind::UnexpectedStopReason,
        ));

        let mut events = valid.clone();
        events.pop();
        cases.push((
            "missing message_stop",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.push(json!({ "type": "message_stop" }));
        cases.push((
            "duplicate message_stop",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        let mut events = valid.clone();
        events.push(json!({
            "type": "content_block_start",
            "index": 8,
            "content_block": { "type": "text", "text": "trailing" }
        }));
        cases.push((
            "trailing content after terminal",
            events,
            NativeCompactionErrorKind::MalformedStream,
        ));
        for (name, events, expected) in cases {
            let error = parse_anthropic_compaction_sse(&compaction_sse(&events)).expect_err(name);
            assert_eq!(native_compaction_error_kind(&error), expected, "{name}");
        }
    }

    #[test]
    fn compaction_sse_rejects_malformed_json_and_done_sentinel() {
        for (name, sse) in [
            (
                "malformed JSON",
                "data: {\"type\":\"message_start\",\"message\":{}}\n\ndata: {not-json}\n\n",
            ),
            (
                "done sentinel",
                "data: {\"type\":\"message_start\",\"message\":{}}\n\ndata: [DONE]\n\n",
            ),
        ] {
            let error = parse_anthropic_compaction_sse(sse).expect_err(name);
            assert_eq!(
                native_compaction_error_kind(&error),
                NativeCompactionErrorKind::MalformedStream,
                "{name}"
            );
        }
    }

    #[test]
    fn anthropic_sse_accumulates_text_tool_calls_usage_and_stops_at_message_stop() {
        let sse = r#"
event: message_start
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[],"stop_reason":null,"usage":{"input_tokens":100,"output_tokens":1,"cache_read_input_tokens":75,"cache_creation_input_tokens":25}}}

event: content_block_start
data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"hel"}}

event: content_block_delta
data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"lo"}}

event: content_block_stop
data: {"type":"content_block_stop","index":0}

event: content_block_start
data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"str_replace_based_edit_tool","input":{}}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"path\""}}

event: content_block_delta
data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":":\"README.md\"}"}}

event: content_block_stop
data: {"type":"content_block_stop","index":1}

event: message_delta
data: {"type":"message_delta","delta":{"stop_reason":"tool_use","stop_sequence":null},"usage":{"input_tokens":0,"output_tokens":20}}

event: message_stop
data: {"type":"message_stop"}

event: content_block_start
data: {"type":"content_block_start","index":2,"content_block":{"type":"text","text":"ignored"}}
"#;

        let response = parse_anthropic_sse(sse).expect("sse parses");
        let calls = response.assistant.tool_calls().collect::<Vec<_>>();

        assert_eq!(response.assistant.text(), "hello");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool_name, "Edit");
        assert_eq!(
            calls[0].args_value().unwrap(),
            json!({ "path": "README.md" })
        );
        assert_eq!(response.provider_replay.len(), 2);
        let usage = response.usage.expect("usage should be parsed");
        assert_eq!(usage.input_tokens, Some(200));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(220));
        assert_eq!(usage.cache_read_input_tokens, Some(75));
        assert_eq!(usage.cache_creation_input_tokens, Some(25));
        assert_eq!(response.stop_reason, ModelStopReason::Complete);
    }

    #[test]
    fn anthropic_sse_merges_repeated_cumulative_message_deltas() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[],"usage":{"input_tokens":100,"output_tokens":0,"cache_read_input_tokens":40}}}

data: {"type":"message_delta","delta":{"stop_sequence":null},"usage":{"input_tokens":100,"output_tokens":2}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"still streaming"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":null,"stop_sequence":null},"usage":{"input_tokens":100,"output_tokens":7,"cache_creation_input_tokens":15}}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":100,"output_tokens":9,"cache_read_input_tokens":40,"cache_creation_input_tokens":15}}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"input_tokens":100,"output_tokens":10,"cache_read_input_tokens":40,"cache_creation_input_tokens":15}}

data: {"type":"message_stop"}
"#;

        let response = parse_anthropic_sse(sse).expect("repeated cumulative deltas parse");
        let usage = response.usage.expect("cumulative usage is retained");

        assert_eq!(response.assistant.text(), "still streaming");
        assert_eq!(response.stop_reason, ModelStopReason::Complete);
        assert_eq!(usage.input_tokens, Some(155));
        assert_eq!(usage.output_tokens, Some(10));
        assert_eq!(usage.total_tokens, Some(165));
        assert_eq!(usage.cache_read_input_tokens, Some(40));
        assert_eq!(usage.cache_creation_input_tokens, Some(15));
        assert_eq!(
            usage
                .raw_provider_usage
                .as_ref()
                .and_then(|usage| usage.get("output_tokens")),
            Some(&json!(10))
        );
        assert_eq!(
            usage
                .raw_provider_usage
                .as_ref()
                .and_then(|usage| usage.get("input_tokens")),
            Some(&json!(100))
        );
        assert_eq!(
            usage
                .raw_provider_usage
                .as_ref()
                .and_then(|usage| usage.get("cache_read_input_tokens")),
            Some(&json!(40))
        );
        assert_eq!(
            usage
                .raw_provider_usage
                .as_ref()
                .and_then(|usage| usage.get("cache_creation_input_tokens")),
            Some(&json!(15))
        );
    }

    #[test]
    fn anthropic_sse_preserves_valid_reasoning_server_tool_citation_multiblock_stream() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"opaque-signature"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search","input":{}}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"rust\"}"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"content_block_start","index":2,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[{"type":"web_search_result","title":"Rust","url":"https://www.rust-lang.org","encrypted_content":"opaque","page_age":null}]}}

data: {"type":"content_block_stop","index":2}

data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":"","citations":[]}}

data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"Rust"}}

data: {"type":"content_block_delta","index":3,"delta":{"type":"citations_delta","citation":{"type":"web_search_result_location","cited_text":"Rust","encrypted_index":"opaque-index","title":"Rust","url":"https://www.rust-lang.org"}}}

data: {"type":"content_block_stop","index":3}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

data: {"type":"message_stop"}
"#;

        let response = parse_anthropic_sse(sse).expect("valid multiblock stream parses");
        assert_eq!(response.assistant.text(), "Rust");
        assert_eq!(response.provider_replay.len(), 4);
        let replay = response
            .provider_replay
            .iter()
            .map(ProviderReplayItem::raw_value)
            .collect::<Result<Vec<_>, _>>()
            .expect("replay remains valid JSON");
        assert_eq!(replay[0]["signature"], "opaque-signature");
        assert_eq!(replay[1]["input"], json!({ "query": "rust" }));
        assert_eq!(replay[2]["tool_use_id"], "srvtoolu_1");
        assert_eq!(
            replay[3]["citations"][0]["type"],
            "web_search_result_location"
        );
    }

    #[test]
    fn anthropic_sse_preserves_omitted_optional_hosted_and_citation_metadata() {
        let search_result = json!({
            "type": "web_search_tool_result",
            "tool_use_id": "srvtoolu_search",
            "content": [{
                "type": "web_search_result",
                "title": "Rust",
                "url": "https://www.rust-lang.org",
                "encrypted_content": "opaque",
                "provider_extension": { "exact": true },
            }],
        });
        let fetch_result = json!({
            "type": "web_fetch_tool_result",
            "tool_use_id": "srvtoolu_fetch",
            "content": {
                "type": "web_fetch_result",
                "url": "https://www.rust-lang.org",
                "content": {
                    "type": "document",
                    "source": {
                        "type": "text",
                        "media_type": "text/plain",
                        "data": "Rust",
                    },
                },
                "provider_extension": ["preserved"],
            },
        });
        let web_citation = json!({
            "type": "web_search_result_location",
            "cited_text": "Rust",
            "encrypted_index": "opaque-index",
            "url": "https://www.rust-lang.org",
        });
        let document_citation = json!({
            "type": "char_location",
            "cited_text": "Rust",
            "document_index": 0,
            "start_char_index": 0,
            "end_char_index": 4,
            "provider_extension": { "opaque": true },
        });
        let text = json!({
            "type": "text",
            "text": "Rust",
            "citations": [web_citation, document_citation],
        });
        let events = vec![
            json!({
                "type": "message_start",
                "message": { "id": "msg_1", "content": [] },
            }),
            json!({
                "type": "content_block_start",
                "index": 0,
                "content_block": search_result,
            }),
            json!({ "type": "content_block_stop", "index": 0 }),
            json!({
                "type": "content_block_start",
                "index": 1,
                "content_block": fetch_result,
            }),
            json!({ "type": "content_block_stop", "index": 1 }),
            json!({
                "type": "content_block_start",
                "index": 2,
                "content_block": { "type": "text", "text": "" },
            }),
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": { "type": "text_delta", "text": "Rust" },
            }),
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {
                    "type": "citations_delta",
                    "citation": web_citation,
                },
            }),
            json!({
                "type": "content_block_delta",
                "index": 2,
                "delta": {
                    "type": "citations_delta",
                    "citation": document_citation,
                },
            }),
            json!({ "type": "content_block_stop", "index": 2 }),
            json!({
                "type": "message_delta",
                "delta": { "stop_reason": "end_turn" },
            }),
            json!({ "type": "message_stop" }),
        ];
        let sse = events
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();

        let response =
            parse_anthropic_sse(&sse).expect("omitted optional passive metadata remains valid");
        let replay = response
            .provider_replay
            .iter()
            .map(ProviderReplayItem::raw_value)
            .collect::<Result<Vec<_>, _>>()
            .expect("replay remains valid JSON");

        assert_eq!(response.assistant.text(), "Rust");
        assert_eq!(replay, vec![search_result, fetch_result, text]);
    }

    #[test]
    fn anthropic_sse_rejects_malformed_known_content_sequences_and_huge_indices() {
        let cases = [
            (
                "missing start index",
                vec![json!({
                    "type": "content_block_start",
                    "content_block": { "type": "text", "text": "" },
                })],
            ),
            (
                "content block missing type",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "text": "" },
                })],
            ),
            (
                "unsupported content block type",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "future_content" },
                })],
            ),
            (
                "tool content block missing id",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "name": "read",
                        "input": {},
                    },
                })],
            ),
            (
                "hosted result scalar content",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "web_search_tool_result",
                        "tool_use_id": "srvtoolu_1",
                        "content": "invalid",
                    },
                })],
            ),
            (
                "wrong start index type",
                vec![json!({
                    "type": "content_block_start",
                    "index": "0",
                    "content_block": { "type": "text", "text": "" },
                })],
            ),
            (
                "huge start index",
                vec![json!({
                    "type": "content_block_start",
                    "index": u64::MAX,
                    "content_block": { "type": "text", "text": "" },
                })],
            ),
            (
                "gapped start index",
                vec![json!({
                    "type": "content_block_start",
                    "index": 1,
                    "content_block": { "type": "text", "text": "" },
                })],
            ),
            (
                "missing content block",
                vec![json!({ "type": "content_block_start", "index": 0 })],
            ),
            (
                "scalar content block",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": "text",
                })],
            ),
            (
                "prepopulated text",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": { "type": "text", "text": "already here" },
                })],
            ),
            (
                "prepopulated tool input",
                vec![json!({
                    "type": "content_block_start",
                    "index": 0,
                    "content_block": {
                        "type": "tool_use",
                        "id": "toolu_1",
                        "name": "read",
                        "input": { "path": "README.md" },
                    },
                })],
            ),
            (
                "duplicate start",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                ],
            ),
            (
                "delta missing index",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "delta": { "type": "text_delta", "text": "hello" },
                    }),
                ],
            ),
            (
                "delta missing object",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({ "type": "content_block_delta", "index": 0 }),
                ],
            ),
            (
                "delta for nonexistent block",
                vec![json!({
                    "type": "content_block_delta",
                    "index": 0,
                    "delta": { "type": "text_delta", "text": "hello" },
                })],
            ),
            (
                "delta wrong active index",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 1,
                        "delta": { "type": "text_delta", "text": "hello" },
                    }),
                ],
            ),
            (
                "delta missing type",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "text": "hello" },
                    }),
                ],
            ),
            (
                "unknown delta type",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "future_delta", "text": "hello" },
                    }),
                ],
            ),
            (
                "delta missing required text",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "text_delta" },
                    }),
                ],
            ),
            (
                "delta block type mismatch",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "input_json_delta", "partial_json": "{}" },
                    }),
                ],
            ),
            (
                "malformed streamed tool json",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "tool_use",
                            "id": "toolu_1",
                            "name": "read",
                            "input": {},
                        },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "input_json_delta", "partial_json": "{" },
                    }),
                    json!({ "type": "content_block_stop", "index": 0 }),
                ],
            ),
            (
                "nonobject streamed tool json",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "server_tool_use",
                            "id": "srvtoolu_1",
                            "name": "web_search",
                            "input": {},
                        },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "input_json_delta", "partial_json": "[]" },
                    }),
                    json!({ "type": "content_block_stop", "index": 0 }),
                ],
            ),
            (
                "thinking missing signature",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": {
                            "type": "thinking",
                            "thinking": "",
                            "signature": "",
                        },
                    }),
                    json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "thinking_delta", "thinking": "private" },
                    }),
                    json!({ "type": "content_block_stop", "index": 0 }),
                ],
            ),
            (
                "stop missing index",
                vec![json!({ "type": "content_block_stop" })],
            ),
            (
                "stop nonexistent block",
                vec![json!({ "type": "content_block_stop", "index": 0 })],
            ),
            (
                "stop wrong active index",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({ "type": "content_block_stop", "index": 1 }),
                ],
            ),
            (
                "duplicate stop",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({ "type": "content_block_stop", "index": 0 }),
                    json!({ "type": "content_block_stop", "index": 0 }),
                ],
            ),
            (
                "duplicate completed index",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({ "type": "content_block_stop", "index": 0 }),
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                ],
            ),
            (
                "message stop with open block",
                vec![
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                    json!({ "type": "message_stop" }),
                ],
            ),
            (
                "content after terminal delta",
                vec![
                    json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": "end_turn" },
                    }),
                    json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" },
                    }),
                ],
            ),
            (
                "message delta missing delta",
                vec![json!({ "type": "message_delta" })],
            ),
            (
                "message delta scalar delta",
                vec![json!({
                    "type": "message_delta",
                    "delta": "end_turn",
                })],
            ),
            (
                "message delta without stop reason or usage",
                vec![json!({
                    "type": "message_delta",
                    "delta": { "stop_sequence": null },
                })],
            ),
            (
                "message delta nonstring stop reason",
                vec![json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": 1 },
                })],
            ),
            (
                "message delta empty stop reason",
                vec![json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "" },
                })],
            ),
            (
                "message delta scalar usage",
                vec![json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": null },
                    "usage": 1,
                })],
            ),
            (
                "message delta malformed token count",
                vec![json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": null },
                    "usage": { "output_tokens": "1" },
                })],
            ),
            (
                "message delta empty usage",
                vec![json!({
                    "type": "message_delta",
                    "delta": {},
                    "usage": {},
                })],
            ),
            (
                "message delta scalar stop details",
                vec![json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": "refusal",
                        "stop_details": "refused",
                    },
                })],
            ),
            (
                "message delta malformed stop detail",
                vec![json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": "refusal",
                        "stop_details": { "category": 1 },
                    },
                })],
            ),
            (
                "message delta details without terminal reason",
                vec![json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": null,
                        "stop_details": { "category": "policy" },
                    },
                })],
            ),
        ];

        for (name, events) in cases {
            let mut all_events = vec![json!({
                "type": "message_start",
                "message": { "id": "msg_1", "content": [] },
            })];
            all_events.extend(events);
            all_events.extend([
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                }),
                json!({ "type": "message_stop" }),
            ]);
            let sse = all_events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            assert!(parse_anthropic_sse(&sse).is_err(), "{name}");
        }
    }

    #[test]
    fn anthropic_sse_rejects_conflicting_terminal_deltas() {
        let merged = parse_anthropic_sse(
            r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":"policy","explanation":null}}}

data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":"policy","explanation":"cannot comply"}}}

data: {"type":"message_stop"}
"#,
        )
        .expect("nonconflicting terminal details merge");
        assert_eq!(
            merged.stop_details,
            Some(ModelStopDetails {
                category: Some("policy".to_string()),
                explanation: Some("cannot comply".to_string()),
            })
        );

        let cases = [
            (
                "stop reason",
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "end_turn" },
                }),
                json!({
                    "type": "message_delta",
                    "delta": { "stop_reason": "max_tokens" },
                }),
            ),
            (
                "stop details",
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": "refusal",
                        "stop_details": {
                            "category": "cyber",
                            "explanation": "first",
                        },
                    },
                }),
                json!({
                    "type": "message_delta",
                    "delta": {
                        "stop_reason": "refusal",
                        "stop_details": {
                            "category": "privacy",
                            "explanation": "second",
                        },
                    },
                }),
            ),
        ];
        for (name, first, second) in cases {
            let events = [
                json!({
                    "type": "message_start",
                    "message": { "id": "msg_1", "content": [] },
                }),
                first,
                second,
                json!({ "type": "message_stop" }),
            ];
            let sse = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();

            let error = parse_anthropic_sse(&sse).expect_err(name);
            assert!(error.to_string().contains("conflicting"), "{name}: {error}");
        }
    }

    #[test]
    fn anthropic_sse_maps_max_tokens_after_required_message_stop() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[],"stop_reason":null,"usage":{"input_tokens":8,"output_tokens":1}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":64}}

data: {"type":"message_stop"}

data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":"ignored"}}
"#;

        let response = parse_anthropic_sse(sse).expect("sse parses");
        let usage = response.usage.expect("usage should be parsed");

        assert_eq!(response.assistant.text(), "partial");
        assert_eq!(response.stop_reason, ModelStopReason::MaxOutputTokens);
        assert_eq!(usage.input_tokens, Some(8));
        assert_eq!(usage.output_tokens, Some(64));
    }

    #[test]
    fn anthropic_sse_rejects_incomplete_and_unknown_stop_reasons_without_partial_replay() {
        for (status, reason) in [
            ("incomplete", "pause_turn"),
            ("incomplete", "model_context_window_exceeded"),
            ("unknown_stop_reason", "future_stop_reason"),
        ] {
            let sse = format!(
                r#"
data: {{"type":"message_start","message":{{"id":"msg_1","content":[],"usage":{{"input_tokens":8,"output_tokens":1}}}}}}

data: {{"type":"content_block_start","index":0,"content_block":{{"type":"text","text":""}}}}

data: {{"type":"content_block_delta","index":0,"delta":{{"type":"text_delta","text":"partial"}}}}

data: {{"type":"content_block_stop","index":0}}

data: {{"type":"message_delta","delta":{{"stop_reason":"{reason}"}},"usage":{{"output_tokens":2}}}}

data: {{"type":"message_stop"}}
"#
            );
            let error = parse_anthropic_sse(&sse).expect_err(reason);
            match error {
                ProviderError::Incomplete {
                    status: actual_status,
                    reason: actual_reason,
                } => {
                    assert_eq!(actual_status, status);
                    assert_eq!(actual_reason, reason);
                }
                other => panic!("expected typed incomplete for {reason}, got {other:?}"),
            }
        }
    }

    #[test]
    fn anthropic_sse_requires_an_explicit_known_stop_reason() {
        for (name, delta) in [
            ("no delta", ""),
            (
                "null reason",
                r#"data: {"type":"message_delta","delta":{"stop_reason":null}}

"#,
            ),
            (
                "usage-only delta",
                r#"data: {"type":"message_delta","delta":{},"usage":{"output_tokens":1}}

"#,
            ),
        ] {
            let sse = [
                r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

"#,
                delta,
                r#"data: {"type":"message_stop"}

"#,
            ]
            .concat();
            let error = parse_anthropic_sse(&sse).expect_err(name);

            assert!(error.to_string().contains("stop_reason"), "{name}: {error}");
        }
    }

    #[test]
    fn anthropic_sse_requires_message_stop_not_done_or_eof() {
        let prefix = r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

data: {"type":"content_block_stop","index":0}
"#;
        for (name, suffix) in [("EOF", ""), ("done sentinel", "\ndata: [DONE]\n\n")] {
            let error = parse_anthropic_sse(&format!("{prefix}{suffix}")).expect_err(name);
            assert!(error.to_string().contains("before message_stop"), "{name}");
        }
        assert!(parse_anthropic_sse(
            "data: {\"type\":\"message_start\",\"message\":{}}\n\ndata: {not-json}\n\n"
        )
        .is_err());
    }

    #[test]
    fn anthropic_sse_tolerates_unknown_events_before_message_stop() {
        let response = parse_anthropic_sse(
            r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

data: {"type":"future_progress","opaque":{"value":1}}

data: {"type":"ping"}

data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}

data: {"type":"message_stop"}
"#,
        )
        .expect("unknown nonterminal event is forward compatible");

        assert_eq!(response.stop_reason, ModelStopReason::Complete);
    }

    #[test]
    fn anthropic_sse_maps_refusal_before_output_with_details() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_refused","type":"message","role":"assistant","model":"claude-fable-5","content":[],"stop_reason":null,"stop_details":null,"usage":{"input_tokens":412,"output_tokens":0}}}

data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":"cyber","explanation":"This request was declined because it could enable cyber harm."}},"usage":{"output_tokens":0}}

data: {"type":"message_stop"}
"#;

        let response = parse_anthropic_sse(sse).expect("refusal is a terminal response");

        assert_eq!(response.stop_reason, ModelStopReason::Refusal);
        assert!(response.assistant.items.is_empty());
        assert!(response.provider_replay.is_empty());
        assert_eq!(
            response.stop_details,
            Some(ModelStopDetails {
                category: Some("cyber".to_string()),
                explanation: Some(
                    "This request was declined because it could enable cyber harm.".to_string()
                ),
            })
        );
        assert_eq!(
            response.refusal_error().as_deref(),
            Some(
                "provider refused the request (cyber): This request was declined because it could enable cyber harm."
            )
        );
    }

    #[test]
    fn anthropic_sse_refusal_discards_partial_text_tool_and_replay() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_refused","type":"message","role":"assistant","model":"claude-fable-5","content":[],"stop_reason":null,"stop_details":null,"usage":{"input_tokens":12,"output_tokens":1}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"private partial"}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"unsafe partial"}}

data: {"type":"content_block_stop","index":1}

data: {"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_partial","name":"str_replace_based_edit_tool","input":{}}}

data: {"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"}}

data: {"type":"content_block_stop","index":2}

data: {"type":"content_block_start","index":3,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":3,"delta":{"type":"text_delta","text":"unfinished partial"}}

data: {"type":"content_block_stop","index":3}

data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_sequence":null,"stop_details":{"type":"refusal","category":null,"explanation":null}},"usage":{"output_tokens":22}}

data: {"type":"message_stop"}
"#;

        let response = parse_anthropic_sse(sse).expect("refusal is a terminal response");

        assert_eq!(response.stop_reason, ModelStopReason::Refusal);
        assert!(response.assistant.items.is_empty());
        assert!(response.provider_replay.is_empty());
        assert_eq!(response.stop_details, Some(ModelStopDetails::default()));
        assert_eq!(
            response.refusal_error().as_deref(),
            Some("provider refused the request")
        );
        assert_eq!(
            response.usage.and_then(|usage| usage.output_tokens),
            Some(22)
        );
    }

    #[test]
    fn anthropic_sse_maps_overloaded_error_to_status() {
        let sse = r#"
event: error
data: {"type":"error","error":{"type":"overloaded_error","message":"server overloaded"}}
"#;

        let error = parse_anthropic_sse(sse).expect_err("sse should fail");

        match &error {
            ProviderError::Status { status, message } => {
                assert_eq!(*status, 529);
                assert!(message.contains("overloaded_error"));
                assert!(message.contains("server overloaded"));
            }
            _ => panic!("expected status error, got {error:?}"),
        }
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
    fn anthropic_serializer_preserves_raw_replay_tool_names() {
        let raw = json!({
            "type": "tool_use",
            "id": "toolu_1",
            "name": "str_replace_based_edit_tool",
            "input": { "path": "README.md" },
        });
        let messages = transcript_to_messages(
            &crate::PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::ToolCall(ToolCall {
                        id: ToolCallId::new("toolu_1"),
                        tool_name: "Edit".to_string(),
                        args_json: r#"{"path":"README.md"}"#.to_string(),
                    })],
                }),
                provider_replay: vec![ProviderReplayItem::new(ProviderKind::Claude, &raw).unwrap()],
            }],
        )
        .expect("messages render");

        assert_eq!(messages[0]["content"], json!([raw]));
    }

    #[test]
    fn daemon_tool_observation_renders_as_anthropic_synthetic_tool_pair() {
        let observation = agent_vocab::DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_delegation_1_attempt_1"),
            "delegation_1",
            Some("Delegation delegation_1 completed with status done: 1 ok, 0 failed.".to_string()),
            json!({
                "delegation_id": "delegation_1",
                "status": "done",
                "subagents": [{
                    "id": "child_1",
                    "transcript_file": "child_1/transcript.md",
                }],
            }),
        );

        let messages = transcript_to_messages(
            &crate::PromptSections::default(),
            &[TranscriptItem::DaemonToolObservation(observation).into()],
        )
        .expect("messages render");

        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(
            messages[0]["content"][0]["id"],
            "toolu_call_delegation_1_attempt_1"
        );
        assert_eq!(messages[0]["content"][0]["name"], "inspect_delegation");
        assert_eq!(
            messages[0]["content"][0]["input"]["delegation_id"],
            "delegation_1"
        );
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(
            messages[1]["content"][0]["tool_use_id"],
            "toolu_call_delegation_1_attempt_1"
        );
        assert_eq!(messages[1]["content"][0]["is_error"], false);
        assert!(messages[1]["content"][0]["content"]
            .as_str()
            .expect("json output")
            .contains("\"delegation_id\": \"delegation_1\""));
    }

    #[test]
    fn daemon_tool_observation_after_tool_result_does_not_split_anthropic_tool_pairs() {
        let tool_call = ToolCall {
            id: ToolCallId::new("toolu_1"),
            tool_name: "read".to_string(),
            args_json: "{\"path\":\"README.md\"}".to_string(),
        };
        let observation = agent_vocab::DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_delegation_1_attempt_1"),
            "delegation_1",
            None,
            json!({ "delegation_id": "delegation_1", "status": "done" }),
        );

        let messages = transcript_to_messages(
            &crate::PromptSections::default(),
            &[
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
                TranscriptItem::DaemonToolObservation(observation).into(),
            ],
        )
        .expect("messages render");

        assert_eq!(messages[0]["role"], "assistant");
        assert_eq!(messages[0]["content"][0]["type"], "tool_use");
        assert_eq!(messages[1]["role"], "user");
        assert_eq!(messages[1]["content"][0]["type"], "tool_result");
        assert_eq!(messages[1]["content"][0]["tool_use_id"], "toolu_1");
        assert_eq!(messages[2]["role"], "assistant");
        assert_eq!(messages[2]["content"][0]["type"], "tool_use");
        assert_eq!(
            messages[2]["content"][0]["id"],
            "toolu_call_delegation_1_attempt_1"
        );
        assert_eq!(messages[3]["role"], "user");
        assert_eq!(messages[3]["content"][0]["type"], "tool_result");
        assert_eq!(
            messages[3]["content"][0]["tool_use_id"],
            "toolu_call_delegation_1_attempt_1"
        );
    }

    #[test]
    fn stable_system_block_keeps_one_hour_ttl() {
        let body = messages_body(ModelRequest {
            model: "claude-opus-4-7".to_string(),
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
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
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript,
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
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
            transcript_cache_prefix_len: None,
            prompt: PromptSections::stable("stable rules"),
            transcript,
            tool_profile: ProviderToolProfile::None,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: None,
            turn_id: None,
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
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable("a stable system prompt long enough to fingerprint"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text(first_user)).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
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
                transcript_cache_prefix_len: None,
                prompt: PromptSections::stable(stable),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("anything")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
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
