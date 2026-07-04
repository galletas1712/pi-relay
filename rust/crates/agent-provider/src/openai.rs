use agent_tools::{tool_display, ProviderTool, ToolDisplayInput};
use agent_vocab::{
    AssistantItem, AssistantMessage, ContentBlock, ProviderKind, ProviderReplayItem,
    ReasoningEffort, ReplayDisplay, ToolCall, ToolCallId, TranscriptItem, TurnId, UserMessage,
};
use async_trait::async_trait;
use reqwest::header::{HeaderMap, ACCEPT, CONTENT_ENCODING, CONTENT_TYPE};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
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
    common::{ensure_success, push_text_item, response_excerpt, response_text},
    http::send_provider_generation_request,
    sse::{read_provider_json_sse_response, SseControl, SseEvent},
    ModelProvider, ModelRequest, ModelResponse, ModelStopDetails, ModelStopReason,
    ModelTranscriptEntry, ProviderCompactionRequest, ProviderCompactionResponse, ProviderError,
    ProviderResult, ProviderToolProfile, ProviderUsage,
};

const RESPONSES_REASONING_INCLUDE: &str = "reasoning.encrypted_content";
const OPENAI_PRIORITY_SERVICE_TIER: &str = "priority";
const OPENAI_MAX_CALL_ID_LEN: usize = 64;

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
}

#[cfg(test)]
mod ordinary_policy_tests {
    use super::*;

    #[test]
    fn responses_sse_preserves_supported_semantic_and_passive_items() {
        let items = vec![
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "without id" }],
            }),
            json!({
                "type": "message",
                "id": "msg_2",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "with id" }],
            }),
            json!({
                "type": "agent_message",
                "author": "/root/worker",
                "recipient": "/root",
                "content": [
                    { "type": "input_text", "text": "worker result" },
                    { "type": "input_text", "text": "continued" },
                ],
            }),
            json!({
                "type": "agent_message",
                "id": "amsg_2",
                "author": "/root/worker",
                "recipient": "/root",
                "content": [{ "type": "encrypted_content", "encrypted_content": "opaque" }],
            }),
            json!({
                "type": "context_compaction",
                "encrypted_content": "opaque-checkpoint",
                "future_extension": { "must_survive": true },
            }),
            json!({
                "type": "context_compaction",
            }),
            // Pinned Codex 98d28aa accepts all of these web-search shapes.
            json!({
                "type": "web_search_call",
                "status": "completed",
                "action": { "type": "search", "query": "rust" },
            }),
            json!({
                "type": "web_search_call",
                "status": "open",
                "action": { "type": "open_page", "url": "https://example.com" },
            }),
            json!({
                "type": "web_search_call",
                "id": "ws_partial",
                "status": "in_progress",
            }),
            // Current public Responses output permits failed image results to
            // be null or absent. Neither shape requires a client response.
            json!({
                "type": "image_generation_call",
                "status": "failed",
                "result": null,
            }),
            json!({
                "type": "image_generation_call",
                "status": "failed",
            }),
            // Hosted tool search is passive only when the provider executes it.
            json!({
                "type": "tool_search_call",
                "execution": "server",
            }),
        ];
        let mut sse = String::new();
        for (output_index, item) in items.iter().enumerate() {
            sse.push_str(&format!(
                "data: {}\n\n",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                })
            ));
        }
        sse.push_str(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        );

        let response = parse_responses_sse(&sse, ProviderKind::OpenAi)
            .expect("supported semantic and passive items parse");
        assert_eq!(
            response.assistant.text(),
            "without idwith idworker result\ncontinued"
        );
        assert_eq!(
            response
                .provider_replay
                .iter()
                .map(ProviderReplayItem::raw_value)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            items
        );
    }

    #[test]
    fn responses_sse_accepts_known_passive_types_without_cloning_their_schemas() {
        let mut items = [
            "reasoning",
            "web_search_call",
            "file_search_call",
            "code_interpreter_call",
            "image_generation_call",
            "mcp_call",
            "mcp_list_tools",
            "tool_search_output",
            "additional_tools",
            "compaction",
            "function_call_output",
            "custom_tool_call_output",
            "local_shell_call_output",
            "shell_call_output",
            "apply_patch_call_output",
            "computer_call_output",
            "mcp_approval_response",
            "context_compaction",
        ]
        .into_iter()
        .map(|item_type| json!({ "type": item_type }))
        .collect::<Vec<_>>();
        items.push(json!({
            "type": "tool_search_call",
            "execution": "server",
        }));

        let mut sse = String::new();
        for (output_index, item) in items.iter().enumerate() {
            sse.push_str(&format!(
                "data: {}\n\n",
                json!({
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                })
            ));
        }
        sse.push_str(
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\"}}\n\n",
        );

        let response =
            parse_responses_sse(&sse, ProviderKind::OpenAi).expect("known passive types parse");
        assert_eq!(
            response
                .provider_replay
                .iter()
                .map(ProviderReplayItem::raw_value)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            items
        );
    }
}

impl OpenAiCodexSessionState {
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

fn openai_required_string<'a>(
    item: &'a Value,
    item_type: &str,
    field: &str,
) -> ProviderResult<&'a str> {
    item.get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ProviderError::Provider(format!(
                "OpenAI {item_type} missing nonempty string {field}"
            ))
        })
}

fn openai_required_array<'a>(
    item: &'a Value,
    item_type: &str,
    field: &str,
) -> ProviderResult<&'a [Value]> {
    item.get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .ok_or_else(|| ProviderError::Provider(format!("OpenAI {item_type} missing {field} array")))
}

fn validate_openai_agent_message(item: &Value) -> ProviderResult<()> {
    openai_required_string(item, "agent_message", "author")?;
    openai_required_string(item, "agent_message", "recipient")?;
    for part in openai_required_array(item, "agent_message", "content")? {
        match openai_replay_item_type(part, "OpenAI agent_message content part")? {
            "input_text" => {
                if part.get("text").and_then(Value::as_str).is_none() {
                    return Err(ProviderError::Provider(
                        "OpenAI agent_message input_text missing text".to_string(),
                    ));
                }
            }
            "encrypted_content" => {
                openai_required_string(
                    part,
                    "agent_message encrypted_content",
                    "encrypted_content",
                )?;
            }
            part_type => {
                return Err(ProviderError::Provider(format!(
                    "OpenAI agent_message contained unsupported content part type {part_type}"
                )))
            }
        }
    }
    Ok(())
}

fn validate_openai_function_call(item: &Value) -> ProviderResult<()> {
    openai_required_string(item, "function_call", "call_id")?;
    openai_required_string(item, "function_call", "name")?;
    if item.get("arguments").and_then(Value::as_str).is_none() {
        return Err(ProviderError::Provider(
            "OpenAI function_call missing string arguments".to_string(),
        ));
    }
    Ok(())
}

fn validate_openai_custom_tool_call(item: &Value) -> ProviderResult<()> {
    openai_required_string(item, "custom_tool_call", "call_id")?;
    openai_required_string(item, "custom_tool_call", "name")?;
    if item.get("input").and_then(Value::as_str).is_none() {
        return Err(ProviderError::Provider(
            "OpenAI custom_tool_call missing string input".to_string(),
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OpenAiOrdinaryItemClass {
    Message,
    AgentMessage,
    FunctionCall,
    CustomToolCall,
    HostedOrPassive,
    ClientExecuted,
}

/// Classify ordinary Responses output at the adapter safety boundary.
///
/// Semantic items are validated only where pi-relay projects fields into its
/// normalized response. Known hosted/passive items are replayed opaquely:
/// cloning their evolving optional schemas here caused valid provider output
/// to fail. Known client actions fail closed, as do unknown item types.
fn classify_openai_ordinary_item(
    item_type: &str,
    item: &Value,
) -> ProviderResult<OpenAiOrdinaryItemClass> {
    let class = match item_type {
        "message" => OpenAiOrdinaryItemClass::Message,
        "agent_message" => OpenAiOrdinaryItemClass::AgentMessage,
        "function_call" => OpenAiOrdinaryItemClass::FunctionCall,
        "custom_tool_call" => OpenAiOrdinaryItemClass::CustomToolCall,
        "tool_search_call" => match item.get("execution").and_then(Value::as_str) {
            Some("server") => OpenAiOrdinaryItemClass::HostedOrPassive,
            Some("client") => OpenAiOrdinaryItemClass::ClientExecuted,
            Some(execution) => {
                return Err(ProviderError::Provider(format!(
                    "OpenAI tool_search_call had unknown execution mode {execution}"
                )))
            }
            None => {
                return Err(ProviderError::Provider(
                    "OpenAI tool_search_call missing string execution mode".to_string(),
                ))
            }
        },
        "local_shell_call"
        | "shell_call"
        | "apply_patch_call"
        | "computer_call"
        | "mcp_approval_request" => OpenAiOrdinaryItemClass::ClientExecuted,
        "reasoning"
        | "web_search_call"
        | "file_search_call"
        | "code_interpreter_call"
        | "image_generation_call"
        | "mcp_call"
        | "mcp_list_tools"
        | "tool_search_output"
        | "additional_tools"
        | "compaction"
        | "context_compaction"
        | "function_call_output"
        | "custom_tool_call_output"
        | "local_shell_call_output"
        | "shell_call_output"
        | "apply_patch_call_output"
        | "computer_call_output"
        | "mcp_approval_response" => OpenAiOrdinaryItemClass::HostedOrPassive,
        _ => {
            return Err(ProviderError::Provider(format!(
                "OpenAI returned unknown output item type {item_type}; refusing to assume it requires no client response"
            )))
        }
    };
    Ok(class)
}

fn validate_openai_added_item_policy(item_type: &str, item: &Value) -> ProviderResult<()> {
    let class = if item_type == "tool_search_call"
        && item.get("execution").and_then(Value::as_str).is_none()
    {
        // Added items may be partial. The matching done item must provide the
        // execution mode needed to distinguish hosted from client execution.
        OpenAiOrdinaryItemClass::HostedOrPassive
    } else {
        classify_openai_ordinary_item(item_type, item)?
    };
    if class == OpenAiOrdinaryItemClass::ClientExecuted {
        return Err(ProviderError::Provider(format!(
            "OpenAI returned unsupported client-executed action type {item_type}"
        )));
    }
    Ok(())
}

fn reconcile_openai_item_identity(
    index: usize,
    earlier_label: &str,
    earlier: &Value,
    later_label: &str,
    later: &Value,
) -> ProviderResult<()> {
    let earlier_type =
        openai_replay_item_type(earlier, &format!("OpenAI {earlier_label} output item"))?;
    let later_type = openai_replay_item_type(later, &format!("OpenAI {later_label} output item"))?;
    if later_type != earlier_type {
        return Err(ProviderError::Provider(format!(
            "OpenAI output item type changed at output_index {index}: {earlier_label} {earlier_type}, {later_label} {later_type}"
        )));
    }
    for field in ["id", "call_id"] {
        if let Some(earlier_id) = earlier
            .get(field)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
        {
            if later.get(field).and_then(Value::as_str) != Some(earlier_id) {
                return Err(ProviderError::Provider(format!(
                    "OpenAI output item {field} changed at output_index {index} between {earlier_label} and {later_label}"
                )));
            }
        }
    }
    Ok(())
}

fn reconcile_openai_added_item(index: usize, added: &Value, done: &Value) -> ProviderResult<()> {
    reconcile_openai_item_identity(index, "added", added, "done", done)
}

fn response_daemon_tool_call_item(observation: &agent_vocab::DaemonToolObservation) -> Value {
    let call_id = openai_daemon_observation_call_id(observation);
    json!({
        "type": "function_call",
        "call_id": call_id,
        "name": openai_wire_tool_name(&observation.tool_name),
        "arguments": observation.args_json,
    })
}

fn response_daemon_tool_result_item(
    observation: &agent_vocab::DaemonToolObservation,
) -> ProviderResult<Value> {
    let call_id = openai_daemon_observation_call_id(observation);
    Ok(json!({
        "type": "function_call_output",
        "call_id": call_id,
        "output": observation.result_text()?,
    }))
}

fn openai_daemon_observation_call_id(observation: &agent_vocab::DaemonToolObservation) -> String {
    let original = observation.tool_call_id.as_str();
    if original.len() <= OPENAI_MAX_CALL_ID_LEN {
        return original.to_string();
    }

    // Old stored delegation wakeups used
    // `call_inspect_delegation_<delegation_uuid>_<attempt_uuid>`, which can be
    // >100 chars. The OpenAI Responses API rejects call_id >64. Do not mutate
    // durable transcript data here; render a deterministic provider-local id
    // and use it for both the synthetic call and output so historical broken
    // sessions can replay.
    let mut hasher = Sha256::new();
    hasher.update(observation.tool_name.as_bytes());
    hasher.update(b"\0");
    hasher.update(observation.args_json.as_bytes());
    hasher.update(b"\0");
    hasher.update(original.as_bytes());
    let digest = hasher.finalize();
    let mut suffix = String::with_capacity(32);
    for byte in digest.iter().take(16) {
        use std::fmt::Write as _;
        let _ = write!(&mut suffix, "{byte:02x}");
    }
    format!("call_daemon_{suffix}")
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

    if output.is_empty() {
        return Err(ProviderError::Provider(
            "OpenAI compact response had an empty output array".to_string(),
        ));
    }

    let mut compaction_count = 0;
    let mut summary_parts = Vec::new();
    for item in output {
        openai_replay_item_type(item, "OpenAI compact output item")?;
        if is_openai_compaction_item(item) {
            compaction_count += 1;
        }
        collect_compact_summary_text(item, &mut summary_parts);
    }
    if compaction_count != 1 {
        return Err(ProviderError::Provider(format!(
            "OpenAI compact response expected exactly one compaction item, found {compaction_count}; output item types: {}",
            compact_output_type_summary(output),
        )));
    }

    let summary = summary_parts.join("").trim().to_string();
    let provider_replay = output
        .iter()
        .map(|item| {
            ProviderReplayItem::new(ProviderKind::OpenAi, item).map_err(ProviderError::Json)
        })
        .collect::<ProviderResult<Vec<_>>>()?;
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

fn is_openai_compaction_item(item: &Value) -> bool {
    matches!(
        item.get("type").and_then(Value::as_str),
        Some("compaction" | "compaction_summary")
    )
}

fn openai_replay_item_type<'a>(item: &'a Value, context: &str) -> ProviderResult<&'a str> {
    let object = item
        .as_object()
        .ok_or_else(|| ProviderError::Provider(format!("{context} was not an object")))?;
    object
        .get("type")
        .and_then(Value::as_str)
        .filter(|item_type| !item_type.is_empty())
        .ok_or_else(|| ProviderError::Provider(format!("{context} missing nonempty string type")))
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
                Some("output_text") | Some("input_text") => {
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

        let response = send_provider_generation_request(
            zstd_json_request(
                self.add_codex_headers(
                    self.client
                        .post(format!("{}/responses", self.base_url.trim_end_matches('/')))
                        .header(ACCEPT, "text/event-stream"),
                    &session_id,
                    &window_id,
                    codex_turn_state.as_deref(),
                ),
                &body,
            )?,
            "OpenAI /responses",
        )
        .await?;
        let response_success = response.status().is_success();
        let response_headers = OpenAiResponseHeaders::from_headers(response.headers());
        if let (Some(turn_state), Some(session_state)) = (
            response_success
                .then(|| response_headers.codex_turn_state.clone())
                .flatten(),
            self.session_state
                .as_deref()
                .filter(|state| state.session_id() == session_id),
        ) {
            session_state.record_turn_state(turn_id, turn_state);
        }
        let mut parsed = parse_responses_stream(response, ProviderKind::OpenAi).await?;
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
        ensure_success(status, &text, response_error_message)?;
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

fn responses_body(request: ModelRequest, session_id: &str) -> ProviderResult<Value> {
    let reasoning_effort = openai_reasoning_effort(&request.model, request.reasoning_effort);
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
    //   3. Fresh UUID/test fallback supplied by `openai_session_id` when
    //      neither the request nor provider session state has an id.
    let prompt_cache_key = request
        .prompt_cache_key
        .unwrap_or_else(|| session_id.to_string());
    let mut body = json!({
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
    if let Some(max_output_tokens) = request.max_tokens {
        body["max_output_tokens"] = json!(max_output_tokens);
    }
    Ok(body)
}

// The compaction endpoint is unary, so keep streaming-only `/responses` fields
// out. This is a valid subset of Codex CLI's current `CompactionInput`: pi-relay
// has no text/verbosity request control to forward yet.
fn compact_body(request: ProviderCompactionRequest, session_id: &str) -> ProviderResult<Value> {
    let reasoning_effort = openai_reasoning_effort(&request.model, request.reasoning_effort);
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

// Map a reasoning effort to the OpenAI wire string. The daemon normalizes the
// session effort to a model-supported value before building the request (see
// `model_metadata::normalize_reasoning_effort`), so this should always receive a
// value OpenAI accepts. Defensively clamp `minimal` (all current gpt-5.x) and
// `max` (all current gpt-5.x except the verified GPT-5.6 hosted family) rather
// than letting a stray direct adapter call produce a 400.
fn is_hosted_gpt56(model: &str) -> bool {
    ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"].contains(&model)
}

fn openai_reasoning_effort(model: &str, effort: ReasoningEffort) -> &'static str {
    match effort {
        ReasoningEffort::None
        | ReasoningEffort::Low
        | ReasoningEffort::Medium
        | ReasoningEffort::High
        | ReasoningEffort::XHigh => effort.as_str(),
        ReasoningEffort::Minimal => ReasoningEffort::Low.as_str(),
        ReasoningEffort::Max if is_hosted_gpt56(model) => ReasoningEffort::Max.as_str(),
        ReasoningEffort::Max => ReasoningEffort::XHigh.as_str(),
    }
}

pub(crate) fn transcript_to_response_items(
    _prompt: &crate::PromptSections,
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
            TranscriptItem::CompactionSummary(_) => {
                let replay = openai_replay_items(entry, true)?;
                validate_openai_compaction_replay(&replay)?;
                responses.extend(replay);
            }
            TranscriptItem::AssistantMessage(message) => {
                let replay_items = openai_replay_items(entry, false)?;
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
            TranscriptItem::DaemonToolObservation(observation) => {
                responses.push(response_daemon_tool_call_item(observation));
                responses.push(response_daemon_tool_result_item(observation)?);
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
    items.extend(transcript_to_response_items(prompt, transcript)?);
    if let Some(dynamic_context) = dynamic_context.filter(|value| !value.trim().is_empty()) {
        items.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": dynamic_context }],
        }));
    }
    Ok(items)
}

fn openai_replay_items(
    entry: &ModelTranscriptEntry,
    allow_unknown_compact_items: bool,
) -> ProviderResult<Vec<Value>> {
    let replay = entry
        .provider_replay_values_for(ProviderKind::OpenAi)
        .map_err(ProviderError::Json)?;
    if !allow_unknown_compact_items {
        for item in &replay {
            let item_type = openai_replay_item_type(item, "persisted OpenAI replay item")?;
            validate_openai_ordinary_item_policy(item_type, item)?;
        }
    }
    Ok(replay)
}

fn validate_openai_compaction_replay(replay: &[Value]) -> ProviderResult<()> {
    if replay.is_empty() {
        return Err(ProviderError::Provider(
            "refusing missing persisted OpenAI compaction replay".to_string(),
        ));
    }
    let mut compaction_count = 0;
    for item in replay {
        openai_replay_item_type(item, "persisted OpenAI compaction replay item")?;
        if is_openai_compaction_item(item) {
            compaction_count += 1;
        }
    }
    if compaction_count != 1 {
        return Err(ProviderError::Provider(
            format!(
                "refusing malformed persisted OpenAI compaction replay: expected exactly one native compaction item, found {compaction_count}"
            ),
        ));
    }
    Ok(())
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

async fn parse_responses_stream(
    response: reqwest::Response,
    provider: ProviderKind,
) -> ProviderResult<ModelResponse> {
    let mut state = ResponsesStreamState::new(provider);
    read_provider_json_sse_response(
        response,
        "OpenAI response stream",
        response_error_message,
        |event| state.process_sse_event(event),
    )
    .await?;
    state.finish()
}

#[cfg(test)]
fn parse_responses_sse(text: &str, provider: ProviderKind) -> ProviderResult<ModelResponse> {
    let mut state = ResponsesStreamState::new(provider);
    read_json_sse_text(text, |event| state.process_sse_event(event))?;
    state.finish()
}

#[cfg_attr(test, derive(Debug, Clone, PartialEq, Eq))]
struct ResponsesMaterializedOutput {
    items: Vec<AssistantItem>,
    provider_replay: Vec<ProviderReplayItem>,
    stop_reason: ModelStopReason,
    stop_details: Option<ModelStopDetails>,
}

#[cfg_attr(test, derive(Debug, Clone, PartialEq, Eq))]
struct ResponsesStreamState {
    provider: ProviderKind,
    added_items: BTreeMap<usize, Value>,
    output_items: BTreeMap<usize, Value>,
    usage: Option<ProviderUsage>,
    materialized: Option<ResponsesMaterializedOutput>,
    completed: bool,
}

impl ResponsesStreamState {
    fn new(provider: ProviderKind) -> Self {
        Self {
            provider,
            added_items: BTreeMap::new(),
            output_items: BTreeMap::new(),
            usage: None,
            materialized: None,
            completed: false,
        }
    }

    fn finish(self) -> ProviderResult<ModelResponse> {
        if !self.completed {
            return Err(ProviderError::Provider(
                "OpenAI response stream ended before response.completed".to_string(),
            ));
        }
        let materialized = self
            .materialized
            .expect("completed OpenAI response has staged materialized output");
        Ok(ModelResponse {
            assistant: AssistantMessage {
                items: materialized.items,
            },
            provider_replay: materialized.provider_replay,
            usage: self.usage,
            stop_reason: materialized.stop_reason,
            stop_details: materialized.stop_details,
        })
    }

    fn materialize_output_items(
        output_items: &BTreeMap<usize, Value>,
        provider: ProviderKind,
    ) -> ProviderResult<ResponsesMaterializedOutput> {
        Self::validate_output_items(output_items)?;
        let mut items = Vec::new();
        let mut provider_replay = Vec::new();
        let mut stop_reason = ModelStopReason::Complete;
        let mut stop_details = None;
        for item in output_items.values() {
            if let Some(refusal) =
                parse_response_output_item(item, &mut items, &mut provider_replay, provider)?
            {
                stop_reason = ModelStopReason::Refusal;
                stop_details = Some(ModelStopDetails {
                    category: None,
                    explanation: Some(refusal),
                });
            }
        }
        if stop_reason == ModelStopReason::Refusal {
            items.clear();
            provider_replay.clear();
        }
        Ok(ResponsesMaterializedOutput {
            items,
            provider_replay,
            stop_reason,
            stop_details,
        })
    }

    fn validate_output_items(output_items: &BTreeMap<usize, Value>) -> ProviderResult<()> {
        Self::validate_output_indices(output_items)?;
        Self::validate_output_identities(output_items)
    }

    fn validate_output_indices(output_items: &BTreeMap<usize, Value>) -> ProviderResult<()> {
        for (expected, actual) in output_items.keys().copied().enumerate() {
            if actual != expected {
                return Err(ProviderError::Provider(format!(
                    "OpenAI response output indices were not contiguous: expected {expected}, found {actual}"
                )));
            }
        }
        Ok(())
    }

    fn validate_output_identities(output_items: &BTreeMap<usize, Value>) -> ProviderResult<()> {
        let mut identities = BTreeMap::new();
        for (index, item) in output_items {
            let item_type = openai_replay_item_type(item, "OpenAI output item")?;
            for field in ["id", "call_id"] {
                let Some(identity) = item
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                else {
                    continue;
                };
                let key = (item_type.to_string(), field, identity.to_string());
                if let Some(previous_index) = identities.insert(key, *index) {
                    return Err(ProviderError::Provider(format!(
                        "OpenAI output item type {item_type} duplicated stable {field} {identity} at output indices {previous_index} and {index}"
                    )));
                }
            }
        }
        Ok(())
    }

    fn output_item_event<'a>(
        event: &'a Value,
        event_type: &str,
    ) -> ProviderResult<(usize, &'a Value, &'a str)> {
        let index = event
            .get("output_index")
            .and_then(Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .ok_or_else(|| {
                ProviderError::Provider(format!("OpenAI {event_type} missing valid output_index"))
            })?;
        let item = event
            .get("item")
            .ok_or_else(|| ProviderError::Provider(format!("OpenAI {event_type} missing item")))?;
        let item_type = openai_replay_item_type(item, &format!("OpenAI {event_type} item"))?;
        Ok((index, item, item_type))
    }

    fn record_added_item(&mut self, event: &Value) -> ProviderResult<()> {
        let (index, item, item_type) =
            Self::output_item_event(event, "response.output_item.added")?;
        validate_openai_added_item_policy(item_type, item)?;
        if self.output_items.contains_key(&index) {
            return Err(ProviderError::Provider(format!(
                "OpenAI response.output_item.added arrived after done at output_index {index}"
            )));
        }
        if self.added_items.insert(index, item.clone()).is_some() {
            return Err(ProviderError::Provider(format!(
                "OpenAI response.output_item.added duplicated output_index {index}"
            )));
        }
        Ok(())
    }

    fn record_done_item(&mut self, event: &Value) -> ProviderResult<()> {
        let (index, item, _) = Self::output_item_event(event, "response.output_item.done")?;
        if let Some(added_item) = self.added_items.get(&index) {
            reconcile_openai_added_item(index, added_item, item)?;
        }
        if self.output_items.insert(index, item.clone()).is_some() {
            return Err(ProviderError::Provider(format!(
                "OpenAI response.output_item.done duplicated output_index {index}"
            )));
        }
        Ok(())
    }

    fn reconcile_added_items(&self) -> ProviderResult<()> {
        if self.added_items.is_empty() {
            return Ok(());
        }
        for (index, added_item) in &self.added_items {
            let done_item = self.output_items.get(index).ok_or_else(|| {
                ProviderError::Provider(format!(
                    "OpenAI response completed with pending output item at output_index {index}"
                ))
            })?;
            reconcile_openai_added_item(*index, added_item, done_item)?;
        }
        Ok(())
    }

    fn reconcile_terminal_output(&self, output: &Value) -> ProviderResult<BTreeMap<usize, Value>> {
        let output = output.as_array().ok_or_else(|| {
            ProviderError::Provider(
                "OpenAI response.completed response.output was not an array".to_string(),
            )
        })?;
        let mut reconciled_items = self.output_items.clone();
        for (index, item) in output.iter().enumerate() {
            if let Some(done_item) = self.output_items.get(&index) {
                reconcile_openai_item_identity(index, "done", done_item, "terminal", item)?;
            } else {
                reconciled_items.insert(index, item.clone());
            }
        }
        Ok(reconciled_items)
    }

    fn process_sse_event(&mut self, event: SseEvent) -> ProviderResult<SseControl> {
        match event {
            SseEvent::Json(event) => self.process_event(&event),
            SseEvent::MalformedJson => Err(ProviderError::Provider(
                "OpenAI response stream contained malformed JSON event data".to_string(),
            )),
            SseEvent::Done => Ok(SseControl::Continue),
        }
    }

    fn process_event(&mut self, event: &Value) -> ProviderResult<SseControl> {
        match event.get("type").and_then(Value::as_str) {
            Some("response.output_item.added") => {
                self.record_added_item(event)?;
                Ok(SseControl::Continue)
            }
            Some("response.output_item.done") => {
                self.record_done_item(event)?;
                Ok(SseControl::Continue)
            }
            Some("response.failed") => {
                let code = event
                    .pointer("/response/error/code")
                    .and_then(Value::as_str);
                let message = openai_error_message(
                    code,
                    event
                        .pointer("/response/error/message")
                        .and_then(Value::as_str)
                        .or(Some("response.failed")),
                    event,
                );
                Err(openai_provider_error_from_code(code, message))
            }
            Some("response.incomplete") => {
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_string();
                let status = event
                    .pointer("/response/status")
                    .and_then(Value::as_str)
                    .unwrap_or("incomplete")
                    .to_string();
                Err(ProviderError::Incomplete { status, reason })
            }
            Some("response.completed") => {
                let response = event
                    .get("response")
                    .and_then(Value::as_object)
                    .ok_or_else(|| {
                        ProviderError::Provider(
                            "OpenAI response.completed missing response object".to_string(),
                        )
                    })?;
                if response
                    .get("id")
                    .and_then(Value::as_str)
                    .is_none_or(str::is_empty)
                {
                    return Err(ProviderError::Provider(
                        "OpenAI response.completed missing response id".to_string(),
                    ));
                }
                if let Some(status) = response.get("status") {
                    if status.as_str() != Some("completed") {
                        return Err(ProviderError::Provider(format!(
                            "OpenAI response.completed had invalid status {status}"
                        )));
                    }
                }
                self.reconcile_added_items()?;
                let output_items = if let Some(output) = response.get("output") {
                    self.reconcile_terminal_output(output)?
                } else {
                    self.output_items.clone()
                };
                let materialized = Self::materialize_output_items(&output_items, self.provider)?;
                let usage = response.get("usage").and_then(openai_usage);

                self.output_items = output_items;
                self.usage = usage;
                self.materialized = Some(materialized);
                self.completed = true;
                Ok(SseControl::Stop)
            }
            Some("error") => {
                let code = event.get("code").and_then(Value::as_str);
                let message = openai_error_message(
                    code,
                    event
                        .get("message")
                        .or_else(|| event.get("code"))
                        .and_then(Value::as_str),
                    event,
                );
                Err(openai_provider_error_from_code(
                    code,
                    format!("Codex error: {message}"),
                ))
            }
            _ => Ok(SseControl::Continue),
        }
    }
}

fn openai_error_message(code: Option<&str>, message: Option<&str>, event: &Value) -> String {
    let message = message
        .map(str::to_string)
        .unwrap_or_else(|| event.to_string());
    if let Some(code) = code {
        if !message.contains(code) {
            return format!("{code}: {message}");
        }
    }
    message
}

fn openai_provider_error_from_code(code: Option<&str>, message: String) -> ProviderError {
    match code {
        Some("rate_limit_exceeded") => ProviderError::Status {
            status: 429,
            message,
        },
        Some("internal_error" | "server_error") => ProviderError::Status {
            status: 500,
            message,
        },
        Some("overloaded_error" | "server_is_overloaded" | "slow_down") => ProviderError::Status {
            status: 529,
            message,
        },
        Some(
            "context_length_exceeded"
            | "cyber_policy"
            | "insufficient_quota"
            | "invalid_prompt"
            | "usage_not_included",
        ) => ProviderError::Provider(message),
        Some(_) | None => ProviderError::Transient(message),
    }
}

fn parse_response_output_item(
    item: &Value,
    items: &mut Vec<AssistantItem>,
    provider_replay: &mut Vec<ProviderReplayItem>,
    provider: ProviderKind,
) -> ProviderResult<Option<String>> {
    let item_type = openai_replay_item_type(item, "OpenAI output item")?;
    let class = validate_openai_ordinary_item_policy(item_type, item)?;
    let display = openai_provider_replay_display(item);

    match class {
        OpenAiOrdinaryItemClass::Message => {
            let content = item
                .get("content")
                .and_then(Value::as_array)
                .expect("message content validated");
            let mut refusal = None;
            for part in content {
                match part["type"].as_str().expect("validated content part type") {
                    "output_text" => {
                        let text = part["text"].as_str().expect("validated output text");
                        if !text.is_empty() {
                            push_text_item(items, text);
                        }
                    }
                    "refusal" => {
                        let message = part["refusal"].as_str().expect("validated refusal text");
                        refusal = Some(message.to_string());
                    }
                    _ => unreachable!("validated message content part type"),
                }
            }
            provider_replay.push(ProviderReplayItem::new_with_display(
                provider, item, display,
            )?);
            return Ok(refusal);
        }
        OpenAiOrdinaryItemClass::FunctionCall => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .expect("function_call call_id validated");
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .expect("function_call name validated");
            let arguments = item
                .get("arguments")
                .and_then(Value::as_str)
                .expect("function_call arguments validated");
            items.push(AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new(call_id),
                tool_name: crate::canonical_tool_name_for_provider(provider, name).to_string(),
                args_json: arguments.to_string(),
            }));
        }
        OpenAiOrdinaryItemClass::CustomToolCall => {
            let call_id = item
                .get("call_id")
                .and_then(Value::as_str)
                .expect("custom_tool_call call_id validated");
            let name = item
                .get("name")
                .and_then(Value::as_str)
                .expect("custom_tool_call name validated");
            let input = item
                .get("input")
                .and_then(Value::as_str)
                .expect("custom_tool_call input validated");
            items.push(AssistantItem::ToolCall(ToolCall {
                id: ToolCallId::new(call_id),
                tool_name: crate::canonical_tool_name_for_provider(provider, name).to_string(),
                args_json: json!({ "input": input }).to_string(),
            }));
        }
        OpenAiOrdinaryItemClass::AgentMessage => {
            let content = item["content"]
                .as_array()
                .expect("agent_message content validated");
            if content
                .iter()
                .all(|part| part["type"].as_str() == Some("input_text"))
            {
                let text = content
                    .iter()
                    .map(|part| part["text"].as_str().expect("agent_message text validated"))
                    .collect::<Vec<_>>()
                    .join("\n");
                if !text.trim().is_empty() {
                    push_text_item(items, &text);
                }
            }
        }
        OpenAiOrdinaryItemClass::HostedOrPassive => {}
        OpenAiOrdinaryItemClass::ClientExecuted => {
            unreachable!("client-executed output rejected by policy")
        }
    }
    provider_replay.push(ProviderReplayItem::new_with_display(
        provider, item, display,
    )?);
    Ok(None)
}

fn validate_openai_output_message(item: &Value) -> ProviderResult<&[Value]> {
    if item.get("role").and_then(Value::as_str) != Some("assistant") {
        return Err(ProviderError::Provider(
            "OpenAI message output missing assistant role".to_string(),
        ));
    }
    let content = item
        .get("content")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            ProviderError::Provider("OpenAI message output missing content array".to_string())
        })?;
    for part in content {
        match openai_replay_item_type(part, "OpenAI message content part")? {
            "output_text" => {
                if part.get("text").and_then(Value::as_str).is_none() {
                    return Err(ProviderError::Provider(
                        "OpenAI output_text content missing text".to_string(),
                    ));
                }
            }
            "refusal"
                if part
                    .get("refusal")
                    .and_then(Value::as_str)
                    .is_some_and(|value| !value.is_empty()) => {}
            "refusal" => {
                return Err(ProviderError::Provider(
                    "OpenAI refusal content missing nonempty refusal text".to_string(),
                ))
            }
            part_type => {
                return Err(ProviderError::Provider(format!(
                    "OpenAI message contained unsupported content part type {part_type}"
                )))
            }
        }
    }
    Ok(content)
}

fn validate_openai_ordinary_item_policy(
    item_type: &str,
    item: &Value,
) -> ProviderResult<OpenAiOrdinaryItemClass> {
    let class = classify_openai_ordinary_item(item_type, item)?;
    match class {
        OpenAiOrdinaryItemClass::Message => {
            validate_openai_output_message(item)?;
        }
        OpenAiOrdinaryItemClass::AgentMessage => validate_openai_agent_message(item)?,
        OpenAiOrdinaryItemClass::FunctionCall => validate_openai_function_call(item)?,
        OpenAiOrdinaryItemClass::CustomToolCall => validate_openai_custom_tool_call(item)?,
        OpenAiOrdinaryItemClass::HostedOrPassive => {}
        OpenAiOrdinaryItemClass::ClientExecuted => {
            return Err(ProviderError::Provider(format!(
                "OpenAI returned unsupported client-executed action type {item_type}"
            )))
        }
    }
    Ok(class)
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
    use agent_vocab::{CompactionSummary, ToolCall, ToolResultMessage, TurnId};
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
    fn compact_body_is_supported_codex_compaction_input_subset() {
        // The codex backend's `/responses/compact` is unary, so the body must
        // stay on the supported subset rather than the full streaming
        // `/responses` envelope. pi-relay has no text/verbosity control yet.
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
                compaction_instructions: None,
            },
            "session-1",
        )
        .expect("compact body renders");

        assert_eq!(body["model"], "gpt-5.5");
        assert_eq!(body["instructions"], "stable rules");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["input"][1]["content"][0]["text"], "cwd: /tmp/project");
        assert_eq!(body["tools"], json!([]));
        assert_eq!(body["parallel_tool_calls"], true);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["prompt_cache_key"], "session-1");
        assert_eq!(body["service_tier"], OPENAI_PRIORITY_SERVICE_TIER);

        for forbidden in ["tool_choice", "store", "stream", "include"] {
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
                compaction_instructions: None,
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
            transcript_cache_prefix_len: None,
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
        assert!(message.contains("expected exactly one compaction item, found 0"));
        assert!(message.contains("message:assistant=1"));
        assert!(message.contains("reasoning=1"));
    }

    #[test]
    fn compact_parser_accepts_current_compaction_summary_alias_without_rewriting_it() {
        let response = parse_compact_response(
            r#"{"output":[{"type":"compaction_summary","encrypted_content":"opaque"}]}"#,
        )
        .expect("current Codex compaction alias is valid");

        assert_eq!(
            response.provider_replay[0].raw_value().unwrap(),
            json!({ "type": "compaction_summary", "encrypted_content": "opaque" })
        );
    }

    #[test]
    fn compact_parser_requires_exactly_one_checkpoint_across_current_aliases() {
        for (name, output) in [
            (
                "zero",
                json!([{ "type": "reasoning", "encrypted_content": "opaque" }]),
            ),
            (
                "duplicate canonical",
                json!([
                    { "type": "compaction", "encrypted_content": "one" },
                    { "type": "compaction", "encrypted_content": "two" },
                ]),
            ),
            (
                "duplicate alias",
                json!([
                    { "type": "compaction_summary", "encrypted_content": "one" },
                    { "type": "compaction_summary", "encrypted_content": "two" },
                ]),
            ),
            (
                "mixed duplicate",
                json!([
                    { "type": "compaction", "encrypted_content": "one" },
                    { "type": "compaction_summary", "encrypted_content": "two" },
                ]),
            ),
        ] {
            let error =
                parse_compact_response(&json!({ "output": output }).to_string()).expect_err(name);
            assert!(error.to_string().contains("exactly one"), "{name}: {error}");
        }
    }

    #[test]
    fn compact_parser_rejects_items_without_minimum_replay_shape() {
        for malformed in [
            Value::Null,
            json!(7),
            json!({}),
            json!({ "type": null }),
            json!({ "type": 7 }),
            json!({ "type": "" }),
        ] {
            let body = json!({
                "output": [
                    malformed,
                    { "type": "compaction", "encrypted_content": "opaque" },
                ],
            });
            assert!(parse_compact_response(&body.to_string()).is_err(), "{body}");
        }
    }

    #[test]
    fn compact_parser_keeps_known_item_internals_opaque() {
        let output = json!([
            json!({ "type": "message", "role": "assistant" }),
            json!({ "type": "message", "role": "assistant", "content": "summary" }),
            json!({ "type": "message", "role": "assistant", "content": [null] }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text" }],
            }),
            json!({
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "future_content", "text": "summary" }],
            }),
            json!({ "type": "compaction", "future_checkpoint_shape": 7 }),
        ]);
        let response = parse_compact_response(&json!({ "output": output }).to_string())
            .expect("compact item internals are opaque");
        assert_eq!(
            response
                .provider_replay
                .iter()
                .map(ProviderReplayItem::raw_value)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            output.as_array().unwrap().clone()
        );
        assert!(response.summary.is_none());
    }

    #[test]
    fn compact_parser_preserves_every_output_item_in_provider_order() {
        let output = json!([
            {
                "id": "msg_user",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "Current working directory: this is genuine user text"
                }]
            },
            {
                "id": "msg_user_starting_cwd",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "Starting working directory for this session: genuine user text"
                }]
            },
            {
                "id": "msg_user_bash",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "The Bash tool runs each command in a fresh shell rooted here; quote this text"
                }]
            },
            {
                "id": "msg_user_prior_compaction",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "The conversation history before this point was compacted; explain that phrase"
                }]
            },
            {
                "id": "msg_user_billing",
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_text",
                    "text": "X-Anthropic-Billing-Header: genuine user text"
                }]
            },
            {
                "id": "msg_developer",
                "type": "message",
                "role": "developer",
                "content": [{ "type": "input_text", "text": "retained developer rule" }]
            },
            {
                "id": "rs_1",
                "type": "reasoning",
                "summary": [],
                "encrypted_content": "reasoning-ciphertext"
            },
            {
                "id": "fc_1",
                "type": "function_call",
                "call_id": "call_1",
                "name": "web_search",
                "arguments": "{\"query\":\"rust\"}"
            },
            {
                "type": "function_call_output",
                "call_id": "call_1",
                "output": "result"
            },
            {
                "id": "ws_1",
                "type": "web_search_call",
                "status": "completed",
                "action": { "type": "search", "query": "rust" }
            },
            {
                "id": "future_1",
                "type": "future_compact_extension",
                "extension": { "must_survive": true }
            },
            {
                "id": "msg_assistant",
                "type": "message",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "display summary" }]
            },
            {
                "id": "cmp_1",
                "type": "compaction",
                "encrypted_content": "opaque-compaction"
            }
        ]);
        let response = parse_compact_response(
            &json!({ "output": output, "usage": { "input_tokens": 12 } }).to_string(),
        )
        .expect("canonical compact output parses");
        let replay = response
            .provider_replay
            .iter()
            .map(ProviderReplayItem::raw_value)
            .collect::<Result<Vec<_>, _>>()
            .expect("replay remains valid JSON");

        assert_eq!(replay, output.as_array().unwrap().clone());
        assert_eq!(response.summary.as_deref(), Some("display summary"));

        let rerendered = transcript_to_response_items(
            &PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::CompactionSummary(CompactionSummary::new(
                    "session",
                    "leaf",
                    "semantic display summary must not enter replay",
                    Some(123),
                    TurnId(3),
                )),
                provider_replay: response.provider_replay,
            }],
        )
        .expect("canonical replay renders");
        assert_eq!(rerendered, output.as_array().unwrap().clone());
    }

    #[test]
    fn compact_parser_rejects_empty_output_but_keeps_checkpoint_payload_opaque() {
        assert!(parse_compact_response(r#"{"output":[]}"#).is_err());
        for checkpoint in [
            json!({ "type": "compaction" }),
            json!({ "type": "compaction", "encrypted_content": 7 }),
        ] {
            let response =
                parse_compact_response(&json!({ "output": [checkpoint.clone()] }).to_string())
                    .expect("checkpoint payload remains opaque");
            assert_eq!(response.provider_replay[0].raw_value().unwrap(), checkpoint);
        }
    }

    #[test]
    fn codex_auth_adds_priority_service_tier() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                transcript_cache_prefix_len: None,
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
                transcript_cache_prefix_len: None,
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
        assert_eq!(body["max_output_tokens"], 2048);
        assert_eq!(body["tools"][0]["name"], "read");
        assert_eq!(body["instructions"], "static system");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(body["input"][1]["content"][0]["text"], "cwd: /tmp/project");
    }

    #[test]
    fn responses_body_sends_gpt56_max_reasoning() {
        for model in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            let body =
                responses_body(
                    ModelRequest {
                        model: model.to_string(),
                        transcript_cache_prefix_len: None,
                        prompt: PromptSections::default(),
                        transcript: vec![
                            TranscriptItem::UserMessage(UserMessage::text("hello")).into()
                        ],
                        tool_profile: ProviderToolProfile::None,
                        tools: Vec::new(),
                        max_tokens: None,
                        reasoning_effort: ReasoningEffort::Max,
                        prompt_cache_key: None,
                        session_id: None,
                        turn_id: None,
                    },
                    "test-session",
                )
                .expect("responses body renders");

            assert_eq!(body["reasoning"]["effort"], "max", "{model}");
        }
    }

    #[test]
    fn responses_body_clamps_older_model_max_reasoning() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                transcript_cache_prefix_len: None,
                prompt: PromptSections::default(),
                transcript: vec![TranscriptItem::UserMessage(UserMessage::text("hello")).into()],
                tool_profile: ProviderToolProfile::None,
                tools: Vec::new(),
                max_tokens: None,
                reasoning_effort: ReasoningEffort::Max,
                prompt_cache_key: None,
                session_id: None,
                turn_id: None,
            },
            "test-session",
        )
        .expect("responses body renders");

        assert_eq!(body["reasoning"]["effort"], "xhigh");
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
                transcript_cache_prefix_len: None,
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
                transcript_cache_prefix_len: None,
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
                transcript_cache_prefix_len: None,
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
                transcript_cache_prefix_len: None,
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
    fn responses_body_keeps_dynamic_context_out_of_instructions_and_tail_positioned() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                transcript_cache_prefix_len: None,
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
        assert_eq!(body["input"][0]["content"][0]["text"], "hello");
        assert_eq!(body["input"][1]["content"][0]["text"], "workspace: /tmp/pi");
        assert_eq!(body["prompt_cache_key"], "cache-key");
    }

    #[test]
    fn responses_body_sorts_tools_for_cache_stability() {
        let body = responses_body(
            ModelRequest {
                model: "gpt-5.5".to_string(),
                transcript_cache_prefix_len: None,
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
                transcript_cache_prefix_len: None,
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
        assert_eq!(body["tools"][2]["name"], "cancel_delegation");
        assert_eq!(body["tools"][3]["type"], "function");
        assert_eq!(body["tools"][3]["name"], "delegate_readonly_tasks");
        assert_eq!(body["tools"][4]["type"], "function");
        assert_eq!(body["tools"][4]["name"], "delegate_writing_task");
        assert_eq!(body["tools"][5]["type"], "function");
        assert_eq!(body["tools"][5]["name"], "Grep");
        assert_eq!(body["tools"][6]["type"], "function");
        assert_eq!(body["tools"][6]["name"], "inspect_delegation");
        assert_eq!(body["tools"][7]["type"], "function");
        assert_eq!(body["tools"][7]["name"], "LoadSkill");
        assert_eq!(body["tools"][8]["type"], "function");
        assert_eq!(body["tools"][8]["name"], "steer_subagent");
        assert_eq!(body["tools"][9]["type"], "function");
        assert_eq!(body["tools"][9]["name"], "web_fetch");
        assert_eq!(body["tools"][10]["type"], "function");
        assert_eq!(body["tools"][10]["name"], "web_search");
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
    fn daemon_tool_observation_renders_as_openai_synthetic_tool_pair() {
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

        let items = transcript_to_response_items(
            &crate::PromptSections::default(),
            &[TranscriptItem::DaemonToolObservation(observation).into()],
        )
        .expect("transcript should render");

        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_delegation_1_attempt_1");
        assert_eq!(items[0]["name"], "inspect_delegation");
        assert_eq!(
            items[0]["arguments"],
            "{\"delegation_id\":\"delegation_1\"}"
        );
        assert!(items[0].get("id").is_none());
        assert!(items[0].get("status").is_none());
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_delegation_1_attempt_1");
        assert!(items[1].get("id").is_none());
        assert!(items[1].get("status").is_none());
        assert!(items[1]["output"]
            .as_str()
            .expect("json output")
            .contains("\"delegation_id\": \"delegation_1\""));
    }

    #[test]
    fn daemon_tool_observation_after_tool_result_keeps_openai_tool_pairs_adjacent() {
        let tool_call = ToolCall {
            id: ToolCallId::new("call_1"),
            tool_name: "read".to_string(),
            args_json: "{\"path\":\"README.md\"}".to_string(),
        };
        let observation = agent_vocab::DaemonToolObservation::inspect_delegation(
            ToolCallId::new("call_delegation_1_attempt_1"),
            "delegation_1",
            None,
            json!({ "delegation_id": "delegation_1", "status": "done" }),
        );

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
                TranscriptItem::DaemonToolObservation(observation).into(),
            ],
        )
        .expect("transcript should render");

        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[0]["call_id"], "call_1");
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[2]["call_id"], "call_delegation_1_attempt_1");
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[3]["call_id"], "call_delegation_1_attempt_1");
    }

    #[test]
    fn legacy_long_daemon_observation_call_ids_are_shortened_for_openai() {
        let legacy_id = "call_inspect_delegation_delegation_6d17ff90_6e46_4c3f_88ad_d92d77350d52_62847e1a_b705_48ee_899b_b062ccdf38f6";
        assert!(legacy_id.len() > OPENAI_MAX_CALL_ID_LEN);
        let observation = agent_vocab::DaemonToolObservation::inspect_delegation(
            ToolCallId::new(legacy_id),
            "delegation_6d17ff90-6e46-4c3f-88ad-d92d77350d52",
            Some("Delegation completed".to_string()),
            json!({
                "delegation_id": "delegation_6d17ff90-6e46-4c3f-88ad-d92d77350d52",
                "status": "done",
            }),
        );

        let items = transcript_to_response_items(
            &crate::PromptSections::default(),
            &[TranscriptItem::DaemonToolObservation(observation).into()],
        )
        .expect("transcript should render");

        assert_eq!(items.len(), 2);
        let call_id = items[0]["call_id"].as_str().expect("call id");
        assert!(call_id.starts_with("call_daemon_"));
        assert!(call_id.len() <= OPENAI_MAX_CALL_ID_LEN);
        assert_eq!(items[1]["call_id"], call_id);
        assert_ne!(call_id, legacy_id);
        assert_eq!(
            items[0]["arguments"],
            "{\"delegation_id\":\"delegation_6d17ff90-6e46-4c3f-88ad-d92d77350d52\"}"
        );
    }

    #[test]
    fn responses_sse_parses_text_and_tool_calls() {
        let sse = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"hello","annotations":[]}]}}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_1","name":"read","arguments":"{\"path\":\"README.md\"}"}}

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
        let sse = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"custom_tool_call","call_id":"call_patch","name":"apply_patch","input":"*** Begin Patch\n*** End Patch\n"}}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"function_call","call_id":"call_bash","name":"Bash","arguments":"{\"command\":\"pwd\",\"timeout_ms\":120000}","status":"completed"}}

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
        assert_eq!(calls[1].args_value().unwrap()["command"], "pwd");
        assert_eq!(calls[1].args_value().unwrap()["timeout_ms"], 120000);
    }

    #[test]
    fn responses_sse_requires_added_client_action_done_when_terminal_output_omits_it() {
        let sse = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","call_id":"call_1","name":"read"}}

data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}
"#;

        let error = parse_responses_sse(sse, ProviderKind::OpenAi)
            .expect_err("an added client action cannot disappear");
        assert!(error.to_string().contains("pending"), "{error}");
    }

    #[test]
    fn responses_sse_requires_added_hosted_item_done_when_terminal_output_omits_it() {
        let sse = r#"data: {"type":"response.output_item.added","output_index":0,"item":{"type":"web_search_call","id":"ws_1","status":"in_progress"}}

data: {"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[]}}
"#;

        let error = parse_responses_sse(sse, ProviderKind::OpenAi)
            .expect_err("an added hosted item cannot disappear");
        assert!(error.to_string().contains("pending"), "{error}");
    }

    #[test]
    fn responses_sse_rejects_malformed_duplicate_and_incoherent_added_lifecycles() {
        let cases = [
            (
                "missing added index",
                vec![
                    json!({
                        "type": "response.output_item.added",
                        "item": { "type": "reasoning" },
                    }),
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp_1" },
                    }),
                ],
            ),
            (
                "unknown added type",
                vec![
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": { "type": "future_action" },
                    }),
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp_1" },
                    }),
                ],
            ),
            (
                "duplicate added index",
                vec![
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": { "type": "reasoning" },
                    }),
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": { "type": "reasoning" },
                    }),
                ],
            ),
            (
                "pending added with unrelated done",
                vec![
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": { "type": "reasoning" },
                    }),
                    json!({
                        "type": "response.output_item.done",
                        "output_index": 1,
                        "item": { "type": "reasoning" },
                    }),
                    json!({
                        "type": "response.completed",
                        "response": { "id": "resp_1" },
                    }),
                ],
            ),
            (
                "added done type mismatch",
                vec![
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": { "type": "reasoning" },
                    }),
                    json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "type": "message",
                            "role": "assistant",
                            "content": [],
                        },
                    }),
                ],
            ),
            (
                "added done call id mismatch",
                vec![
                    json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": {
                            "type": "function_call",
                            "call_id": "call_1",
                            "name": "read",
                        },
                    }),
                    json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": {
                            "type": "function_call",
                            "call_id": "call_2",
                            "name": "read",
                            "arguments": "{}",
                        },
                    }),
                ],
            ),
        ];

        for (name, events) in cases {
            let sse = events
                .into_iter()
                .map(|event| format!("data: {event}\n\n"))
                .collect::<String>();
            assert!(
                parse_responses_sse(&sse, ProviderKind::OpenAi).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn responses_sse_allows_done_only_and_added_done_items_to_coexist() {
        let message = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first" }],
        });
        let added_call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
        });
        let done_call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
            "arguments": "{\"path\":\"README.md\"}",
        });
        let sse = [
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": message,
            }),
            json!({
                "type": "response.output_item.added",
                "output_index": 1,
                "item": added_call,
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": done_call,
            }),
            json!({
                "type": "response.completed",
                "response": { "id": "resp_1", "status": "completed" },
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();

        let response = parse_responses_sse(&sse, ProviderKind::OpenAi)
            .expect("per-index lifecycles reconcile independently");
        assert_eq!(response.assistant.text(), "first");
        assert_eq!(response.assistant.tool_calls().count(), 1);
        assert_eq!(response.provider_replay.len(), 2);
    }

    #[test]
    fn responses_sse_materializes_terminal_only_semantic_and_action_items() {
        let output = vec![
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "terminal text" }],
            }),
            json!({
                "type": "function_call",
                "call_id": "call_1",
                "name": "read",
                "arguments": "{\"path\":\"README.md\"}",
            }),
            json!({
                "type": "custom_tool_call",
                "call_id": "call_2",
                "name": "apply_patch",
                "input": "*** Begin Patch\n*** End Patch\n",
            }),
            json!({
                "type": "reasoning",
                "encrypted_content": "opaque-terminal-reasoning",
            }),
        ];
        let sse = format!(
            "data: {}\n\n",
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "output": output,
                },
            })
        );

        let response = parse_responses_sse(&sse, ProviderKind::OpenAi)
            .expect("supported terminal-only items materialize");
        assert_eq!(response.assistant.text(), "terminal text");
        let calls = response.assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id.as_str(), "call_1");
        assert_eq!(calls[0].tool_name, "read");
        assert_eq!(calls[1].id.as_str(), "call_2");
        assert_eq!(calls[1].tool_name, "Edit");
        assert_eq!(
            response
                .provider_replay
                .iter()
                .map(ProviderReplayItem::raw_value)
                .collect::<Result<Vec<_>, _>>()
                .unwrap(),
            output
        );
    }

    #[test]
    fn responses_sse_rejects_terminal_only_unsupported_or_unknown_actions() {
        for item in [
            json!({
                "type": "local_shell_call",
                "call_id": "call_1",
                "status": "completed",
                "action": { "type": "exec", "command": ["pwd"] },
            }),
            json!({
                "type": "future_action",
                "id": "action_1",
            }),
        ] {
            let item_type = item["type"].as_str().unwrap();
            let sse = format!(
                "data: {}\n\n",
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "output": [item],
                    },
                })
            );

            let error = parse_responses_sse(&sse, ProviderKind::OpenAi)
                .expect_err("terminal-only unsafe actions must fail closed");
            assert!(error.to_string().contains(item_type), "{error}");
        }
    }

    #[test]
    fn responses_sse_reconciles_compatible_overlap_and_keeps_exact_done_item() {
        let added = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
        });
        let done = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
            "arguments": "{\"path\":\"README.md\"}",
        });
        let terminal = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
            "arguments": "{\"path\":\"terminal-copy.md\"}",
            "status": "completed",
        });
        let sse = [
            json!({
                "type": "response.output_item.added",
                "output_index": 0,
                "item": added,
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": done,
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "output": [terminal],
                },
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();

        let response =
            parse_responses_sse(&sse, ProviderKind::OpenAi).expect("lifecycle reconciles");
        let calls = response.assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "call_1");
        assert_eq!(calls[0].args_value().unwrap()["path"], "README.md");
        assert_eq!(response.provider_replay.len(), 1);
        assert_eq!(response.provider_replay[0].raw_value().unwrap(), done);
    }

    #[test]
    fn responses_sse_rejects_terminal_overlap_type_or_stable_identity_conflicts() {
        let done = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "done" }],
        });
        for terminal in [
            json!({
                "type": "reasoning",
                "id": "msg_1",
                "encrypted_content": "opaque",
            }),
            json!({
                "type": "message",
                "id": "msg_2",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": "done" }],
            }),
        ] {
            let sse = [
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": done,
                }),
                json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "output": [terminal],
                    },
                }),
            ]
            .into_iter()
            .map(|event| format!("data: {event}\n\n"))
            .collect::<String>();

            let error = parse_responses_sse(&sse, ProviderKind::OpenAi)
                .expect_err("terminal overlap conflicts must fail");
            assert!(error.to_string().contains("changed"), "{error}");
        }
    }

    #[test]
    fn responses_state_accepts_live_derived_terminal_output_omitting_done_item() {
        // Sanitized from the private Codex SSE shape observed in a credentialed
        // daemon smoke: the done item is authoritative even though the terminal
        // response carries an output array that does not repeat it.
        let done = json!({
            "type": "message",
            "id": "msg_sanitized",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": "ok",
                "annotations": [],
            }],
        });
        let mut state = ResponsesStreamState::new(ProviderKind::OpenAi);
        assert_eq!(
            state
                .process_event(&json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": done,
                }))
                .expect("done item records"),
            SseControl::Continue
        );
        assert_eq!(
            state
                .process_event(&json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_sanitized",
                        "status": "completed",
                        "output": [],
                        "usage": {
                            "input_tokens": 1,
                            "output_tokens": 2,
                            "total_tokens": 3,
                        },
                    },
                }))
                .expect("terminal output may omit a fully received done item"),
            SseControl::Stop
        );

        assert!(state.completed);
        assert_eq!(state.output_items.len(), 1);
        assert_eq!(state.output_items.get(&0), Some(&done));
        assert!(state.materialized.is_some());
        assert_eq!(
            state.usage.as_ref().and_then(|usage| usage.total_tokens),
            Some(3)
        );

        let response = state.finish().expect("completed state finishes");
        assert_eq!(response.assistant.text(), "ok");
        assert_eq!(response.provider_replay.len(), 1);
        assert_eq!(response.provider_replay[0].raw_value().unwrap(), done);
    }

    #[test]
    fn responses_state_rejects_duplicate_terminal_identity_without_mutation() {
        let message = |text| {
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "content": [{ "type": "output_text", "text": text }],
            })
        };
        let mut state = ResponsesStreamState::new(ProviderKind::OpenAi);
        state
            .process_event(&json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": message("done"),
            }))
            .expect("done item records");
        let before = state.clone();

        let error = state
            .process_event(&json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "output": [message("terminal")],
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "total_tokens": 120,
                    },
                },
            }))
            .expect_err("the same stable item cannot occupy two output indices");

        assert!(
            error.to_string().contains("duplicated stable id"),
            "{error}"
        );
        assert_eq!(state, before, "failed reconciliation must be atomic");
        assert!(state.usage.is_none());
        assert!(!state.completed);
    }

    #[test]
    fn responses_state_rejects_unmaterializable_done_items_without_terminal_mutation() {
        let cases = [
            (
                "malformed message",
                json!({
                    "type": "message",
                    "id": "msg_1",
                    "role": "assistant",
                    "content": [{ "type": "output_text" }],
                }),
                "output_text",
            ),
            (
                "unsupported client action",
                json!({
                    "type": "local_shell_call",
                    "call_id": "call_shell",
                    "status": "completed",
                    "action": { "type": "exec", "command": ["pwd"] },
                }),
                "local_shell_call",
            ),
            (
                "malformed function arguments",
                json!({
                    "type": "function_call",
                    "call_id": "call_function",
                    "name": "Bash",
                    "arguments": { "command": "pwd" },
                }),
                "arguments",
            ),
            (
                "malformed custom input",
                json!({
                    "type": "custom_tool_call",
                    "call_id": "call_custom",
                    "name": "apply_patch",
                    "input": ["*** Begin Patch", "*** End Patch"],
                }),
                "input",
            ),
        ];

        for (name, item, expected_error) in cases {
            let mut state = ResponsesStreamState::new(ProviderKind::OpenAi);
            state
                .process_event(&json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": item,
                }))
                .expect("done item records before terminal materialization");
            let before = state.clone();

            let error = state
                .process_event(&json!({
                    "type": "response.completed",
                    "response": {
                        "id": "resp_1",
                        "status": "completed",
                        "output": [],
                        "usage": {
                            "input_tokens": 100,
                            "output_tokens": 20,
                            "total_tokens": 120,
                        },
                    },
                }))
                .expect_err(name);

            assert!(
                error.to_string().contains(expected_error),
                "{name}: {error}"
            );
            assert_eq!(state, before, "{name} terminal failure must be atomic");
            assert!(state.usage.is_none(), "{name}");
            assert!(state.materialized.is_none(), "{name}");
            assert!(!state.completed, "{name}");
        }
    }

    #[test]
    fn responses_state_rejects_sparse_terminal_candidate_without_mutation() {
        let mut state = ResponsesStreamState::new(ProviderKind::OpenAi);
        state
            .process_event(&json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": {
                    "type": "reasoning",
                    "encrypted_content": "done",
                },
            }))
            .expect("done item records");
        let before = state.clone();

        let error = state
            .process_event(&json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_1",
                        "role": "assistant",
                        "content": [{ "type": "output_text", "text": "terminal" }],
                    }],
                    "usage": {
                        "input_tokens": 100,
                        "output_tokens": 20,
                        "total_tokens": 120,
                    },
                },
            }))
            .expect_err("terminal reconciliation cannot leave a sparse candidate");

        assert!(error.to_string().contains("not contiguous"), "{error}");
        assert_eq!(state, before, "failed reconciliation must be atomic");
        assert!(state.usage.is_none());
        assert!(!state.completed);
    }

    #[test]
    fn responses_sse_orders_done_and_terminal_only_items_by_reconciled_index() {
        let message = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "content": [{ "type": "output_text", "text": "first" }],
        });
        let call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "read",
            "arguments": "{\"path\":\"README.md\"}",
        });
        let sse = [
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": call,
            }),
            json!({
                "type": "response.completed",
                "response": {
                    "id": "resp_1",
                    "status": "completed",
                    "output": [message],
                },
            }),
        ]
        .into_iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();

        let response = parse_responses_sse(&sse, ProviderKind::OpenAi)
            .expect("terminal-only item fills the done index gap");
        assert_eq!(response.assistant.text(), "first");
        let calls = response.assistant.tool_calls().collect::<Vec<_>>();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id.as_str(), "call_1");
        assert_eq!(
            response
                .provider_replay
                .iter()
                .map(ProviderReplayItem::raw_type)
                .collect::<Vec<_>>(),
            vec![
                Some("message".to_string()),
                Some("function_call".to_string())
            ]
        );
    }

    #[test]
    fn responses_sse_accepts_private_minimal_completion_without_output() {
        let response = parse_responses_sse(
            r#"data: {"type":"response.completed","response":{"id":"resp_1"}}
"#,
            ProviderKind::OpenAi,
        )
        .expect("private minimal completion remains valid");

        assert!(response.assistant.items.is_empty());
        assert!(response.provider_replay.is_empty());
    }

    #[test]
    fn responses_sse_orders_output_by_output_index_not_event_arrival() {
        let message = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": "first",
                "annotations": [],
            }],
        });
        let call = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "Bash",
            "arguments": "{\"command\":\"pwd\"}",
        });
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
            json!({
                "type": "response.output_item.done",
                "output_index": 1,
                "item": call.clone(),
            }),
            json!({
                "type": "response.output_item.done",
                "output_index": 0,
                "item": message.clone(),
            }),
        );

        let response =
            parse_responses_sse(&sse, ProviderKind::OpenAi).expect("out-of-order events parse");
        assert_eq!(
            response.assistant.items,
            vec![
                AssistantItem::Text("first".to_string()),
                AssistantItem::ToolCall(ToolCall {
                    id: ToolCallId::new("call_1"),
                    tool_name: "Bash".to_string(),
                    args_json: "{\"command\":\"pwd\"}".to_string(),
                }),
            ]
        );
        let replay = response
            .provider_replay
            .iter()
            .map(ProviderReplayItem::raw_value)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(replay, vec![message, call]);
    }

    #[test]
    fn responses_sse_rejects_duplicate_missing_invalid_and_gapped_output_indices() {
        let item = json!({ "type": "reasoning", "encrypted_content": "opaque" });
        let cases = [
            (
                "missing",
                format!(
                    "data: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                    json!({ "type": "response.output_item.done", "item": item }),
                ),
            ),
            (
                "negative",
                format!(
                    "data: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                    json!({ "type": "response.output_item.done", "output_index": -1, "item": item }),
                ),
            ),
            (
                "scalar",
                format!(
                    "data: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                    json!({ "type": "response.output_item.done", "output_index": "0", "item": item }),
                ),
            ),
            (
                "duplicate",
                format!(
                    "data: {}\n\ndata: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                    json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
                    json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
                ),
            ),
            (
                "gap",
                format!(
                    "data: {}\n\ndata: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                    json!({ "type": "response.output_item.done", "output_index": 0, "item": item }),
                    json!({ "type": "response.output_item.done", "output_index": 2, "item": item }),
                ),
            ),
        ];

        for (name, sse) in cases {
            let error = parse_responses_sse(&sse, ProviderKind::OpenAi).expect_err(name);
            assert!(
                error.to_string().contains("output_index") || error.to_string().contains("indices"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn responses_sse_rejects_missing_or_malformed_done_item() {
        for (name, item) in [
            ("missing", None),
            ("null", Some(Value::Null)),
            ("scalar", Some(json!(7))),
            ("missing type", Some(json!({}))),
            ("null type", Some(json!({ "type": null }))),
            ("empty type", Some(json!({ "type": "" }))),
        ] {
            let mut event = json!({
                "type": "response.output_item.done",
                "output_index": 0,
            });
            if let Some(item) = item {
                event["item"] = item;
            }
            let sse = format!(
                "data: {event}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n"
            );
            let error = parse_responses_sse(&sse, ProviderKind::OpenAi).expect_err(name);
            assert!(
                error.to_string().contains("item") || error.to_string().contains("type"),
                "{name}: {error}"
            );
        }
    }

    #[test]
    fn responses_sse_rejects_malformed_semantic_items() {
        let valid = json!({
            "type": "message",
            "id": "msg_1",
            "role": "assistant",
            "status": "completed",
            "content": [{
                "type": "output_text",
                "text": "hello",
                "annotations": [],
            }],
        });
        let malformed = [
            ("missing role", {
                let mut value = valid.clone();
                value.as_object_mut().unwrap().remove("role");
                value
            }),
            ("missing content", {
                let mut value = valid.clone();
                value.as_object_mut().unwrap().remove("content");
                value
            }),
            ("scalar content", {
                let mut value = valid.clone();
                value["content"] = json!("hello");
                value
            }),
            ("scalar part", {
                let mut value = valid.clone();
                value["content"] = json!([7]);
                value
            }),
            ("part missing type", {
                let mut value = valid.clone();
                value["content"] = json!([{ "text": "hello", "annotations": [] }]);
                value
            }),
            ("unknown part", {
                let mut value = valid.clone();
                value["content"] = json!([{ "type": "future_content", "text": "hello" }]);
                value
            }),
            ("output_text missing text", {
                let mut value = valid.clone();
                value["content"] = json!([{ "type": "output_text", "annotations": [] }]);
                value
            }),
            ("refusal missing text", {
                let mut value = valid.clone();
                value["content"] = json!([{ "type": "refusal" }]);
                value
            }),
            (
                "agent message missing author",
                json!({
                    "type": "agent_message",
                    "recipient": "/root",
                    "content": [],
                }),
            ),
            (
                "agent message malformed content",
                json!({
                    "type": "agent_message",
                    "author": "/root/worker",
                    "recipient": "/root",
                    "content": [{ "type": "input_text" }],
                }),
            ),
            (
                "function call missing arguments",
                json!({
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "read",
                }),
            ),
            (
                "custom tool call missing input",
                json!({
                    "type": "custom_tool_call",
                    "call_id": "call_2",
                    "name": "apply_patch",
                }),
            ),
        ];

        for (name, item) in malformed {
            let sse = format!(
                "data: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n",
                json!({
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": item,
                }),
            );
            assert!(
                parse_responses_sse(&sse, ProviderKind::OpenAi).is_err(),
                "{name}"
            );
        }
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
    fn responses_sse_rejects_incomplete_and_retains_status_and_reason() {
        let sse = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"incomplete","content":[{"type":"output_text","text":"partial","annotations":[]}]}}

data: {"type":"response.incomplete","response":{"id":"resp_1","status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":100,"output_tokens":64,"total_tokens":164,"input_tokens_details":{"cached_tokens":80}}}}
"#;

        let error = parse_responses_sse(sse, ProviderKind::OpenAi)
            .expect_err("incomplete response is not a successful assistant turn");

        match error {
            ProviderError::Incomplete { status, reason } => {
                assert_eq!(status, "incomplete");
                assert_eq!(reason, "max_output_tokens");
            }
            other => panic!("expected typed incomplete error, got {other:?}"),
        }
    }

    #[test]
    fn responses_sse_stops_at_completed_even_with_trailing_partial_frame() {
        let sse = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"message","id":"msg_1","role":"assistant","status":"completed","content":[{"type":"output_text","text":"done","annotations":[]}]}}

data: {"type":"response.completed","response":{"id":"resp_1","usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3}}}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_2","role":"assistant","status":"completed","content":[{"type":"output_text","text":"should not be parsed","annotations":[]}]}}"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi).expect("sse parses");

        assert_eq!(response.assistant.text(), "done");
        assert_eq!(response.provider_replay.len(), 1);
        let usage = response.usage.expect("usage should be parsed");
        assert_eq!(usage.total_tokens, Some(3));
    }

    #[test]
    fn responses_sse_requires_completed_not_done_or_eof() {
        for (name, suffix) in [("EOF", ""), ("done sentinel", "\ndata: [DONE]\n\n")] {
            let sse = format!(
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"message\",\"id\":\"msg_1\",\"role\":\"assistant\",\"status\":\"incomplete\",\"content\":[{{\"type\":\"output_text\",\"text\":\"partial\",\"annotations\":[]}}]}}}}\n\n{suffix}"
            );
            let error = parse_responses_sse(&sse, ProviderKind::OpenAi).expect_err(name);
            assert!(
                error.to_string().contains("before response.completed"),
                "{name}"
            );
        }
    }

    #[test]
    fn responses_sse_tolerates_unknown_events_before_completed() {
        let sse = r#"data: {"type":"response.future_progress","opaque":{"value":1}}

data: {"type":"response.completed","response":{"id":"resp_1","status":"completed"}}
"#;

        let response = parse_responses_sse(sse, ProviderKind::OpenAi)
            .expect("unknown nonterminal event is forward compatible");
        assert_eq!(response.stop_reason, ModelStopReason::Complete);
    }

    #[test]
    fn responses_sse_preserves_known_hosted_output_as_opaque_replay() {
        let hosted = json!({
            "id": "ws_1",
            "type": "web_search_call",
            "status": "completed",
            "action": { "type": "search", "query": "rust" },
        });
        let server_tool_search = json!({
            "id": "ts_1",
            "type": "tool_search_call",
            "execution": "server",
            "status": "completed",
            "arguments": { "query": "rust" },
        });
        let image_generation = json!({
            "id": "ig_1",
            "type": "image_generation_call",
            "status": "completed",
            "result": "opaque-image",
        });
        let sse = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\",\"status\":\"completed\"}}}}\n\n",
            json!({ "type": "response.output_item.done", "output_index": 0, "item": hosted }),
            json!({ "type": "response.output_item.done", "output_index": 1, "item": server_tool_search }),
            json!({ "type": "response.output_item.done", "output_index": 2, "item": image_generation }),
        );

        let response = parse_responses_sse(&sse, ProviderKind::OpenAi)
            .expect("known hosted output remains replayable");
        let replay = response
            .provider_replay
            .iter()
            .map(ProviderReplayItem::raw_value)
            .collect::<Result<Vec<_>, _>>()
            .expect("replay remains valid JSON");

        assert!(response.assistant.items.is_empty());
        assert_eq!(replay, vec![hosted, server_tool_search, image_generation]);
    }

    #[test]
    fn responses_sse_rejects_malformed_completed_event() {
        for sse in [
            "data: {\"type\":\"response.completed\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"failed\"}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":7}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":{}}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"output\":[7]}}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":\n",
        ] {
            assert!(
                parse_responses_sse(sse, ProviderKind::OpenAi).is_err(),
                "{sse}"
            );
        }
    }

    #[test]
    fn responses_sse_maps_transient_failed_events_to_status() {
        let sse = r#"data: {"type":"response.failed","response":{"error":{"code":"rate_limit_exceeded","message":"retry later"}}}
"#;

        let error = parse_responses_sse(sse, ProviderKind::OpenAi).expect_err("sse should fail");

        match &error {
            ProviderError::Status { status, message } => {
                assert_eq!(*status, 429);
                assert!(message.contains("rate_limit_exceeded"));
                assert!(message.contains("retry later"));
            }
            _ => panic!("expected status error, got {error:?}"),
        }
    }

    #[test]
    fn responses_sse_maps_unknown_failed_events_to_transient_error() {
        let sse = r#"data: {"type":"response.failed","response":{"error":{"code":"backend_restart","message":"try again"}}}
"#;

        let error = parse_responses_sse(sse, ProviderKind::OpenAi).expect_err("sse should fail");

        match &error {
            ProviderError::Transient(message) => {
                assert!(message.contains("backend_restart"));
                assert!(message.contains("try again"));
            }
            _ => panic!("expected transient error, got {error:?}"),
        }
    }

    #[test]
    fn responses_sse_refusal_discards_partial_output_and_replay() {
        let sse = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"partial-reasoning"}}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"message","id":"msg_refusal","role":"assistant","status":"completed","content":[{"type":"output_text","text":"unsafe partial","annotations":[]},{"type":"refusal","refusal":"I cannot help with that request."}]}}

data: {"type":"response.completed","response":{"id":"resp_refusal","status":"completed","usage":{"input_tokens":10,"output_tokens":2,"total_tokens":12}}}
"#;

        let response =
            parse_responses_sse(sse, ProviderKind::OpenAi).expect("refusal terminal parses");

        assert_eq!(response.stop_reason, ModelStopReason::Refusal);
        assert!(response.assistant.items.is_empty());
        assert!(response.provider_replay.is_empty());
        assert_eq!(
            response.stop_details,
            Some(ModelStopDetails {
                category: None,
                explanation: Some("I cannot help with that request.".to_string()),
            })
        );
        assert_eq!(
            response.refusal_error().as_deref(),
            Some("provider refused the request: I cannot help with that request.")
        );
        assert_eq!(
            response.usage.and_then(|usage| usage.total_tokens),
            Some(12)
        );
    }

    #[test]
    fn responses_sse_rejects_unsupported_client_executed_actions() {
        for item_type in [
            "local_shell_call",
            "shell_call",
            "apply_patch_call",
            "computer_call",
            "mcp_approval_request",
        ] {
            let sse = format!(
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"partial\"}}}}\n\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{{\"type\":\"{item_type}\"}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n"
            );
            let error = parse_responses_sse(&sse, ProviderKind::OpenAi).expect_err(item_type);
            assert!(error.to_string().contains(item_type), "{error}");
        }
        for execution in ["client", "future_execution"] {
            let sse = format!(
                "data: {{\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{{\"type\":\"reasoning\",\"summary\":[],\"encrypted_content\":\"partial\"}}}}\n\ndata: {{\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{{\"type\":\"tool_search_call\",\"execution\":\"{execution}\",\"arguments\":{{}}}}}}\n\ndata: {{\"type\":\"response.completed\",\"response\":{{\"id\":\"resp_1\"}}}}\n\n"
            );
            let error = parse_responses_sse(&sse, ProviderKind::OpenAi).expect_err(execution);
            assert!(error.to_string().contains("tool_search_call"), "{error}");
        }
        let missing_execution = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"tool_search_call"}}

data: {"type":"response.completed","response":{"id":"resp_1"}}
"#;
        let error = parse_responses_sse(missing_execution, ProviderKind::OpenAi)
            .expect_err("tool search without an execution mode must fail closed");
        assert!(error.to_string().contains("execution mode"), "{error}");

        let sse = r#"data: {"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","summary":[],"encrypted_content":"partial"}}

data: {"type":"response.output_item.done","output_index":1,"item":{"type":"future_action"}}

data: {"type":"response.completed","response":{"id":"resp_1"}}
"#;
        let error = parse_responses_sse(sse, ProviderKind::OpenAi)
            .expect_err("unknown output item types must fail closed");
        assert!(error.to_string().contains("future_action"), "{error}");
    }

    #[test]
    fn responses_input_prefers_openai_replay_sidecar() {
        let raw = json!({
            "type": "message",
            "id": "msg_1",
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
    fn responses_input_rejects_corrupt_assistant_replay() {
        let error = transcript_to_response_items(
            &PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::Text(
                        "must not replace corrupt replay".to_string(),
                    )],
                }),
                provider_replay: vec![ProviderReplayItem {
                    provider: ProviderKind::OpenAi,
                    raw_json: "{".to_string(),
                    display: None,
                }],
            }],
        )
        .expect_err("corrupt durable replay must fail closed");

        assert!(matches!(error, ProviderError::Json(_)));
    }

    #[test]
    fn responses_input_rejects_malformed_or_unknown_ordinary_replay_items() {
        for raw in [
            Value::Null,
            json!(7),
            json!({}),
            json!({ "type": null }),
            json!({ "type": 7 }),
            json!({ "type": "" }),
            json!({ "type": "future_action" }),
            json!({ "type": "reasoning_summary" }),
            json!({ "type": "compaction_summary", "encrypted_content": "opaque" }),
            json!({ "type": "shell_call" }),
            json!({
                "type": "message",
                "id": "msg_1",
                "role": "assistant",
                "status": "completed",
                "content": [{ "type": "output_text", "annotations": [] }],
            }),
        ] {
            assert!(
                transcript_to_response_items(
                    &PromptSections::default(),
                    &[ModelTranscriptEntry {
                        item: TranscriptItem::AssistantMessage(AssistantMessage {
                            items: vec![AssistantItem::Text(
                                "must not replace malformed replay".to_string(),
                            )],
                        }),
                        provider_replay: vec![
                            ProviderReplayItem::new(ProviderKind::OpenAi, &raw).unwrap()
                        ],
                    }],
                )
                .is_err(),
                "{raw}"
            );
        }
    }

    #[test]
    fn responses_input_replays_compaction_without_semantic_summary_injection() {
        let raw = json!({ "type": "compaction", "encrypted_content": "opaque" });
        let items = transcript_to_response_items(
            &crate::PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::CompactionSummary(CompactionSummary::new(
                    "session",
                    "leaf",
                    "provider summary\n\n## Delegation state at compaction time\n\n- delegation_id: `delegation_1`; status: running",
                    Some(123),
                    TurnId(3),
                )),
                provider_replay: vec![ProviderReplayItem::new(ProviderKind::OpenAi, &raw).unwrap()],
            }],
        )
        .expect("responses input renders");

        assert_eq!(items, vec![raw]);
    }

    #[test]
    fn responses_input_compaction_replay_fails_closed_when_missing_or_corrupt() {
        let summary = || {
            TranscriptItem::CompactionSummary(CompactionSummary::new(
                "session",
                "leaf",
                "semantic summary",
                Some(123),
                TurnId(3),
            ))
        };
        for provider_replay in [
            Vec::new(),
            vec![ProviderReplayItem {
                provider: ProviderKind::OpenAi,
                raw_json: "{".to_string(),
                display: None,
            }],
            vec![ProviderReplayItem::new(ProviderKind::OpenAi, &Value::Null).unwrap()],
            vec![ProviderReplayItem::new(ProviderKind::OpenAi, &json!(7)).unwrap()],
            vec![ProviderReplayItem::new(ProviderKind::OpenAi, &json!({})).unwrap()],
            vec![ProviderReplayItem::new(ProviderKind::OpenAi, &json!({ "type": null })).unwrap()],
            vec![ProviderReplayItem::new(ProviderKind::OpenAi, &json!({ "type": "" })).unwrap()],
            vec![ProviderReplayItem::new(
                ProviderKind::OpenAi,
                &json!({ "type": "message", "role": "assistant", "content": [] }),
            )
            .unwrap()],
            vec![
                ProviderReplayItem::new(
                    ProviderKind::OpenAi,
                    &json!({ "type": "compaction", "encrypted_content": "one" }),
                )
                .unwrap(),
                ProviderReplayItem::new(
                    ProviderKind::OpenAi,
                    &json!({ "type": "compaction", "encrypted_content": "two" }),
                )
                .unwrap(),
            ],
            vec![
                ProviderReplayItem::new(
                    ProviderKind::OpenAi,
                    &json!({ "type": "compaction_summary", "encrypted_content": "one" }),
                )
                .unwrap(),
                ProviderReplayItem::new(
                    ProviderKind::OpenAi,
                    &json!({ "type": "compaction_summary", "encrypted_content": "two" }),
                )
                .unwrap(),
            ],
            vec![
                ProviderReplayItem::new(
                    ProviderKind::OpenAi,
                    &json!({ "type": "compaction", "encrypted_content": "one" }),
                )
                .unwrap(),
                ProviderReplayItem::new(
                    ProviderKind::OpenAi,
                    &json!({ "type": "compaction_summary", "encrypted_content": "two" }),
                )
                .unwrap(),
            ],
        ] {
            assert!(transcript_to_response_items(
                &PromptSections::default(),
                &[ModelTranscriptEntry {
                    item: summary(),
                    provider_replay,
                }],
            )
            .is_err());
        }
    }

    #[test]
    fn responses_input_preserves_raw_replay_tool_names() {
        let raw = json!({
            "type": "function_call",
            "call_id": "call_1",
            "name": "web_search",
            "arguments": "{\"query\":\"rust\"}",
        });
        let items = transcript_to_response_items(
            &PromptSections::default(),
            &[ModelTranscriptEntry {
                item: TranscriptItem::AssistantMessage(AssistantMessage {
                    items: vec![AssistantItem::ToolCall(ToolCall {
                        id: ToolCallId::new("call_1"),
                        tool_name: "WebSearch".to_string(),
                        args_json: "{\"query\":\"rust\"}".to_string(),
                    })],
                }),
                provider_replay: vec![ProviderReplayItem::new(ProviderKind::OpenAi, &raw).unwrap()],
            }],
        )
        .expect("raw replay renders");

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
