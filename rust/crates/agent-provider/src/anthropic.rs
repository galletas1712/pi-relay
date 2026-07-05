use agent_tools::{tool_display, ProviderTool, ToolDisplayInput};
use agent_vocab::{
    AssistantItem, AssistantMessage, ContentBlock, ProviderKind, ProviderReplayItem,
    ReasoningEffort, ReplayDisplay, ToolCall, ToolCallId, TranscriptItem, UserMessage,
};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{
    collections::HashMap,
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
    ModelTranscriptEntry, ProviderError, ProviderModelMetadata, ProviderResult,
    ProviderTokenCountRequest, ProviderTokenCountResponse, ProviderToolProfile, ProviderUsage,
};

const DEFAULT_MAX_OUTPUT_BUDGET: u32 = 64_000;
const UNKNOWN_MODEL_MAX_OUTPUT_TOKENS: u32 = 64_000;
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
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

fn reject_ordinary_anthropic_compaction() -> ProviderError {
    ProviderError::Provider(
        "Anthropic ordinary response unexpectedly contained inline compaction; refusing to persist partial or opaque response content"
            .to_string(),
    )
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

fn anthropic_auto_compact_limit(window: usize) -> usize {
    if window == 1_000_000 {
        500_000
    } else {
        window / 100 * 85 + window % 100 * 85 / 100
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
    effort: ModelsApiEffortCapability,
    thinking: ModelsApiThinkingCapability,
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
            AnthropicModelCapabilities::default(),
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
        let body = messages_body_with_metadata(request, &metadata)?;

        let response = send_provider_generation_request(
            self.client
                .post(format!("{}/messages", self.base_url.trim_end_matches('/')))
                .header("accept", "text/event-stream")
                .header("x-api-key", &self.api_key)
                .header("anthropic-version", "2023-06-01")
                .header("anthropic-beta", anthropic_beta_header())
                .header("anthropic-dangerous-direct-browser-access", "true")
                .header("User-Agent", CLAUDE_CODE_USER_AGENT)
                .header("x-app", "cli")
                .header("X-Claude-Code-Session-Id", session_id)
                .header("x-client-request-id", client_request_id())
                .json(&body),
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
        let body = count_tokens_body_with_metadata(request, &metadata)?;

        let response = self
            .client
            .post(format!(
                "{}/messages/count_tokens",
                self.base_url.trim_end_matches('/')
            ))
            .header("accept", "application/json")
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", anthropic_beta_header())
            .header("anthropic-dangerous-direct-browser-access", "true")
            .header("User-Agent", CLAUDE_CODE_USER_AGENT)
            .header("x-app", "cli")
            .header("X-Claude-Code-Session-Id", session_id)
            .header("x-client-request-id", client_request_id())
            .json(&body)
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
    messages_body_with_metadata(request, &metadata)
}

fn messages_body_with_metadata(
    request: ModelRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<Value> {
    let tool_profile = request.tool_profile;
    // The Messages API requires `max_tokens`. Keep 64k as the ordinary-turn
    // target recommended for xhigh/max agentic work, but clamp both defaults
    // and explicit overrides to the model's authoritative output ceiling.
    let max_tokens = request
        .max_tokens
        .unwrap_or(DEFAULT_MAX_OUTPUT_BUDGET)
        .min(metadata.max_tokens);
    anthropic_request_body(AnthropicRequestBodyInput {
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
    })
}

#[cfg(test)]
fn count_tokens_body(request: ProviderTokenCountRequest) -> ProviderResult<Value> {
    let metadata = static_anthropic_model_metadata(&request.model);
    count_tokens_body_with_metadata(request, &metadata)
}

fn count_tokens_body_with_metadata(
    request: ProviderTokenCountRequest,
    metadata: &AnthropicModelMetadata,
) -> ProviderResult<Value> {
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
        // Anthropic's /messages/count_tokens endpoint accepts the same prompt,
        // message, thinking, and tool-shaping fields as /messages, but rejects
        // generation-only budgets such as max_tokens.
        max_tokens: None,
        reasoning_effort: Some(request.reasoning_effort),
        capabilities: metadata.capabilities,
        cache_transcript: false,
        transcript_cache_prefix_len: None,
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
    capabilities: AnthropicModelCapabilities,
    cache_transcript: bool,
    transcript_cache_prefix_len: Option<usize>,
}

fn anthropic_request_body(input: AnthropicRequestBodyInput) -> ProviderResult<Value> {
    let capabilities = input.capabilities;
    let messages = transcript_to_messages_for_request(&input)?;
    let mut body = json!({
        "model": input.model,
        "messages": messages,
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
    Ok(body)
}

fn transcript_to_messages_for_request(
    input: &AnthropicRequestBodyInput,
) -> ProviderResult<Vec<Value>> {
    if !input.cache_transcript {
        let mut messages = transcript_to_messages(&input.prompt, &input.transcript)?;
        append_dynamic_context_message(&input.prompt, &mut messages);
        return Ok(messages);
    }
    let Some(prefix_len) = input.transcript_cache_prefix_len else {
        let mut messages = transcript_to_messages(&input.prompt, &input.transcript)?;
        add_transcript_cache_breakpoints(&mut messages);
        append_dynamic_context_message(&input.prompt, &mut messages);
        return Ok(messages);
    };

    let prefix_len = prefix_len.min(input.transcript.len());
    let (prefix, suffix) = input.transcript.split_at(prefix_len);
    let mut messages = transcript_to_messages(&input.prompt, prefix)?;
    add_transcript_cache_breakpoints(&mut messages);
    messages.extend(transcript_to_messages(&input.prompt, suffix)?);
    append_dynamic_context_message(&input.prompt, &mut messages);
    Ok(messages)
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
        // Preserve the historical Claude normalization inside the adapter now
        // that provider-neutral core passes raw configured intent.
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
/// truly supplies no stable prefix; normal daemon requests and local
/// compaction prompts are stable, and Anthropic remote compaction is not
/// supported.
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
                let mut content = anthropic_replay_blocks(entry)?;
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
    Ok(messages)
}

fn anthropic_replay_blocks(entry: &ModelTranscriptEntry) -> ProviderResult<Vec<Value>> {
    let blocks = entry
        .provider_replay_values_for(ProviderKind::Claude)
        .map_err(ProviderError::Json)?;
    for block in &blocks {
        anthropic_block_type(block, "persisted Anthropic replay block")?;
    }
    Ok(blocks)
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

#[cfg(test)]
fn parse_anthropic_message(response: &Value) -> ProviderResult<ModelResponse> {
    let stop_reason = anthropic_stop_reason(response);
    let stop_details = anthropic_stop_details(response.get("stop_details"))?;
    if stop_reason == ModelStopReason::Refusal {
        return Ok(ModelResponse {
            assistant: AssistantMessage { items: Vec::new() },
            provider_replay: Vec::new(),
            usage: response.get("usage").and_then(anthropic_usage),
            stop_reason,
            stop_details,
        });
    }
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
        stop_reason,
        stop_details,
    })
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

#[cfg(test)]
fn parse_anthropic_sse(text: &str) -> ProviderResult<ModelResponse> {
    let mut state = AnthropicStreamState::default();
    read_json_sse_text(text, |event| state.process_sse_event(event))?;
    state.finish()
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
        if self
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

#[cfg(test)]
fn anthropic_stop_reason(response: &Value) -> ModelStopReason {
    match response.get("stop_reason").and_then(Value::as_str) {
        Some("max_tokens") => ModelStopReason::MaxOutputTokens,
        Some("refusal") => ModelStopReason::Refusal,
        _ => ModelStopReason::Complete,
    }
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
    fn adaptive_effort_normalization_is_adapter_owned_for_ordinary_and_count() {
        for effort in [ReasoningEffort::None, ReasoningEffort::Minimal] {
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
                "Grep",
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
        assert_eq!(body["tools"][4]["name"], "Grep");
        assert!(body["tools"][4].get("type").is_none());
        assert_eq!(body["tools"][5]["name"], "inspect_delegation");
        assert!(body["tools"][5].get("type").is_none());
        assert_eq!(body["tools"][6]["name"], "interrupt_subagent");
        assert!(body["tools"][6].get("type").is_none());
        assert_eq!(body["tools"][7]["name"], "LoadSkill");
        assert!(body["tools"][7].get("type").is_none());
        assert_eq!(body["tools"][8]["name"], "steer_subagent");
        assert!(body["tools"][8].get("type").is_none());
        assert_eq!(body["tools"][9]["type"], "text_editor_20250728");
        assert_eq!(body["tools"][9]["name"], "str_replace_based_edit_tool");
        assert_eq!(body["tools"][10]["name"], "web_fetch");
        assert!(body["tools"][10].get("type").is_none());
        assert_eq!(body["tools"][11]["name"], "web_search");
        assert!(body["tools"][11].get("type").is_none());
        // Native coding tools also carry no per-tool cache_control: the
        // stable-system breakpoint covers them via the cumulative hash.
        for index in 0..12 {
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

        assert_eq!(usage.input_tokens, Some(200));
        assert_eq!(usage.output_tokens, Some(20));
        assert_eq!(usage.total_tokens, Some(220));
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
    fn anthropic_sse_maps_max_tokens_stop_reason_and_done_sentinel() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-4-7","content":[],"stop_reason":null,"usage":{"input_tokens":8,"output_tokens":1}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"max_tokens","stop_sequence":null},"usage":{"output_tokens":64}}

data: {"type":"message_stop"}

data: [DONE]

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
    fn anthropic_sse_refusal_discards_partial_output_and_replay() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_refused","content":[],"usage":{"input_tokens":12,"output_tokens":0}}}

data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}

data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"partial"}}

data: {"type":"content_block_stop","index":0}

data: {"type":"message_delta","delta":{"stop_reason":"refusal","stop_details":{"category":"cyber","explanation":"declined"}},"usage":{"output_tokens":1}}

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
                explanation: Some("declined".to_string()),
            })
        );
    }

    #[test]
    fn anthropic_sse_retains_incomplete_stop_reason() {
        let sse = r#"
data: {"type":"message_start","message":{"id":"msg_paused","content":[]}}

data: {"type":"message_delta","delta":{"stop_reason":"pause_turn"}}
"#;

        match parse_anthropic_sse(sse).expect_err("pause_turn is not success") {
            ProviderError::Incomplete { status, reason } => {
                assert_eq!(status, "incomplete");
                assert_eq!(reason, "pause_turn");
            }
            other => panic!("expected typed incomplete error, got {other:?}"),
        }
    }

    #[test]
    fn anthropic_sse_rejects_gapped_blocks_and_done_only_completion() {
        let gapped = r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

data: {"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}
"#;
        let done_only = r#"
data: {"type":"message_start","message":{"id":"msg_1","content":[]}}

data: [DONE]
"#;

        assert!(parse_anthropic_sse(gapped)
            .expect_err("gapped index must fail")
            .to_string()
            .contains("not contiguous"));
        assert!(parse_anthropic_sse(done_only)
            .expect_err("done sentinel is not success")
            .to_string()
            .contains("before message_stop"));
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
    fn anthropic_serializer_rejects_corrupt_replay_without_fallback() {
        let entry = ModelTranscriptEntry {
            item: TranscriptItem::AssistantMessage(AssistantMessage {
                items: vec![AssistantItem::Text("must not substitute".to_string())],
            }),
            provider_replay: vec![ProviderReplayItem {
                provider: ProviderKind::Claude,
                raw_json: "{".to_string(),
                display: None,
            }],
        };

        assert!(transcript_to_messages(&crate::PromptSections::default(), &[entry]).is_err());
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
