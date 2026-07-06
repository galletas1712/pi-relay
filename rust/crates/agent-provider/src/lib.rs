#![forbid(unsafe_code)]

use agent_tools::{ProviderTool, ToolRegistry};
use agent_vocab::{
    AssistantMessage, ProviderKind, ProviderReplayItem, ReasoningEffort, ToolCall, TranscriptItem,
    TurnId,
};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;
use thiserror::Error;

#[cfg(test)]
macro_rules! test_provider_model_input {
    (
        model: $model:expr,
        prompt: $prompt:expr,
        transcript: $transcript:expr,
        tool_profile: $tool_profile:expr,
        tools: $tools:expr,
        reasoning_effort: $reasoning_effort:expr,
        prompt_cache_key: $prompt_cache_key:expr,
        session_id: $session_id:expr $(,)?
    ) => {{
        let prompt_cache_key: Option<String> = $prompt_cache_key;
        let session_id: Option<String> = $session_id;
        let mut input = $crate::ProviderModelInput::new(
            $model,
            $prompt,
            $transcript,
            $tool_profile,
            $tools,
            $reasoning_effort,
        );
        if let Some(prompt_cache_key) = prompt_cache_key {
            input.set_prompt_cache_key(prompt_cache_key);
        }
        if let Some(session_id) = session_id {
            input = input.with_session_id(session_id);
        }
        std::sync::Arc::new(input)
    }};
}

#[cfg(test)]
macro_rules! test_model_request {
    (
        model: $model:expr,
        transcript_cache_prefix_len: $transcript_cache_prefix_len:expr,
        prompt: $prompt:expr,
        transcript: $transcript:expr,
        tool_profile: $tool_profile:expr,
        tools: $tools:expr,
        max_tokens: $max_tokens:expr,
        reasoning_effort: $reasoning_effort:expr,
        prompt_cache_key: $prompt_cache_key:expr,
        session_id: $session_id:expr,
        turn_id: $turn_id:expr $(,)?
    ) => {{
        let input = test_provider_model_input!(
            model: $model,
            prompt: $prompt,
            transcript: $transcript,
            tool_profile: $tool_profile,
            tools: $tools,
            reasoning_effort: $reasoning_effort,
            prompt_cache_key: $prompt_cache_key,
            session_id: $session_id,
        );
        let mut request = $crate::ModelRequest::new(input);
        request.transcript_cache_prefix_len = $transcript_cache_prefix_len;
        request.max_tokens = $max_tokens;
        request.turn_id = $turn_id;
        request
    }};
}

#[cfg(test)]
macro_rules! test_compaction_request {
    (
        model: $model:expr,
        prompt: $prompt:expr,
        transcript: $transcript:expr,
        tool_profile: $tool_profile:expr,
        tools: $tools:expr,
        reasoning_effort: $reasoning_effort:expr,
        prompt_cache_key: $prompt_cache_key:expr,
        session_id: $session_id:expr,
        compaction_instructions: $compaction_instructions:expr $(,)?
    ) => {{
        let input = test_provider_model_input!(
            model: $model,
            prompt: $prompt,
            transcript: $transcript,
            tool_profile: $tool_profile,
            tools: $tools,
            reasoning_effort: $reasoning_effort,
            prompt_cache_key: $prompt_cache_key,
            session_id: $session_id,
        );
        let mut request = $crate::ProviderCompactionRequest::new(input);
        request.compaction_instructions = $compaction_instructions;
        request
    }};
}

#[cfg(test)]
macro_rules! test_token_count_request {
    (
        model: $model:expr,
        prompt: $prompt:expr,
        transcript: $transcript:expr,
        tool_profile: $tool_profile:expr,
        tools: $tools:expr,
        max_tokens: $max_tokens:expr,
        reasoning_effort: $reasoning_effort:expr,
        prompt_cache_key: $prompt_cache_key:expr,
        session_id: $session_id:expr $(,)?
    ) => {{
        let mut request = $crate::ProviderTokenCountRequest::new(test_provider_model_input!(
            model: $model,
            prompt: $prompt,
            transcript: $transcript,
            tool_profile: $tool_profile,
            tools: $tools,
            reasoning_effort: $reasoning_effort,
            prompt_cache_key: $prompt_cache_key,
            session_id: $session_id,
        ));
        request.max_tokens = $max_tokens;
        request
    }};
}

pub mod anthropic;
mod common;
mod http;
pub mod openai;
mod sse;
mod token_estimator;
mod transcript;

pub use token_estimator::{
    approx_tokens_from_byte_count, estimate_model_input, estimate_model_input_tokens,
    estimate_transcript_tokens, TokenEstimate,
};
pub use transcript::normalize_transcript_for_provider;

/// Immutable provider-visible input shared by generation, accounting, and
/// retry operations.
///
/// Large prompt, transcript, and tool projections are independently
/// reference-counted so a request that changes small routing metadata can
/// retain the same provider-visible allocations.
#[derive(Debug, Clone)]
pub struct ProviderModelInput {
    model: Arc<str>,
    prompt: Arc<PromptSections>,
    transcript: Arc<[ModelTranscriptEntry]>,
    tool_profile: ProviderToolProfile,
    tools: Arc<[ProviderTool]>,
    reasoning_effort: ReasoningEffort,
    prompt_cache_key: Option<Arc<str>>,
    session_id: Option<Arc<str>>,
}

impl ProviderModelInput {
    pub fn new(
        model: impl Into<String>,
        prompt: PromptSections,
        transcript: Vec<ModelTranscriptEntry>,
        tool_profile: ProviderToolProfile,
        tools: Vec<ProviderTool>,
        reasoning_effort: ReasoningEffort,
    ) -> Self {
        Self {
            model: Arc::from(model.into()),
            prompt: Arc::new(prompt),
            transcript: transcript.into(),
            tool_profile,
            tools: tools.into(),
            reasoning_effort,
            prompt_cache_key: None,
            session_id: None,
        }
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn prompt(&self) -> &PromptSections {
        &self.prompt
    }

    pub fn transcript(&self) -> &[ModelTranscriptEntry] {
        &self.transcript
    }

    pub fn tool_profile(&self) -> ProviderToolProfile {
        self.tool_profile
    }

    pub fn tools(&self) -> &[ProviderTool] {
        &self.tools
    }

    pub fn reasoning_effort(&self) -> ReasoningEffort {
        self.reasoning_effort
    }

    pub fn prompt_cache_key(&self) -> Option<&str> {
        self.prompt_cache_key.as_deref()
    }

    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    pub fn with_prompt_cache_key(mut self, prompt_cache_key: impl Into<String>) -> Self {
        self.prompt_cache_key = Some(Arc::from(prompt_cache_key.into()));
        self
    }

    pub fn with_session_id(mut self, session_id: impl Into<String>) -> Self {
        self.session_id = Some(Arc::from(session_id.into()));
        self
    }

    pub fn with_reasoning_effort(mut self, reasoning_effort: ReasoningEffort) -> Self {
        self.reasoning_effort = reasoning_effort;
        self
    }

    fn set_prompt_cache_key(&mut self, prompt_cache_key: impl Into<String>) {
        self.prompt_cache_key = Some(Arc::from(prompt_cache_key.into()));
    }

    fn set_session_id_if_missing(&mut self, session_id: impl Into<String>) {
        self.session_id
            .get_or_insert_with(|| Arc::from(session_id.into()));
    }
}

#[derive(Debug, Clone)]
pub struct ModelRequest {
    input: Arc<ProviderModelInput>,
    transcript_suffix: Arc<[ModelTranscriptEntry]>,
    body_generation: Arc<()>,
    /// If set, providers that support transcript cache markers should place
    /// those markers only within the first `n` transcript entries.
    ///
    /// This is useful for non-persistent sidecar requests that append an
    /// instruction after an otherwise normal model-turn transcript: the
    /// provider-visible prefix stays identical to the regular request, and the
    /// sidecar-only suffix does not become the cache breakpoint.
    pub transcript_cache_prefix_len: Option<usize>,
    pub max_tokens: Option<u32>,
    /// Turn identifier for the user turn that owns this model request.
    ///
    /// Codex treats `x-codex-turn-state` as turn-scoped sticky routing state:
    /// any value returned by an upstream request should be replayed by later
    /// requests for the same turn, but must not leak into future turns.
    pub turn_id: Option<TurnId>,
}

impl ModelRequest {
    pub fn new(input: Arc<ProviderModelInput>) -> Self {
        Self {
            input,
            transcript_suffix: Arc::from([]),
            body_generation: Arc::new(()),
            transcript_cache_prefix_len: None,
            max_tokens: None,
            turn_id: None,
        }
    }

    pub fn transcript_suffix(&self) -> &[ModelTranscriptEntry] {
        &self.transcript_suffix
    }

    pub fn with_transcript_suffix(mut self, suffix: Vec<ModelTranscriptEntry>) -> Self {
        self.transcript_suffix = suffix.into();
        self.body_generation = Arc::new(());
        self
    }

    pub fn with_turn_id(mut self, turn_id: TurnId) -> Self {
        self.turn_id = Some(turn_id);
        self
    }

    pub fn set_prompt_cache_key(&mut self, prompt_cache_key: impl Into<String>) {
        Arc::make_mut(&mut self.input).set_prompt_cache_key(prompt_cache_key);
        self.body_generation = Arc::new(());
    }

    pub fn set_session_id_if_missing(&mut self, session_id: impl Into<String>) {
        if self.input.session_id().is_none() {
            Arc::make_mut(&mut self.input).set_session_id_if_missing(session_id);
            self.body_generation = Arc::new(());
        }
    }

    fn prepared_body_generation(&self) -> PreparedBodyGeneration {
        PreparedBodyGeneration {
            marker: self.body_generation.clone(),
            transcript_cache_prefix_len: self.transcript_cache_prefix_len,
            max_tokens: self.max_tokens,
        }
    }
}

impl Deref for ModelRequest {
    type Target = ProviderModelInput;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeCompactionErrorKind {
    Unsupported,
    MalformedStream,
    UnexpectedStopReason,
    NullBlock,
    EmptyBlock,
    UnexpectedContent,
}

impl std::fmt::Display for NativeCompactionErrorKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Unsupported => "unsupported",
            Self::MalformedStream => "malformed_stream",
            Self::UnexpectedStopReason => "unexpected_stop_reason",
            Self::NullBlock => "null_block",
            Self::EmptyBlock => "empty_block",
            Self::UnexpectedContent => "unexpected_content",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCompactionRequest {
    input: Arc<ProviderModelInput>,
    /// Provider-native summary instructions. Providers with a dedicated
    /// compaction endpoint may ignore this when their wire contract derives
    /// instructions from `prompt`.
    pub compaction_instructions: Option<String>,
}

impl ProviderCompactionRequest {
    pub fn new(input: Arc<ProviderModelInput>) -> Self {
        Self {
            input,
            compaction_instructions: None,
        }
    }

    pub fn with_compaction_instructions(mut self, compaction_instructions: String) -> Self {
        self.compaction_instructions = Some(compaction_instructions);
        self
    }
}

impl Deref for ProviderCompactionRequest {
    type Target = ProviderModelInput;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

#[derive(Debug, Clone)]
pub struct ProviderCompactionResponse {
    /// Provider-returned text, if the provider exposes one. Provider-native
    /// compaction endpoints can return only opaque replay state.
    pub summary: Option<String>,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub usage: Option<ProviderUsage>,
}

#[derive(Debug, Clone)]
pub struct ProviderTokenCountRequest {
    input: Arc<ProviderModelInput>,
    pub max_tokens: Option<u32>,
}

impl ProviderTokenCountRequest {
    pub fn new(input: Arc<ProviderModelInput>) -> Self {
        Self {
            input,
            max_tokens: None,
        }
    }
}

impl Deref for ProviderTokenCountRequest {
    type Target = ProviderModelInput;

    fn deref(&self) -> &Self::Target {
        &self.input
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderTokenCountResponse {
    /// Effective input occupancy after any provider-native context edits
    /// already represented in the request.
    pub input_tokens: usize,
    /// Input occupancy before applying an existing provider-native compaction
    /// block, when the provider returns that diagnostic.
    pub original_input_tokens: Option<usize>,
}

/// Provider-normalized model limits consumed by the daemon scheduler.
///
/// Provider adapters own discovery, caching, and provider-specific threshold
/// policy. The daemon uses only the resolved current/default input window and
/// an optional provider-recommended automatic compaction limit.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProviderModelMetadata {
    pub max_input_tokens: Option<usize>,
    pub recommended_auto_compact_tokens: Option<usize>,
}

#[derive(Clone)]
struct PreparedBodyGeneration {
    marker: Arc<()>,
    transcript_cache_prefix_len: Option<usize>,
    max_tokens: Option<u32>,
}

impl PreparedBodyGeneration {
    fn matches(&self, request: &ModelRequest) -> bool {
        Arc::ptr_eq(&self.marker, &request.body_generation)
            && self.transcript_cache_prefix_len == request.transcript_cache_prefix_len
            && self.max_tokens == request.max_tokens
    }
}

struct OpaqueBodyGeneration;

impl fmt::Debug for OpaqueBodyGeneration {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<opaque>")
    }
}

#[derive(Clone)]
enum PreparedModelRequestInner {
    OpenAi(openai::OpenAiPreparedRequestMetadata),
    Anthropic(anthropic::AnthropicPreparedRequestMetadata),
}

/// Opaque, provider-owned bytes for one ordinary model generation request.
///
/// The artifact contains no credentials or mutable physical-attempt headers.
/// Provider adapters validate its private body-shaping metadata before reuse
/// after a provider or capability refresh.
#[derive(Clone)]
pub struct PreparedModelRequest {
    body_template: Arc<reqwest::Request>,
    content_type: &'static str,
    content_encoding: Option<&'static str>,
    body_generation: PreparedBodyGeneration,
    inner: PreparedModelRequestInner,
}

impl PreparedModelRequest {
    pub(crate) fn openai(
        request: &ModelRequest,
        bytes: Vec<u8>,
        metadata: openai::OpenAiPreparedRequestMetadata,
    ) -> Self {
        Self::new(
            request,
            bytes,
            Some("zstd"),
            PreparedModelRequestInner::OpenAi(metadata),
        )
    }

    pub(crate) fn anthropic(
        request: &ModelRequest,
        bytes: Vec<u8>,
        metadata: anthropic::AnthropicPreparedRequestMetadata,
    ) -> Self {
        Self::new(
            request,
            bytes,
            None,
            PreparedModelRequestInner::Anthropic(metadata),
        )
    }

    fn new(
        request: &ModelRequest,
        bytes: Vec<u8>,
        content_encoding: Option<&'static str>,
        inner: PreparedModelRequestInner,
    ) -> Self {
        let mut body_template = reqwest::Request::new(
            reqwest::Method::POST,
            reqwest::Url::parse("http://prepared.invalid/")
                .expect("static prepared request URL is valid"),
        );
        *body_template.body_mut() = Some(bytes.into());
        Self {
            body_template: Arc::new(body_template),
            content_type: "application/json",
            content_encoding,
            body_generation: request.prepared_body_generation(),
            inner,
        }
    }

    pub(crate) fn matches_request(&self, request: &ModelRequest) -> bool {
        self.body_generation.matches(request)
    }

    pub(crate) fn request_with_body(
        &self,
        mut request: reqwest::RequestBuilder,
    ) -> ProviderResult<reqwest::RequestBuilder> {
        let mut template = self.body_template.try_clone().ok_or_else(|| {
            ProviderError::Provider("prepared model request body was not reusable".to_string())
        })?;
        let body = template.body_mut().take().ok_or_else(|| {
            ProviderError::Provider("prepared model request body was missing".to_string())
        })?;
        request = request.header(reqwest::header::CONTENT_TYPE, self.content_type);
        if let Some(content_encoding) = self.content_encoding {
            request = request.header(reqwest::header::CONTENT_ENCODING, content_encoding);
        }
        Ok(request.body(body))
    }

    pub(crate) fn openai_metadata(&self) -> ProviderResult<&openai::OpenAiPreparedRequestMetadata> {
        match &self.inner {
            PreparedModelRequestInner::OpenAi(metadata) => Ok(metadata),
            PreparedModelRequestInner::Anthropic(_) => Err(ProviderError::Provider(
                "prepared model request belongs to a different provider".to_string(),
            )),
        }
    }

    pub(crate) fn anthropic_metadata(
        &self,
    ) -> ProviderResult<&anthropic::AnthropicPreparedRequestMetadata> {
        match &self.inner {
            PreparedModelRequestInner::Anthropic(metadata) => Ok(metadata),
            PreparedModelRequestInner::OpenAi(_) => Err(ProviderError::Provider(
                "prepared model request belongs to a different provider".to_string(),
            )),
        }
    }

    #[cfg(any(test, feature = "test-utils"))]
    pub fn body_allocation(&self) -> ProviderResult<(*const u8, usize)> {
        let body = self.body_bytes()?;
        Ok((body.as_ptr(), body.len()))
    }

    #[cfg(any(test, feature = "test-utils"))]
    fn body_bytes(&self) -> ProviderResult<&[u8]> {
        self.body_template
            .body()
            .and_then(reqwest::Body::as_bytes)
            .ok_or_else(|| {
                ProviderError::Provider("prepared model request body was missing".to_string())
            })
    }
}

impl fmt::Debug for PreparedModelRequest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let provider = match self.inner {
            PreparedModelRequestInner::OpenAi(_) => "openai",
            PreparedModelRequestInner::Anthropic(_) => "claude",
        };
        let body_len = self
            .body_template
            .body()
            .and_then(reqwest::Body::as_bytes)
            .map_or(0, <[u8]>::len);
        formatter
            .debug_struct("PreparedModelRequest")
            .field("provider", &provider)
            .field("body_len", &body_len)
            .field("content_type", &self.content_type)
            .field("content_encoding", &self.content_encoding)
            .field("body_generation", &OpaqueBodyGeneration)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderToolProfile {
    None,
    CustomDefinitions,
    OpenAiCoding,
    AnthropicCoding,
}

impl ProviderToolProfile {
    pub fn for_provider(kind: ProviderKind) -> Self {
        match kind {
            ProviderKind::OpenAi => Self::OpenAiCoding,
            ProviderKind::Claude => Self::AnthropicCoding,
        }
    }
}

fn effective_provider_tools(
    profile: ProviderToolProfile,
    tools: &[ProviderTool],
) -> Cow<'_, [ProviderTool]> {
    if !tools.is_empty() {
        return Cow::Borrowed(tools);
    }
    Cow::Owned(match profile {
        ProviderToolProfile::OpenAiCoding => {
            ToolRegistry::with_builtin_tools().provider_tools_for_provider(ProviderKind::OpenAi)
        }
        ProviderToolProfile::AnthropicCoding => {
            ToolRegistry::with_builtin_tools().provider_tools_for_provider(ProviderKind::Claude)
        }
        ProviderToolProfile::None | ProviderToolProfile::CustomDefinitions => Vec::new(),
    })
}

impl ModelTranscriptEntry {
    pub(crate) fn provider_replay_values_for(
        &self,
        provider: ProviderKind,
    ) -> serde_json::Result<Vec<Value>> {
        self.provider_replay
            .iter()
            .filter(|record| record.provider == provider)
            .map(ProviderReplayItem::raw_value)
            .collect()
    }

    pub fn item(&self) -> &TranscriptItem {
        &self.item
    }
}

pub fn canonical_tool_call_for_provider(provider: ProviderKind, call: &ToolCall) -> ToolCall {
    let tool_name = canonical_tool_name_for_provider(provider, &call.tool_name);
    if tool_name == call.tool_name {
        return call.clone();
    }
    ToolCall {
        id: call.id.clone(),
        tool_name: tool_name.to_string(),
        args_json: call.args_json.clone(),
    }
}

pub fn canonical_tool_name_for_provider(provider: ProviderKind, name: &str) -> &str {
    match provider {
        ProviderKind::OpenAi => match name {
            "apply_patch" => "Edit",
            "web_search" => "WebSearch",
            "web_fetch" => "WebFetch",
            other => other,
        },
        ProviderKind::Claude => match name {
            "str_replace_based_edit_tool" => "Edit",
            "web_search" => "WebSearch",
            "web_fetch" => "WebFetch",
            other => other,
        },
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelTranscriptEntry {
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}

impl From<TranscriptItem> for ModelTranscriptEntry {
    fn from(item: TranscriptItem) -> Self {
        Self {
            item,
            provider_replay: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PromptSections {
    pub stable_prefix: Option<String>,
    pub dynamic_context: Option<String>,
}

impl PromptSections {
    pub fn new(stable_prefix: Option<String>, dynamic_context: Option<String>) -> Self {
        Self {
            stable_prefix: normalize_prompt_section(stable_prefix),
            dynamic_context: normalize_prompt_section(dynamic_context),
        }
    }

    pub fn stable(stable_prefix: impl Into<String>) -> Self {
        Self::new(Some(stable_prefix.into()), None)
    }

    pub fn render_joined(&self) -> Option<String> {
        match (&self.stable_prefix, &self.dynamic_context) {
            (Some(stable), Some(dynamic)) => Some(format!("{stable}\n\n{dynamic}")),
            (Some(stable), None) => Some(stable.clone()),
            (None, Some(dynamic)) => Some(dynamic.clone()),
            (None, None) => None,
        }
    }
}

fn normalize_prompt_section(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    })
}

#[derive(Debug, Clone)]
pub struct ModelResponse {
    pub assistant: AssistantMessage,
    pub provider_replay: Vec<ProviderReplayItem>,
    pub usage: Option<ProviderUsage>,
    pub stop_reason: ModelStopReason,
    pub stop_details: Option<ModelStopDetails>,
}

impl ModelResponse {
    /// Return the terminal refusal message callers should surface instead of
    /// persisting this response as an assistant completion.
    pub fn refusal_error(&self) -> Option<String> {
        (self.stop_reason == ModelStopReason::Refusal).then(|| {
            let Some(details) = self.stop_details.as_ref() else {
                return "provider refused the request".to_string();
            };
            match (&details.category, &details.explanation) {
                (Some(category), Some(explanation)) => {
                    format!("provider refused the request ({category}): {explanation}")
                }
                (Some(category), None) => {
                    format!("provider refused the request ({category})")
                }
                (None, Some(explanation)) => {
                    format!("provider refused the request: {explanation}")
                }
                (None, None) => "provider refused the request".to_string(),
            }
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelStopReason {
    Complete,
    MaxOutputTokens,
    Refusal,
    Compaction,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelStopDetails {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explanation: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderUsage {
    pub input_tokens: Option<usize>,
    pub output_tokens: Option<usize>,
    pub total_tokens: Option<usize>,
    pub cache_read_input_tokens: Option<usize>,
    pub cache_creation_input_tokens: Option<usize>,
    /// Provider-native merged usage fields. This retains provider-specific
    /// accounting such as Anthropic compaction iterations, cache TTL detail,
    /// and thinking-token detail without replacing raw counters with
    /// normalized aggregates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw_provider_usage: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream_request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cf_ray: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub server_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codex_turn_state: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_included: Option<bool>,
}

#[derive(Debug, Error)]
pub enum ProviderError {
    #[error("http error: {0}")]
    Http(#[from] reqwest::Error),
    #[error("provider request timed out: {0}")]
    Timeout(String),
    #[error("transient provider error: {0}")]
    Transient(String),
    #[error("provider returned an error: {0}")]
    Provider(String),
    #[error("provider model catalog error: {message}")]
    ModelCatalog {
        status: Option<u16>,
        message: String,
    },
    #[error("provider returned HTTP {status}: {message}")]
    Status { status: u16, message: String },
    #[error("provider response was incomplete (status: {status}, reason: {reason})")]
    Incomplete { status: String, reason: String },
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("native compaction failed ({kind}): {message}")]
    NativeCompaction {
        kind: NativeCompactionErrorKind,
        message: String,
    },
}

impl ProviderError {
    pub fn native_compaction(kind: NativeCompactionErrorKind, message: impl Into<String>) -> Self {
        Self::NativeCompaction {
            kind,
            message: message.into(),
        }
    }

    pub fn status_code(&self) -> Option<u16> {
        match self {
            ProviderError::Status { status, .. } => Some(*status),
            ProviderError::ModelCatalog { status, .. } => *status,
            ProviderError::Http(error) => error.status().map(|status| status.as_u16()),
            ProviderError::Timeout(_)
            | ProviderError::Transient(_)
            | ProviderError::Provider(_)
            | ProviderError::Incomplete { .. }
            | ProviderError::Json(_)
            | ProviderError::NativeCompaction { .. } => None,
        }
    }

    pub fn retry_diagnostic(&self) -> Option<String> {
        match self {
            ProviderError::Http(error) => {
                let flags = [
                    ("timeout", error.is_timeout()),
                    ("connect", error.is_connect()),
                    ("request", error.is_request()),
                    ("body", error.is_body()),
                    ("decode", error.is_decode()),
                    ("status", error.is_status()),
                ]
                .into_iter()
                .filter_map(|(name, enabled)| enabled.then_some(name))
                .collect::<Vec<_>>();
                let mut parts = Vec::new();
                if !flags.is_empty() {
                    parts.push(format!("reqwest_flags={}", flags.join(",")));
                }
                if let Some(status) = error.status() {
                    parts.push(format!("status={}", status.as_u16()));
                }
                let source_chain = error_source_chain(error);
                if !source_chain.is_empty() {
                    parts.push(format!("sources={}", source_chain.join(" <- ")));
                }
                (!parts.is_empty()).then(|| parts.join("; "))
            }
            ProviderError::Timeout(message) => Some(format!("timeout={message}")),
            ProviderError::Transient(message) => Some(format!("transient={message}")),
            ProviderError::ModelCatalog { status, .. } => status
                .map(|status| format!("model_catalog_status={status}"))
                .or_else(|| Some("model_catalog".to_string())),
            ProviderError::Status { status, .. } => Some(format!("status={status}")),
            ProviderError::Provider(_)
            | ProviderError::Incomplete { .. }
            | ProviderError::Json(_)
            | ProviderError::NativeCompaction { .. } => None,
        }
    }

    pub fn is_context_overflow(&self) -> bool {
        // Only match errors whose message clearly identifies a context-window
        // overflow. A plain 400 is not enough: Anthropic /count_tokens, for
        // example, returns schema-validation 400s for unsupported server tools.
        let status = self.status_code();
        let message = match self {
            ProviderError::Status { message, .. }
            | ProviderError::Transient(message)
            | ProviderError::Provider(message) => message.clone(),
            ProviderError::Http(error) => error.to_string(),
            // Catalog failures happen before request shaping and are never
            // evidence that a generation exceeded its context window.
            ProviderError::ModelCatalog { .. } | ProviderError::Timeout(_) => return false,
            ProviderError::Incomplete { .. }
            | ProviderError::Json(_)
            | ProviderError::NativeCompaction { .. } => return false,
        };
        let lower = message.to_ascii_lowercase();
        if status == Some(413) {
            return true;
        }
        if lower.contains("prompt is too long") {
            return true;
        }
        if lower.contains("context_length_exceeded") {
            return true;
        }
        lower.contains("context")
            && (lower.contains("length")
                || lower.contains("window")
                || lower.contains("too large")
                || lower.contains("exceed")
                || lower.contains("maximum"))
    }
}

fn error_source_chain(error: &(dyn std::error::Error + 'static)) -> Vec<String> {
    let mut chain = Vec::new();
    let mut source = error.source();
    while let Some(error) = source {
        chain.push(error.to_string());
        source = error.source();
    }
    chain
}

pub type ProviderResult<T> = Result<T, ProviderError>;

#[async_trait]
pub trait ModelProvider: Send + Sync {
    async fn complete(&self, request: ModelRequest) -> ProviderResult<ModelResponse>;

    /// Prepare provider-specific ordinary-generation bytes for reuse.
    ///
    /// Custom providers may retain the legacy `complete` lifecycle by using
    /// this default. Built-in providers return an opaque prepared artifact.
    async fn prepare_model_request(
        &self,
        _request: &ModelRequest,
        _existing: Option<&PreparedModelRequest>,
    ) -> ProviderResult<Option<PreparedModelRequest>> {
        Ok(None)
    }

    /// Send a prepared request with the implementation's current attempt
    /// headers and credentials.
    async fn complete_prepared(
        &self,
        request: ModelRequest,
        _prepared: PreparedModelRequest,
    ) -> ProviderResult<ModelResponse> {
        self.complete(request).await
    }

    async fn model_metadata(&self, _model: &str) -> ProviderResult<Option<ProviderModelMetadata>> {
        Ok(None)
    }

    async fn compact(
        &self,
        request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse>;

    async fn count_tokens(
        &self,
        _request: ProviderTokenCountRequest,
    ) -> ProviderResult<ProviderTokenCountResponse> {
        Err(ProviderError::Provider(
            "provider does not support token counting".to_string(),
        ))
    }
}

#[cfg(test)]
mod provider_error_tests {
    use super::*;
    use agent_vocab::{ReplayDisplay, ReplayDisplayKind};

    #[test]
    fn operation_requests_share_one_logical_input_allocation() {
        let input = Arc::new(ProviderModelInput::new(
            "test-model",
            PromptSections::stable("stable prompt"),
            vec![
                TranscriptItem::UserMessage(agent_vocab::UserMessage::text("large transcript"))
                    .into(),
            ],
            ProviderToolProfile::None,
            Vec::new(),
            ReasoningEffort::Medium,
        ));
        let generation = ModelRequest::new(input.clone());
        let count = ProviderTokenCountRequest::new(input.clone());
        let compaction = ProviderCompactionRequest::new(input.clone());
        let sidecar_input = input
            .as_ref()
            .clone()
            .with_reasoning_effort(ReasoningEffort::Low);

        assert!(std::ptr::eq::<ProviderModelInput>(&*generation, &*count));
        assert!(std::ptr::eq::<ProviderModelInput>(
            &*generation,
            &*compaction
        ));
        assert!(Arc::ptr_eq(&input.prompt, &sidecar_input.prompt));
        assert!(Arc::ptr_eq(&input.transcript, &sidecar_input.transcript));
        assert!(Arc::ptr_eq(&input.tools, &sidecar_input.tools));
    }

    #[test]
    fn context_overflow_classifier_matches_known_provider_messages() {
        assert!(ProviderError::Status {
            status: 400,
            message: "invalid_request_error: prompt is too long: 1100000 tokens > 1000000 maximum"
                .to_string(),
        }
        .is_context_overflow());
        assert!(ProviderError::Status {
            status: 413,
            message: "request entity too large".to_string(),
        }
        .is_context_overflow());
        assert!(ProviderError::Status {
            status: 400,
            message: "context_length_exceeded: input is too long".to_string(),
        }
        .is_context_overflow());
        assert!(ProviderError::Provider(
            "Your input exceeds the context window of this model.".to_string(),
        )
        .is_context_overflow());
        assert!(!ProviderError::Status {
            status: 400,
            message:
                "invalid_request_error: Server tools are not supported in the count_tokens endpoint: web_fetch_20260318, web_search_20260318."
                    .to_string(),
        }
        .is_context_overflow());
        assert!(!ProviderError::Status {
            status: 400,
            message: "invalid_request_error: messages: at least one message is required"
                .to_string(),
        }
        .is_context_overflow());
        assert!(!ProviderError::ModelCatalog {
            status: Some(413),
            message: "Codex model has invalid context_window".to_string(),
        }
        .is_context_overflow());
    }

    #[test]
    fn provider_errors_report_retry_diagnostics_without_retry_classification() {
        let error = ProviderError::Timeout("response headers timed out".to_string());

        assert!(!error.is_context_overflow());
        assert_eq!(
            error.retry_diagnostic(),
            Some("timeout=response headers timed out".to_string())
        );

        let error = ProviderError::Transient("server disconnected".to_string());

        assert_eq!(
            error.retry_diagnostic(),
            Some("transient=server disconnected".to_string())
        );

        let error = ProviderError::Transient("context_length_exceeded".to_string());

        assert!(error.is_context_overflow());
        assert_eq!(
            error.retry_diagnostic(),
            Some("transient=context_length_exceeded".to_string())
        );

        let error = ProviderError::Status {
            status: 401,
            message: "unauthorized".to_string(),
        };

        assert_eq!(error.retry_diagnostic(), Some("status=401".to_string()));
    }

    #[test]
    fn provider_replay_filter_parses_raw_values_without_rewriting() {
        let openai = ProviderReplayItem {
            provider: ProviderKind::OpenAi,
            raw_json: r#"{"type":"function_call","name":"web_search"}"#.to_string(),
            display: Some(ReplayDisplay {
                kind: ReplayDisplayKind::HostedTool,
                pretty_name: "WebSearch".to_string(),
                input_summary: None,
            }),
        };
        let corrupt_claude = ProviderReplayItem {
            provider: ProviderKind::Claude,
            raw_json: "{".to_string(),
            display: None,
        };
        let entry = ModelTranscriptEntry {
            item: TranscriptItem::AssistantMessage(AssistantMessage { items: Vec::new() }),
            provider_replay: vec![openai.clone(), corrupt_claude.clone()],
        };

        assert_eq!(
            entry
                .provider_replay_values_for(ProviderKind::OpenAi)
                .unwrap(),
            vec![openai.raw_value().unwrap()]
        );
        assert!(entry
            .provider_replay_values_for(ProviderKind::Claude)
            .is_err());
        assert_eq!(entry.provider_replay, vec![openai, corrupt_claude]);
    }
}
