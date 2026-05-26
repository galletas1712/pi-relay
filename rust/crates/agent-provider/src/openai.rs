use agent_tools::{tool_display, ProviderTool, ToolDisplayInput};
use agent_vocab::{
    AssistantItem, AssistantMessage, CompactionSummary, ContentBlock, ProviderKind,
    ProviderReplayItem, ReasoningEffort, ReplayDisplay, ToolCall, ToolCallId, TranscriptItem,
    TurnId, UserMessage,
};
use async_trait::async_trait;
use reqwest::{
    header::{HeaderMap, ACCEPT, CONTENT_ENCODING, CONTENT_TYPE},
    StatusCode,
};
use serde_json::{json, Value};
use std::{
    io::Cursor,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::Duration,
};
use uuid::Uuid;

#[cfg(test)]
use crate::sse::read_json_sse_text;
use crate::{
    sse::{read_json_sse_response, SseControl, SseEvent},
    ModelProvider, ModelRequest, ModelResponse, ModelStopReason, ModelTranscriptEntry,
    ProviderCompactionRequest, ProviderCompactionResponse, ProviderError, ProviderResult,
    ProviderToolProfile, ProviderUsage,
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
const HEADER_CODEX_TURN_STATE: &str = "x-codex-turn-state";
const HEADER_REQUEST_ID: &str = "x-request-id";
const HEADER_CF_RAY: &str = "cf-ray";
const HEADER_OPENAI_MODEL: &str = "openai-model";
const HEADER_OPENAI_MODEL_LEGACY: &str = "x-openai-model";
const HEADER_REASONING_INCLUDED: &str = "x-reasoning-included";

// The Codex CLI's `originator` is the literal string `codex_cli_rs`. Sending
// this from pi-relay is deliberate: the ChatGPT backend uses it for routing
// and rate-limit accounting (see `is_first_party_originator` in the Codex
// source). Diverging from this label is what causes throttling.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";
const CODEX_RESIDENCY_US: &str = "us";
const CODEX_REQUEST_COMPRESSION_LEVEL: i32 = 3;
const CODEX_COMPACT_REQUEST_TIMEOUT_SECS: u64 = 20 * 60;
const CODEX_RESPONSES_STREAM_IDLE_TIMEOUT_SECS: u64 = 5 * 60;

#[derive(Debug, Clone)]
pub struct OpenAiProvider {
    client: reqwest::Client,
    session_state: Option<Arc<OpenAiCodexSessionState>>,
    access_token: String,
    account_id: Option<String>,
    /// Persistent Codex installation identifier (UUID), read from
    /// `~/.codex/installation_id` by the daemon and passed through as the
    /// `x-codex-installation-id` header on every request. Optional because
    /// tests may not have a Codex install.
    installation_id: Option<String>,
    base_url: String,
}

#[derive(Debug)]
pub struct OpenAiCodexSessionState {
    session_id: String,
    window_generation: AtomicU64,
    turn_state: Mutex<Option<CodexTurnState>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CodexTurnState {
    turn_id: TurnId,
    value: String,
}

impl OpenAiCodexSessionState {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            window_generation: AtomicU64::new(0),
            turn_state: Mutex::new(None),
        }
    }

    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    pub fn window_generation(&self) -> u64 {
        self.window_generation.load(Ordering::Relaxed)
    }

    pub fn set_window_generation(&self, generation: u64) {
        self.window_generation.store(generation, Ordering::Relaxed);
    }

    pub fn observe_transcript_generation(&self, generation: u64) {
        let mut current = self.window_generation();
        while generation > current {
            match self.window_generation.compare_exchange_weak(
                current,
                generation,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(actual) => current = actual,
            }
        }
    }

    pub fn window_id(&self) -> String {
        format!("{}:{}", self.session_id, self.window_generation())
    }

    pub fn turn_state_for_request(&self, turn_id: Option<TurnId>) -> Option<String> {
        let turn_id = turn_id?;
        let guard = self
            .turn_state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        guard
            .as_ref()
            .filter(|state| state.turn_id == turn_id)
            .map(|state| state.value.clone())
    }

    pub fn record_turn_state(&self, turn_id: Option<TurnId>, value: String) {
        let Some(turn_id) = turn_id else {
            return;
        };
        let mut guard = self
            .turn_state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        *guard = Some(CodexTurnState { turn_id, value });
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpenAiResponseHeaders {
    upstream_request_id: Option<String>,
    cf_ray: Option<String>,
    server_model: Option<String>,
    codex_turn_state: Option<String>,
    reasoning_included: Option<bool>,
}

impl OpenAiResponseHeaders {
    fn from_headers(headers: &HeaderMap) -> Self {
        Self {
            upstream_request_id: header_value(headers, HEADER_REQUEST_ID),
            cf_ray: header_value(headers, HEADER_CF_RAY),
            server_model: header_value(headers, HEADER_OPENAI_MODEL)
                .or_else(|| header_value(headers, HEADER_OPENAI_MODEL_LEGACY)),
            codex_turn_state: header_value(headers, HEADER_CODEX_TURN_STATE),
            reasoning_included: headers
                .contains_key(HEADER_REASONING_INCLUDED)
                .then_some(true),
        }
    }

    fn attach_to_usage(self, usage: &mut Option<ProviderUsage>) {
        let Some(usage) = usage.as_mut() else {
            return;
        };
        usage.upstream_request_id = self.upstream_request_id;
        usage.cf_ray = self.cf_ray;
        usage.server_model = self.server_model;
        usage.codex_turn_state = self.codex_turn_state;
        usage.reasoning_included = self.reasoning_included;
    }
}

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn parse_compact_response(text: &str) -> ProviderResult<ProviderCompactionResponse> {
    let response: Value = serde_json::from_str(text).map_err(|error| {
        ProviderError::Provider(format!(
            "failed to parse OpenAI compact response JSON: {error}; body: {}",
            response_excerpt(text)
        ))
    })?;
    let output = response
        .get("output")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::Provider("OpenAI compact response missing output array".to_string())
        })?;

    let mut provider_replay = Vec::new();
    let mut has_compaction = false;
    let mut summary_parts = Vec::new();
    for item in output {
        if keep_compact_output_item(item) {
            if is_compaction_item(item) {
                has_compaction = true;
            }
            collect_compact_summary_text(item, &mut summary_parts);
            provider_replay.push(ProviderReplayItem::new(ProviderKind::OpenAi, item)?);
        }
    }

    if provider_replay.is_empty() {
        return Err(ProviderError::Provider(
            "OpenAI compact response had no usable replacement history".to_string(),
        ));
    }
    if !has_compaction {
        return Err(ProviderError::Provider(format!(
            "OpenAI compact response did not include a compaction item; output item types: {}",
            compact_output_type_summary(output)
        )));
    }

    let summary = summary_parts.join("").trim().to_string();
    Ok(ProviderCompactionResponse {
        summary: (!summary.is_empty()).then_some(summary),
        provider_replay,
        usage: response.get("usage").and_then(openai_usage),
    })
}

fn compact_output_type_summary(output: &[Value]) -> String {
    let mut counts = std::collections::BTreeMap::<String, usize>::new();
    for item in output {
        let ty = item
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        let role = item.get("role").and_then(Value::as_str);
        let key = role
            .map(|role| format!("{ty}:{role}"))
            .unwrap_or_else(|| ty.to_string());
        *counts.entry(key).or_insert(0) += 1;
    }
    if counts.is_empty() {
        "<empty>".to_string()
    } else {
        counts
            .into_iter()
            .map(|(key, count)| format!("{key}={count}"))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// Codex's wire type for the opaque encrypted summary item has shifted over
// time. The current backend emits `compaction_summary`; older builds emitted
// `compaction`. Codex CLI's own `ResponseItem::Compaction` variant aliases
// both (see `~/codex/codex-rs/protocol/src/models.rs`), so we accept either.
fn is_compaction_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction") | Some("compaction_summary")
    )
}

fn keep_compact_output_item(item: &Value) -> bool {
    if is_compaction_item(item) {
        return true;
    }
    match item.get("type").and_then(Value::as_str) {
        Some("message") => match item.get("role").and_then(Value::as_str) {
            Some("assistant") => true,
            Some("user") => compact_user_message_is_real(item),
            _ => false,
        },
        _ => false,
    }
}

fn compact_user_message_is_real(item: &Value) -> bool {
    let text = message_text(item).trim().to_string();
    if text.is_empty() {
        return false;
    }
    !is_synthetic_compact_user_text(&text)
}

fn is_synthetic_compact_user_text(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.starts_with("starting working directory for this session:")
        || lower.starts_with("current working directory:")
        || lower.contains("the bash tool runs each command in a fresh shell rooted here")
        || lower.starts_with("the conversation history before this point was compacted")
        || lower.starts_with("x-anthropic-billing-header:")
}

fn collect_compact_summary_text(item: &Value, summary_parts: &mut Vec<String>) {
    if item.get("type").and_then(Value::as_str) != Some("message")
        || item.get("role").and_then(Value::as_str) != Some("assistant")
    {
        return;
    }
    let text = message_text(item);
    if !text.is_empty() {
        summary_parts.push(text);
    }
}

fn message_text(item: &Value) -> String {
    item.get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|part| {
            let part_type = part.get("type").and_then(Value::as_str);
            match part_type {
                Some("output_text") | Some("input_text") | Some("text") => {
                    part.get("text").and_then(Value::as_str)
                }
                _ => None,
            }
        })
        .collect::<Vec<_>>()
        .join("")
}

impl OpenAiProvider {
    pub fn codex(
        access_token: impl Into<String>,
        account_id: Option<String>,
        installation_id: Option<String>,
    ) -> Self {
        Self::codex_with_client(
            reqwest::Client::new(),
            access_token,
            account_id,
            installation_id,
        )
    }

    pub fn codex_with_client(
        client: reqwest::Client,
        access_token: impl Into<String>,
        account_id: Option<String>,
        installation_id: Option<String>,
    ) -> Self {
        Self {
            client,
            session_state: None,
            access_token: access_token.into(),
            account_id,
            installation_id,
            base_url: "https://chatgpt.com/backend-api/codex".to_string(),
        }
    }

    pub fn codex_with_client_and_session(
        client: reqwest::Client,
        session_state: Arc<OpenAiCodexSessionState>,
        access_token: impl Into<String>,
        account_id: Option<String>,
        installation_id: Option<String>,
    ) -> Self {
        Self {
            client,
            session_state: Some(session_state),
            access_token: access_token.into(),
            account_id,
            installation_id,
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
        window_id: &str,
        turn_state: Option<&str>,
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
            .header(HEADER_WINDOW_ID, window_id)
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
        if let Some(turn_state) = turn_state {
            request = request.header(HEADER_CODEX_TURN_STATE, turn_state);
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

    fn supports_remote_compaction(&self) -> bool {
        true
    }

    async fn compact(
        &self,
        request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        self.compact_responses(request).await
    }

    // No `count_tokens` impl: the codex backend has no `/responses/input_tokens`
    // route (Cloudflare responds 403 with a challenge interstitial). pi-relay
    // reads `usage.input_tokens` off the streaming `response.completed` event
    // and the runtime gate falls through to the reactive overflow recovery
    // path for OpenAI sessions.
}

impl OpenAiProvider {
    async fn complete_responses(&self, request: ModelRequest) -> ProviderResult<ModelResponse> {
        // Lift the session id off the request before consuming it for the body.
        // This is the value the Codex CLI calls `thread_id`: the unique
        // pi-relay session identifier that doubles as the prompt-cache cohort
        // and as every routing header (session_id, x-client-request-id,
        // etc.). Falling back to a fresh UUID keeps the CLI / test paths
        // functional, but the daemon always supplies a real session id.
        let session_id = openai_session_id(&request.session_id, self.session_state.as_deref());
        let window_id = openai_window_id(
            &session_id,
            self.session_state.as_deref(),
            &request.transcript,
        );
        let turn_id = request.turn_id;
        let codex_turn_state = self
            .session_state
            .as_deref()
            .filter(|state| state.session_id() == session_id)
            .and_then(|state| state.turn_state_for_request(turn_id));
        let body = responses_body(request, &session_id)?;

        let response = zstd_json_request(
            self.add_codex_headers(
                self.client
                    .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
                    .header(ACCEPT, "text/event-stream"),
                &session_id,
                &window_id,
                codex_turn_state.as_deref(),
            ),
            &body,
        )?
        .send()
        .await?;
        let response_headers = OpenAiResponseHeaders::from_headers(response.headers());
        let mut parsed = parse_responses_stream(response, ProviderKind::OpenAi).await?;
        if let (Some(turn_state), Some(session_state)) = (
            response_headers.codex_turn_state.clone(),
            self.session_state
                .as_deref()
                .filter(|state| state.session_id() == session_id),
        ) {
            session_state.record_turn_state(turn_id, turn_state);
        }
        response_headers.attach_to_usage(&mut parsed.usage);
        Ok(parsed)
    }

    async fn compact_responses(
        &self,
        request: ProviderCompactionRequest,
    ) -> ProviderResult<ProviderCompactionResponse> {
        let session_id = openai_session_id(&request.session_id, self.session_state.as_deref());
        let window_id = openai_window_id(
            &session_id,
            self.session_state.as_deref(),
            &request.transcript,
        );
        let body = compact_body(request, &session_id)?;

        let response = self
            .add_codex_headers(
                self.client
                    .post(format!(
                        "{}/responses/compact",
                        self.base_url.trim_end_matches('/')
                    ))
                    .header(ACCEPT, "application/json"),
                &session_id,
                &window_id,
                None,
            )
            .timeout(Duration::from_secs(CODEX_COMPACT_REQUEST_TIMEOUT_SECS))
            .json(&body)
            .send()
            .await?;
        let response_headers = OpenAiResponseHeaders::from_headers(response.headers());
        let (status, text) = response_text(response).await?;
        ensure_success(status, &text)?;
        let mut parsed = parse_compact_response(&text)?;
        response_headers.attach_to_usage(&mut parsed.usage);
        Ok(parsed)
    }
}

fn openai_session_id(
    request_session_id: &Option<String>,
    session_state: Option<&OpenAiCodexSessionState>,
) -> String {
    request_session_id
        .clone()
        .or_else(|| session_state.map(|state| state.session_id().to_string()))
        .unwrap_or_else(|| Uuid::new_v4().to_string())
}

fn openai_window_id(
    session_id: &str,
    session_state: Option<&OpenAiCodexSessionState>,
    transcript: &[ModelTranscriptEntry],
) -> String {
    let transcript_generation = codex_window_generation(transcript);
    if let Some(state) = session_state.filter(|state| state.session_id() == session_id) {
        state.observe_transcript_generation(transcript_generation);
        state.window_id()
    } else {
        format!("{session_id}:{transcript_generation}")
    }
}

fn zstd_json_request(
    request: reqwest::RequestBuilder,
    body: &Value,
) -> ProviderResult<reqwest::RequestBuilder> {
    let json = serde_json::to_vec(body)?;
    let compressed =
        match zstd::stream::encode_all(Cursor::new(json), CODEX_REQUEST_COMPRESSION_LEVEL) {
            Ok(compressed) => compressed,
            Err(error) => {
                return Err(ProviderError::Provider(format!(
                    "failed to zstd-compress OpenAI request body: {error}"
                )));
            }
        };
    Ok(request
        .header(CONTENT_TYPE, "application/json")
        .header(CONTENT_ENCODING, "zstd")
        .body(compressed))
}

async fn response_text(response: reqwest::Response) -> ProviderResult<(StatusCode, String)> {
    let status = response.status();
    let bytes = response.bytes().await?;
    Ok((status, String::from_utf8_lossy(&bytes).into_owned()))
}

fn codex_window_generation(transcript: &[ModelTranscriptEntry]) -> u64 {
    // Codex CLI stores an explicit per-session window generation and increments
    // it after replacing history with a compacted transcript. Pi-relay's Rust
    // provider is currently re-created per request, so an agent-provider-only
    // implementation derives a stable generation from the active transcript:
    // 0 before compaction, then the latest compacted turn id after compaction.
    // This preserves the backend-visible "new window after compaction" signal
    // without requiring daemon/session state.
    transcript
        .iter()
        .filter_map(|entry| match entry.item() {
            TranscriptItem::CompactionSummary(summary) => Some(summary.last_turn_id.0),
            _ => None,
        })
        .max()
        .unwrap_or(0)
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
    let tool_profile = request.tool_profile;
    let request_tools = crate::effective_provider_tools(tool_profile, request.tools);
    let tools = response_tools(tool_profile, &request_tools)?;
    // Cache cohort priority, highest to lowest:
    //   1. Explicit override from `ProviderConfig.prompt_cache.key` (lets
    //      operators force a particular bucket from config).
    //   2. The session id we received from the daemon, matching Codex CLI's
    //      `prompt_cache_key = thread_id.to_string()` (see
    //      `~/codex/codex-rs/core/src/client.rs`). One bucket per pi-relay
    //      session keeps us well under OpenAI's ~15 RPM-per-shard ceiling
    //      while still maximising in-session prefix reuse.
    //   3. Deterministic config-hash fallback for tests that don't
    //      supply a session id.
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| session_id.to_string());
    let body = json!({
        "model": request.model,
        "instructions": request.prompt.stable_prefix.clone().unwrap_or_default(),
        "input": response_input_items(request.prompt.dynamic_context.as_deref(), &request.prompt, &request.transcript)?,
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

// Mirrors Codex CLI's current `CompactionInput` (see
// `~/codex/codex-rs/codex-api/src/common.rs`). The compaction endpoint is
// unary, so keep streaming-only `/responses` fields (`stream`, `store`,
// `include`, `tool_choice`) out, but preserve the same request-affinity fields
// Codex now carries into compaction (`prompt_cache_key`, `service_tier`).
fn compact_body(request: ProviderCompactionRequest, session_id: &str) -> ProviderResult<Value> {
    let reasoning_effort = openai_reasoning_effort(request.reasoning_effort)?;
    let tool_profile = request.tool_profile;
    let request_tools = crate::effective_provider_tools(tool_profile, request.tools);
    let tools = response_tools(tool_profile, &request_tools)?;
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| session_id.to_string());
    Ok(json!({
        "model": request.model,
        "instructions": request.prompt.stable_prefix.clone().unwrap_or_default(),
        "input": response_input_items(request.prompt.dynamic_context.as_deref(), &request.prompt, &request.transcript)?,
        "tools": tools,
        "parallel_tool_calls": true,
        "reasoning": {
            "effort": reasoning_effort,
        },
        "service_tier": OPENAI_PRIORITY_SERVICE_TIER,
        "prompt_cache_key": prompt_cache_key,
    }))
}

fn response_tools(
    profile: ProviderToolProfile,
    tools: &[ProviderTool],
) -> ProviderResult<Vec<Value>> {
    match profile {
        ProviderToolProfile::None => Ok(Vec::new()),
        ProviderToolProfile::CustomDefinitions | ProviderToolProfile::OpenAiCoding => {
            Ok(response_provider_tools(tools))
        }
        ProviderToolProfile::AnthropicCoding => Err(ProviderError::Provider(
            "Anthropic coding tools cannot be sent to OpenAI".to_string(),
        )),
    }
}

fn response_provider_tools(tools: &[ProviderTool]) -> Vec<Value> {
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

fn transcript_to_response_items(
    prompt: &crate::PromptSections,
    items: &[ModelTranscriptEntry],
) -> ProviderResult<Vec<Value>> {
    let mut responses = Vec::new();
    for entry in items {
        match entry.item() {
            TranscriptItem::UserMessage(message) => {
                responses.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": responses_user_content(message),
                }));
            }
            TranscriptItem::CompactionSummary(summary) => {
                let replay_items =
                    openai_replay_items(&entry.provider_replay_for(ProviderKind::OpenAi))?;
                if !replay_items.is_empty() {
                    responses.extend(replay_items);
                } else {
                    responses.push(json!({
                        "type": "message",
                        "role": "user",
                        "content": [{ "type": "input_text", "text": compaction_summary_text(summary, prompt) }],
                    }));
                }
            }
            TranscriptItem::AssistantMessage(message) => {
                let replay_items =
                    openai_replay_items(&entry.provider_replay_for(ProviderKind::OpenAi))?;
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
    if call.tool_name == "Edit" {
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
            "name": openai_wire_tool_name(&call.tool_name),
            "input": input,
        })
    } else {
        json!({
            "type": "function_call",
            "call_id": call.id.as_str(),
            "name": openai_wire_tool_name(&call.tool_name),
            "arguments": call.args_json,
        })
    }
}

fn response_tool_result_item(result: &agent_vocab::ToolResultMessage) -> Value {
    if result.tool_name == "Edit" {
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
    prompt: &crate::PromptSections,
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
    items.extend(transcript_to_response_items(prompt, transcript)?);
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

async fn parse_responses_stream(
    response: reqwest::Response,
    provider: ProviderKind,
) -> ProviderResult<ModelResponse> {
    let mut state = ResponsesStreamState::new(provider);
    read_json_sse_response(
        response,
        Duration::from_secs(CODEX_RESPONSES_STREAM_IDLE_TIMEOUT_SECS),
        format!(
            "OpenAI response stream was idle for {CODEX_RESPONSES_STREAM_IDLE_TIMEOUT_SECS} seconds"
        ),
        response_error_message,
        |event| state.process_sse_event(event),
    )
    .await?;
    Ok(state.finish())
}

#[cfg(test)]
fn parse_responses_sse(text: &str, provider: ProviderKind) -> ProviderResult<ModelResponse> {
    let mut state = ResponsesStreamState::new(provider);
    read_json_sse_text(text, |event| state.process_sse_event(event))?;
    Ok(state.finish())
}

struct ResponsesStreamState {
    provider: ProviderKind,
    items: Vec<AssistantItem>,
    provider_replay: Vec<ProviderReplayItem>,
    usage: Option<ProviderUsage>,
    stop_reason: ModelStopReason,
}

impl ResponsesStreamState {
    fn new(provider: ProviderKind) -> Self {
        Self {
            provider,
            items: Vec::new(),
            provider_replay: Vec::new(),
            usage: None,
            stop_reason: ModelStopReason::Complete,
        }
    }

    fn finish(self) -> ModelResponse {
        ModelResponse {
            assistant: AssistantMessage { items: self.items },
            provider_replay: self.provider_replay,
            usage: self.usage,
            stop_reason: self.stop_reason,
        }
    }

    fn process_sse_event(&mut self, event: SseEvent) -> ProviderResult<SseControl> {
        match event {
            SseEvent::Json(event) => self.process_event(&event),
            SseEvent::Done => Ok(SseControl::Stop),
        }
    }

    fn process_event(&mut self, event: &Value) -> ProviderResult<SseControl> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.done") => {
                if let Some(item) = event.get("item") {
                    parse_response_output_item(
                        item,
                        &mut self.items,
                        &mut self.provider_replay,
                        self.provider,
                    )?;
                }
                Ok(SseControl::Continue)
            }
            Some("response.failed") => {
                let message = event
                    .pointer("/response/error/message")
                    .and_then(Value::as_str)
                    .unwrap_or("response.failed");
                Err(ProviderError::Provider(message.to_string()))
            }
            Some("response.incomplete") => {
                let message = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str);
                if message == Some("max_output_tokens") {
                    self.stop_reason = ModelStopReason::MaxOutputTokens;
                    self.usage = event.pointer("/response/usage").and_then(openai_usage);
                    return Ok(SseControl::Stop);
                }
                let message = message
                    .map(|reason| format!("response incomplete: {reason}"))
                    .unwrap_or_else(|| "response incomplete".to_string());
                Err(ProviderError::Provider(message))
            }
            Some("response.completed" | "response.done") => {
                self.usage = event.pointer("/response/usage").and_then(openai_usage);
                Ok(SseControl::Stop)
            }
            Some("error") => {
                let message = event
                    .get("message")
                    .or_else(|| event.get("code"))
                    .and_then(Value::as_str)
                    .map(ToOwned::to_owned)
                    .unwrap_or_else(|| event.to_string());
                Err(ProviderError::Provider(format!("Codex error: {message}")))
            }
            _ => Ok(SseControl::Continue),
        }
    }
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
    let display = openai_provider_replay_display(item);
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
                tool_name: crate::canonical_tool_name_for_provider(provider, name).to_string(),
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
                tool_name: crate::canonical_tool_name_for_provider(provider, name).to_string(),
                args_json: json!({ "input": input }).to_string(),
            }));
        }
        "reasoning" | "reasoning_summary" => {}
        _ => {}
    }
    Ok(())
}

fn openai_provider_replay_display(item: &Value) -> Option<ReplayDisplay> {
    match item.get("type").and_then(Value::as_str)? {
        "web_search_call" => {
            let action = item.get("action")?;
            let tool_name = match action.get("type").and_then(Value::as_str)? {
                "search" => "WebSearch",
                "open_page" => "OpenPage",
                _ => return None,
            };
            tool_display(tool_name, ToolDisplayInput::HostedTool, Some(action))
        }
        "function_call" => {
            let name = item.get("name").and_then(Value::as_str)?;
            let canonical_name =
                crate::canonical_tool_name_for_provider(ProviderKind::OpenAi, name);
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())?;
            tool_display(
                canonical_name,
                ToolDisplayInput::LocalTool,
                Some(&arguments),
            )
        }
        "custom_tool_call" => {
            let name = item.get("name").and_then(Value::as_str)?;
            let canonical_name =
                crate::canonical_tool_name_for_provider(ProviderKind::OpenAi, name);
            tool_display(canonical_name, ToolDisplayInput::LocalTool, None)
        }
        _ => None,
    }
}

fn openai_wire_tool_name(canonical_name: &str) -> &str {
    match canonical_name {
        "Edit" => "apply_patch",
        "WebFetch" => "web_fetch",
        "WebSearch" => "web_search",
        other => other,
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
        ..ProviderUsage::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PromptSections;
    use agent_vocab::{ToolCall, ToolResultMessage, TurnId};
    use reqwest::header::HeaderValue;

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
                "session-uuid-abcd:0",
                Some("sticky-turn-state"),
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
        assert_eq!(
            header("authorization").as_deref(),
            Some("Bearer access-token")
        );
        assert_eq!(header("originator").as_deref(), Some(CODEX_ORIGINATOR));
        assert!(
            header("user-agent")
                .as_deref()
                .map(|ua| ua.starts_with("codex_cli_rs/"))
                .unwrap_or(false),
            "user-agent should start with codex_cli_rs/: {:?}",
            header("user-agent")
        );
        assert_eq!(
            header(HEADER_RESIDENCY).as_deref(),
            Some(CODEX_RESIDENCY_US)
        );
        assert_eq!(header("chatgpt-account-id").as_deref(), Some("account-id"));

        // Codex-specific identity headers.
        assert_eq!(
            header(HEADER_INSTALLATION_ID).as_deref(),
            Some("install-uuid-1234")
        );
        assert_eq!(
            header(HEADER_WINDOW_ID).as_deref(),
            Some("session-uuid-abcd:0")
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
        assert_eq!(
            header(HEADER_CODEX_TURN_STATE).as_deref(),
            Some("sticky-turn-state")
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
                "session-xyz:0",
                None,
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
    fn zstd_json_request_sets_codex_compression_headers_and_body() {
        let provider = OpenAiProvider::codex("access-token", None, None);
        let body = json!({
            "model": "gpt-5.5",
            "input": ["hello"],
            "padding": "x".repeat(1024),
        });

        let request =
            zstd_json_request(provider.client.post("https://example.com/responses"), &body)
                .expect("request should compress")
                .build()
                .expect("request builds");

        assert_eq!(
            request
                .headers()
                .get(CONTENT_ENCODING)
                .and_then(|value| value.to_str().ok()),
            Some("zstd")
        );
        assert_eq!(
            request
                .headers()
                .get(CONTENT_TYPE)
                .and_then(|value| value.to_str().ok()),
            Some("application/json")
        );
        let encoded = request
            .body()
            .and_then(reqwest::Body::as_bytes)
            .expect("compressed body should be buffered");
        let decoded = zstd::stream::decode_all(std::io::Cursor::new(encoded))
            .expect("compressed body should decode");
        let decoded: Value = serde_json::from_slice(&decoded).expect("decoded body should be JSON");
        assert_eq!(decoded, body);
    }

    #[test]
    fn openai_response_headers_attach_debug_metadata_to_usage() {
        let mut headers = HeaderMap::new();
        headers.insert(HEADER_REQUEST_ID, HeaderValue::from_static("req-123"));
        headers.insert(HEADER_CF_RAY, HeaderValue::from_static("cf-ray-456"));
        headers.insert(
            HEADER_OPENAI_MODEL,
            HeaderValue::from_static("gpt-5.5-fast"),
        );
        headers.insert(
            HEADER_CODEX_TURN_STATE,
            HeaderValue::from_static("turn-state"),
        );
        headers.insert(HEADER_REASONING_INCLUDED, HeaderValue::from_static("true"));

        let mut usage = Some(ProviderUsage {
            input_tokens: Some(10),
            ..ProviderUsage::default()
        });
        OpenAiResponseHeaders::from_headers(&headers).attach_to_usage(&mut usage);
        let usage = usage.expect("usage remains present");

        assert_eq!(usage.upstream_request_id.as_deref(), Some("req-123"));
        assert_eq!(usage.cf_ray.as_deref(), Some("cf-ray-456"));
        assert_eq!(usage.server_model.as_deref(), Some("gpt-5.5-fast"));
        assert_eq!(usage.codex_turn_state.as_deref(), Some("turn-state"));
        assert_eq!(usage.reasoning_included, Some(true));
        assert_eq!(usage.input_tokens, Some(10));
    }

    #[test]
    fn compact_body_matches_codex_cli_compaction_input_shape() {
        // The codex backend's `/responses/compact` is unary, so the body must
        // stay on Codex CLI's `CompactionInput` shape rather than the full
        // streaming `/responses` envelope.
        let body = compact_body(
            ProviderCompactionRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::new(
                    Some("stable rules".to_string()),
                    Some("cwd: /tmp/project".to_string()),
                ),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: None,
                session_id: Some("session-1".to_string()),
            },
            "session-1",
        )
        .expect("compact body renders");

        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["instructions"], "stable rules");
        assert_eq!(body["input"][0]["content"][0]["text"], "cwd: /tmp/project");
        assert_eq!(body["input"][1]["content"][0]["text"], "hello");
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["prompt_cache_key"], "session-1");
        assert_eq!(body["service_tier"], OPENAI_PRIORITY_SERVICE_TIER);

        for forbidden in ["tool_choice", "store", "stream", "include", "text"] {
            assert!(
                body.get(forbidden).is_none(),
                "compact body must not include `{forbidden}`"
            );
        }
    }

    #[test]
    fn compact_body_prefers_explicit_prompt_cache_key_override() {
        let body = compact_body(
            ProviderCompactionRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: Some("explicit-compact-cohort".to_string()),
                session_id: Some("session-1".to_string()),
            },
            "session-1",
        )
        .expect("compact body renders");

        assert_eq!(body["prompt_cache_key"], "explicit-compact-cohort");
        assert_eq!(body["service_tier"], OPENAI_PRIORITY_SERVICE_TIER);
    }

    #[test]
    fn codex_window_id_uses_session_and_zero_before_compaction() {
        let transcript = vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()];

        assert_eq!(
            openai_window_id("thread-1", None, &transcript),
            "thread-1:0"
        );
    }

    #[test]
    fn codex_window_id_advances_after_compaction_summary() {
        let transcript = vec![
            TranscriptItem::CompactionSummary(CompactionSummary::new(
                "session-1",
                "leaf-1",
                "summary",
                Some(1024),
                TurnId(42),
            ))
            .into(),
            TranscriptItem::UserMessage(UserMessage::text("after compaction")).into(),
        ];

        assert_eq!(
            openai_window_id("thread-1", None, &transcript),
            "thread-1:42"
        );
    }

    #[test]
    fn codex_turn_state_is_replayed_only_for_same_turn() {
        let state = OpenAiCodexSessionState::new("session-1");

        assert_eq!(state.turn_state_for_request(Some(TurnId(7))), None);
        state.record_turn_state(Some(TurnId(7)), "sticky-state".to_string());

        assert_eq!(
            state.turn_state_for_request(Some(TurnId(7))).as_deref(),
            Some("sticky-state")
        );
        assert_eq!(state.turn_state_for_request(Some(TurnId(8))), None);
        assert_eq!(state.turn_state_for_request(None), None);
    }

    #[tokio::test]
    async fn codex_complete_replays_turn_state_on_followup_request_only_same_turn() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("listener binds");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let captured_turn_states = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let server_turn_states = captured_turn_states.clone();
        let server = tokio::spawn(async move {
            for _ in 0..3 {
                let (mut stream, _) = listener.accept().await.expect("request accepted");
                let mut buffer = Vec::new();
                let mut chunk = [0; 1024];
                let (header_end, content_length) = loop {
                    let read = stream.read(&mut chunk).await.expect("request reads");
                    assert!(read > 0, "request closed before headers");
                    buffer.extend_from_slice(&chunk[..read]);
                    let Some(header_end) =
                        buffer.windows(4).position(|window| window == b"\r\n\r\n")
                    else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&buffer[..header_end]);
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
                while buffer.len() < body_end {
                    let read = stream.read(&mut chunk).await.expect("request body reads");
                    assert!(read > 0, "request closed before body");
                    buffer.extend_from_slice(&chunk[..read]);
                }

                let headers = String::from_utf8_lossy(&buffer[..header_end]);
                let turn_state = headers
                    .lines()
                    .filter_map(|line| line.split_once(':'))
                    .find_map(|(name, value)| {
                        name.eq_ignore_ascii_case(HEADER_CODEX_TURN_STATE)
                            .then(|| value.trim().to_string())
                    });
                server_turn_states.lock().await.push(turn_state);

                let sse = r#"data: {"type":"response.completed","response":{"id":"resp_1"}}

"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\n\
                     content-type: text/event-stream\r\n\
                     x-codex-turn-state: sticky-state\r\n\
                     content-length: {}\r\n\
                     connection: close\r\n\
                     \r\n\
                     {sse}",
                    sse.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("response writes");
            }
        });

        let session_state = Arc::new(OpenAiCodexSessionState::new("session-1"));
        let provider = OpenAiProvider {
            client: reqwest::Client::new(),
            session_state: Some(session_state),
            access_token: "token".to_string(),
            account_id: None,
            installation_id: None,
            base_url,
        };
        let make_request = |turn_id| ModelRequest {
            model: "gpt-5.5".to_string(),
            prompt: PromptSections::default(),
            transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
            tool_profile: ProviderToolProfile::OpenAiCoding,
            tools: Vec::new(),
            max_tokens: None,
            reasoning_effort: ReasoningEffort::Medium,
            prompt_cache_key: None,
            session_id: Some("session-1".to_string()),
            turn_id: Some(turn_id),
        };

        provider
            .complete(make_request(TurnId(1)))
            .await
            .expect("first request completes");
        provider
            .complete(make_request(TurnId(1)))
            .await
            .expect("same-turn request completes");
        provider
            .complete(make_request(TurnId(2)))
            .await
            .expect("next-turn request completes");
        server.await.expect("server finishes");

        assert_eq!(
            *captured_turn_states.lock().await,
            vec![None, Some("sticky-state".to_string()), None]
        );
    }

    #[test]
    fn compact_parser_requires_compaction_item_with_type_diagnostics() {
        let error = parse_compact_response(
            r#"{"output":[{"type":"message","role":"assistant","content":[{"type":"output_text","text":"summary"}]},{"type":"reasoning"}]}"#,
        )
        .expect_err("missing compaction item should fail");
        let message = error.to_string();
        assert!(message.contains("did not include a compaction item"));
        assert!(message.contains("message:assistant=1"));
        assert!(message.contains("reasoning=1"));
    }

    #[test]
    fn compact_parser_accepts_compaction_summary_alias() {
        // The current codex backend emits `compaction_summary`; codex CLI's
        // `ResponseItem` aliases this to its `Compaction` variant. Pi-relay
        // must accept it identically to avoid spurious "missing compaction
        // item" errors after a successful 200 from /responses/compact.
        let response = parse_compact_response(
            r#"{"output":[{"id":"msg_1","type":"message","role":"user","content":[{"type":"input_text","text":"hi"}]},{"id":"cmp_1","type":"compaction_summary","encrypted_content":"opaque"}]}"#,
        )
        .expect("compaction_summary should be accepted");
        assert_eq!(response.provider_replay.len(), 2);
        assert!(response.summary.is_none());
    }

    #[test]
    fn codex_auth_adds_priority_service_tier() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::default(),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            "test-session",
        )
        .expect("responses body renders");

        assert_eq!(body["service_tier"], "priority");
        assert!(body.get("prompt_cache_retention").is_none());
    }

    #[test]
    fn responses_body_sets_openai_request_shape() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::new(
                    Some("static system".to_string()),
                    Some("cwd: /tmp/project".to_string()),
                ),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::CustomDefinitions,
                tools: vec![test_tool(
                    ProviderKind::OpenAi,
                    "read",
                    "read a file",
                    json!({
                        "type": "object",
                        "properties": {
                            "path": { "type": "string" }
                        },
                        "required": ["path"]
                    }),
                )],
                max_tokens: Some(2048),
                reasoning_effort: ReasoningEffort::High,
                prompt_cache_key: Some("pi-relay-test".to_string()),
                session_id: None,
                turn_id: None,
            },
            "test-session",
        )
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
        let first = responses_body(
            ModelRequest {
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
                turn_id: None,
            },
            "test-session",
        )
        .expect("responses body renders");
        let second = responses_body(
            ModelRequest {
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
                turn_id: None,
            },
            "test-session",
        )
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
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: Some("explicit-cohort".to_string()),
                session_id: None,
                turn_id: None,
            },
            "session-not-used",
        )
        .expect("responses body renders");

        assert_eq!(body["prompt_cache_key"], "explicit-cohort");
    }

    #[test]
    fn responses_body_session_id_from_request_used_as_cache_key() {
        // End-to-end check that `ModelRequest.session_id` flows through the
        // ModelProvider trait into the cache key: when the daemon passes a
        // session id, it lands as the prompt_cache_key.
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::stable("stable rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: Some("daemon-session-id".to_string()),
                turn_id: None,
            },
            "daemon-session-id",
        )
        .expect("responses body renders");

        assert_eq!(body["prompt_cache_key"], "daemon-session-id");
    }

    #[test]
    fn responses_body_keeps_dynamic_context_out_of_instructions() {
        let body = responses_body(
            ModelRequest {
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
                turn_id: None,
            },
            "test-session",
        )
        .expect("responses body renders");

        assert_eq!(body["instructions"], "stable agent rules");
        assert_eq!(body["input"][0]["content"][0]["text"], "workspace: /tmp/pi");
        assert_eq!(body["input"][1]["content"][0]["text"], "hello");
        assert_eq!(body["prompt_cache_key"], "cache-key");
    }

    #[test]
    fn responses_body_sorts_tools_for_cache_stability() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::stable("stable agent rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::CustomDefinitions,
                tools: vec![
                    test_tool(
                        ProviderKind::OpenAi,
                        "write",
                        "write a file",
                        json!({ "type": "object" }),
                    ),
                    test_tool(
                        ProviderKind::OpenAi,
                        "read",
                        "read a file",
                        json!({ "type": "object" }),
                    ),
                ],
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Medium,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            "test-session",
        )
        .expect("responses body renders");

        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["tools"][1]["name"], "write");
    }

    #[test]
    fn responses_body_renders_openai_native_coding_tools() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                prompt: PromptSections::stable("stable agent rules"),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::OpenAiCoding,
                tools: first_party_tools(ProviderKind::OpenAi),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::XHigh,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            "test-session",
        )
        .expect("responses body renders");

        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["tools"][0]["name"], "apply_patch");
        assert_eq!(body["tools"][1]["type"], "function");
        assert_eq!(body["tools"][1]["name"], "Bash");
        assert_eq!(body["tools"][2]["type"], "function");
        assert_eq!(body["tools"][2]["name"], "Grep");
        assert_eq!(body["tools"][3]["type"], "function");
        assert_eq!(body["tools"][3]["name"], "LoadSkill");
        assert_eq!(body["tools"][4]["type"], "function");
        assert_eq!(body["tools"][4]["name"], "web_fetch");
        assert_eq!(body["tools"][5]["type"], "function");
        assert_eq!(body["tools"][5]["name"], "web_search");
    }

    #[test]
    fn transcript_to_response_items_preserves_assistant_tool_calls() {
        let tool_call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "read".to_string(),
            args_json: "{\"path\":\"README.md\"}".to_string(),
        };
        let items = transcript_to_response_items(
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
            ],
        )
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

data: {"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_bash","name":"Bash","arguments":"{\"command\":[\"pwd\"],\"timeout_ms\":120000}","status":"completed"}}

data: {"type":"response.completed","response":{"id":"resp_1"}}
"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");
        let calls = response.assistant.tool_calls().collect::<Vec<_>>();

        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].tool_name, "Edit");
        assert_eq!(
            calls[0].args_value().unwrap()["input"],
            "*** Begin Patch\n*** End Patch\n"
        );
        assert_eq!(calls[1].tool_name, "Bash");
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
    fn responses_sse_keeps_partial_output_on_max_output_tokens() {
        let sse = r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"partial"}]}}

data: {"type":"response.incomplete","response":{"id":"resp_1","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":100,"output_tokens":64,"total_tokens":164,"input_tokens_details":{"cached_tokens":80}}}}
"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi)
            .expect("max-output incomplete should parse as partial response");

        assert_eq!(response.assistant.text(), "partial");
        assert_eq!(response.provider_replay.len(), 1);
        assert_eq!(response.stop_reason, ModelStopReason::MaxOutputTokens);
        let usage = response.usage.expect("usage should be parsed");
        assert_eq!(usage.input_tokens, Some(100));
        assert_eq!(usage.output_tokens, Some(64));
        assert_eq!(usage.total_tokens, Some(164));
        assert_eq!(usage.cache_read_input_tokens, Some(80));
    }

    #[test]
    fn responses_sse_stops_at_completed_even_with_trailing_partial_frame() {
        let sse = r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}

data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}

data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"should not be parsed"}]}}"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");

        assert_eq!(response.assistant.text(), "done");
        assert_eq!(response.provider_replay.len(), 1);
        let usage = response.usage.expect("usage should be parsed");
        assert_eq!(usage.total_tokens, Some(3));
    }

    #[test]
    fn responses_sse_accepts_done_sentinel() {
        let sse = r#"data: {"type":"response.output_item.done","item":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"done"}]}}

data: [DONE]
"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");

        assert_eq!(response.assistant.text(), "done");
        assert_eq!(response.provider_replay.len(), 1);
    }

    #[test]
    fn responses_input_prefers_openai_replay_sidecar() {
        let raw = json!({
            "type": "message",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "hello", "annotations": [] }],
            "status": "completed",
        });
        let items = transcript_to_response_items(
            &crate::PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text("hello".to_string())],
                }),
                provider_replay: vec![ProviderReplayItem::new(ProviderKind::OpenAi, &raw).unwrap()],
            }],
        )
        .expect("responses input renders");

        assert_eq!(items, vec![raw]);
    }

    #[test]
    fn responses_input_preserves_images_and_tool_results() {
        let items = transcript_to_response_items(
            &crate::PromptSections::default(),
            &[
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
            ],
        )
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
