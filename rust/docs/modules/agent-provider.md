# agent-provider

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-provider` is the model-IO boundary. It defines the `ModelProvider` trait and two adapters: `OpenAiProvider` (ChatGPT/Codex Responses API) and `AnthropicProvider` (Messages API). Each adapter turns a provider-neutral `ModelRequest` — stable prompt prefix, dynamic context, transcript items, tool definitions — into the exact wire envelope the upstream backend expects, streams the SSE response, and returns a single normalized `ModelResponse` plus the opaque per-turn replay state needed to continue the conversation later. The crate forbids `unsafe`.

See [design decisions](../design-decisions.md) for *why* the provider scope is small (two providers, ChatGPT/Codex subscription transport only, no plain OpenAI API key).

## Responsibilities

- Define the `ModelProvider` trait: `complete`, plus optional `compact` and `count_tokens`.
- Render `ModelRequest` into provider-native request bodies and headers.
- Stream and parse provider SSE into one `AssistantMessage` (`Text` / `ToolCall` items only).
- Capture per-item `ProviderReplayItem` sidecars so encrypted reasoning / thinking blocks replay verbatim on the next request.
- Map provider-native tool names to/from canonical pi-relay names.
- Classify provider errors (transient/retryable, context-overflow) for the daemon's retry and recovery logic.
- Estimate input tokens locally (`token_estimator`) for the runtime's pre-flight context gate.

It does **not** own auth acquisition/refresh, retry loops, compaction policy, or tool execution — those live in the daemon and [agent-tools](./agent-tools.md). The provider only surfaces the typed errors and capabilities the daemon acts on.

## Key types

`ModelRequest`:

- `model: String`
- `prompt: PromptSections` — `{ stable_prefix, dynamic_context }`, both `Option<String>`, trimmed to `None` when empty
- `transcript: Vec<ModelTranscriptEntry>` — each entry is a `TranscriptItem` plus its `provider_replay: Vec<ProviderReplayItem>`
- `tool_profile: ProviderToolProfile` — `None | CustomDefinitions | OpenAiCoding | AnthropicCoding`
- `tools: Vec<ProviderTool>` — empty falls back to the builtin registry for the profile
- `max_tokens: Option<u32>`
- `reasoning_effort: ReasoningEffort` — default `Medium`
- `prompt_cache_key: Option<String>` — explicit cache-cohort override
- `session_id: Option<String>` — Codex `thread_id` analog; doubles as cache cohort + routing headers
- `turn_id: Option<TurnId>` — scopes Codex sticky `x-codex-turn-state`

`ModelResponse` = `{ assistant: AssistantMessage, provider_replay, usage: Option<ProviderUsage>, stop_reason }`. `ModelStopReason` is `Complete` or `MaxOutputTokens`.

`ProviderCompactionRequest` / `ProviderCompactionResponse`, `ProviderTokenCountRequest` / `ProviderTokenCountResponse` mirror the same prompt/transcript/tool inputs for the non-`complete` methods.

`ProviderUsage` carries token counts (`input`, `output`, `total`, `cache_read_input_tokens`, `cache_creation_input_tokens`) plus OpenAI debug metadata lifted off response headers (`upstream_request_id`, `cf_ray`, `server_model`, `codex_turn_state`, `reasoning_included`).

`ProviderError` variants: `Http`, `Timeout`, `Transient`, `Provider`, `Status { status, message }`, `Json`. Two classifiers drive daemon behavior:

- `is_retryable_transient()` — true for statuses `408/409/429/500/502/503/504/529` and reqwest timeout/connect/body/decode errors, but never for a context overflow.
- `is_context_overflow()` — status `413`, or messages matching `prompt is too long` / `context_length_exceeded` / `context …(length|window|too large|exceed|maximum)`. A bare 400 is *not* treated as overflow (Anthropic `count_tokens` returns 400 for unsupported server tools).

`ProviderKind` is `{ OpenAi, Claude }` only; it parses `"openai"`, `"claude"`, and `"anthropic"`. ("codex" is not a provider kind — it is the auth transport OpenAI always uses.)

## How it works

```
ModelRequest ──┬─ OpenAiProvider ─ POST /responses (zstd, SSE)        ─┐
               │                                                       ├─ parse SSE ─ ModelResponse
               └─ AnthropicProvider ─ POST /messages (JSON, SSE)      ─┘            (AssistantMessage + provider_replay + usage)
PromptSections ─ stable_prefix (cacheable) + dynamic_context (per-request)
transcript ────  TranscriptItem + provider_replay sidecars (replay-first)
```

### Prompt sections and the stable prefix

`PromptSections` splits the prompt so the cacheable bytes come first and request-specific context comes after:

- OpenAI renders `stable_prefix` as Responses `instructions` and `dynamic_context` as the first `input` item (a synthetic user message), then transcript history.
- Anthropic renders an attribution `system[0]` header, then `stable_prefix` as a `cache_control` system block, then `dynamic_context` as an uncached system suffix. There is no model-facing "dynamic context" heading; the split is purely a cache-layout detail.

### OpenAI / Codex (Responses API)

Requests go to `https://chatgpt.com/backend-api/codex/responses`, streamed (`Accept: text/event-stream`), with the body zstd-compressed (`Content-Encoding: zstd`, level 3). The Codex request envelope is byte-for-byte aligned with the Codex CLI so the backend's routing and anti-abuse heuristics treat pi-relay like a real Codex client: `originator: codex_cli_rs`, a `codex_cli_rs/<version>` User-Agent, bearer ChatGPT token, optional `ChatGPT-Account-ID`, optional `x-codex-installation-id`, the `x-openai-internal-codex-residency: us` header, a `x-codex-window-id`, and the session id echoed across `session_id`/`session-id`/`thread_id`/`thread-id`/`x-client-request-id`.

The Responses body hardcodes the low-variance personal-use policy:

```
parallel_tool_calls = true
service_tier        = "priority"
store               = false
stream              = true
include             = ["reasoning.encrypted_content"]
tool_choice         = "auto"
reasoning.effort    = <ReasoningEffort, rejects Max>
prompt_cache_key    = <cohort key>
```

`store = false` makes every request stateless, so reasoning must be replayed from sidecars (see below). There is **no daemon-enforced output-token cap**: `max_tokens` is omitted unless the request supplies an explicit value.

`x-codex-turn-state` is sticky routing state scoped to a single `turn_id`: a value returned by an upstream request is replayed on later requests for the same turn (held in `OpenAiCodexSessionState`) and never leaks into the next turn. The `x-codex-window-id` carries a per-session window generation that bumps after compaction, mirroring Codex's "new window after compaction" signal — derived from the latest compacted turn id in the transcript when no session state is attached.

OpenAI has **no** `count_tokens` impl (the backend has no `/responses/input_tokens` route); the runtime reads `usage.input_tokens` off the `response.completed` event and otherwise falls back to reactive overflow recovery.

### Anthropic (Messages API)

Requests go to `https://api.anthropic.com/v1/messages`, streamed, authenticated with `x-api-key`, `anthropic-version: 2023-06-01`, and a Claude-Code-style User-Agent/`x-app: cli`/`X-Claude-Code-Session-Id` envelope. The `anthropic-beta` header is assembled per model from a base set plus capability betas keyed off `anthropic_capabilities(model)`: `context-management-2025-06-27` (Claude 4), `interleaved-thinking-2025-05-14` (non-adaptive Claude 4), and `effort-2025-11-24` (adaptive models: opus-4-6/4-7/4-8, sonnet-4-6).

`max_tokens` is required by the API: when the request omits it, the provider sends a `64_000` fallback. Adaptive-thinking models send `thinking: { type: "adaptive" }` and put reasoning effort in `output_config.effort` rather than the `thinking` block, because changing the `thinking` parameter invalidates Anthropic's message-content cache while `output_config` does not.

`count_tokens` is supported via `/messages/count_tokens` using the same input-shaping body minus `max_tokens` and transcript cache breakpoints.

### Prompt-cache cohort key and Anthropic cache markers

The OpenAI `prompt_cache_key` cohort is derived highest-to-lowest:

1. explicit `prompt_cache_key` override (operator config),
2. the `session_id` (one bucket per pi-relay session — matches Codex CLI `prompt_cache_key = thread_id`, keeps each session under OpenAI's ~15 RPM-per-shard ceiling while maximizing in-session prefix reuse),
3. a fresh UUID fallback for CLI/test paths with no session.

Anthropic spends its limited `cache_control` breakpoints deliberately:

- **1-hour TTL** on the stable `system` block only (stable enough to outlive the 5-minute window; 1h writes cost 2x base vs 1.25x).
- **5-minute TTL** tail breakpoint on the latest cacheable transcript block (text/tool_use/tool_result).
- **5-minute TTL** deep breakpoint placed `~18` cacheable blocks behind the tail, added only once total cacheable blocks exceed Anthropic's ~20-block lookback so long agentic sessions keep hitting older cached prefix.
- **No** tool-level breakpoint: tools are hashed in the cumulative `tools → system → messages` prefix, so the stable-system marker already covers them.

The attribution `system[0]` fingerprint is derived from the **stable prefix** (not the first user message) so sessions sharing the same system prompt share the cached prefix; it falls back to a first-user-text digest only when no stable prefix exists (e.g. compaction calls). `ProviderUsage` reports `cache_read_input_tokens` / `cache_creation_input_tokens` for both providers (OpenAI exposes only `input_tokens_details.cached_tokens`).

### Reasoning continuity (provider replay)

Because OpenAI runs stateless (`store = false`) and Anthropic preserves thinking blocks across tool calls, both adapters store every parsed output item as a `ProviderReplayItem` sidecar attached to the transcript entry. On the next request these raw blocks are replayed verbatim ahead of any synthesized representation:

- OpenAI replays the stored `reasoning` (encrypted via `reasoning.encrypted_content`), `message`, `function_call`, and `custom_tool_call` items.
- Anthropic replays the stored `thinking` / `redacted_thinking`, `text`, `tool_use`, and `server_tool_use` blocks.

When replay items exist for an assistant/compaction entry, `transcript_to_messages` / `transcript_to_response_items` emit them instead of reconstructing from `AssistantMessage`. Thinking blocks are intentionally **discarded** at the parse layer (they never become `AssistantItem`s — `AssistantItem` is `Text`/`ToolCall` only); they survive solely in the replay sidecar, keeping reasoning continuity without polluting the typed transcript.

Replay records canonicalize local client-tool names to pi-relay names (e.g. `apply_patch`/`str_replace_based_edit_tool` → `Edit`, `web_search` → `WebSearch`) but keep provider-hosted blocks (`server_tool_use`, `web_search_call`) under their native wire names so a stateless replay is byte-for-byte and the web UI can still pair hosted result blocks.

### Compaction: provider-native vs local summary

`supports_remote_compaction()` is `true` for OpenAI, `false` for Anthropic.

- **OpenAI** posts a unary `ProviderCompactionRequest` to `/responses/compact` (JSON, 20-minute timeout). The body matches Codex's `CompactionInput` — `model`, `instructions`, `input`, `tools`, `parallel_tool_calls`, `reasoning.effort`, `service_tier`, `prompt_cache_key` — and omits streaming-only fields. The response is parsed into replacement history: the opaque `compaction`/`compaction_summary` item (both wire types accepted) plus real assistant/user messages, surfaced as `ProviderCompactionResponse { summary, provider_replay, usage }`. Synthetic scaffolding messages (cwd preamble, prior compaction summaries, billing header) are dropped.
- **Anthropic** has no compaction endpoint; the daemon generates a local text summary through the normal `complete` path and stores it as a `CompactionSummary` transcript item. The Anthropic adapter never constructs or consumes `ProviderCompactionRequest`.

A `CompactionSummary` transcript item renders as a synthetic user message ("The conversation history before this point was compacted into this summary…"), with the active PI.md system prompt re-prepended when a stable prefix is present.

### Streaming, timeouts, and tool naming

`sse.rs` parses provider SSE generically: it buffers chunks, splits on `\n\n`/`\r\n\r\n` frame boundaries, collects multi-line `data:` payloads, treats `[DONE]` as terminal, and skips malformed JSON frames rather than failing the stream. `http.rs` enforces a 45-second response-headers timeout; the SSE reader enforces a 5-minute idle timeout. OpenAI parses `response.output_item.done` events (and `response.completed`/`response.failed`/`response.incomplete` for terminal handling); Anthropic assembles `content_block_start`/`_delta`/`_stop` events, accumulating streamed `input_json_delta`/`text_delta`/`thinking_delta`/`signature_delta` per block.

Tool-name mapping is centralized: `canonical_tool_name_for_provider` maps wire → pi-relay names; `openai_wire_tool_name` / `anthropic_wire_tool_name` map back. `transcript.rs::normalize_transcript_for_provider` canonicalizes historical tool-call names to the entry's recorded provider and bounds historical tool-result output via `agent-tools::limit_tool_output`.

### Token estimation

`token_estimator.rs` serializes each prompt section and transcript entry to its provider wire JSON and approximates tokens as `ceil(bytes / 4)`. Entries with replay sidecars are estimated from the raw replay JSON. Base64 image data URLs are discounted to a fixed `~7373`-byte resized-image estimate (Codex-style) so large inline images don't inflate the count.

## Notes

- `ReasoningEffort::default()` is `Medium`. OpenAI rejects `Max`. Anthropic rejects `None`/`Minimal`; it maps `XHigh` down to `high` unless the model is opus-4-7/4-8.
- The single Codex **401 token-refresh retry** is *not* in this crate. The daemon (`provider_runtime/auth_retry.rs`) wraps `complete`/`compact`/`count_tokens`: on a 401 from a Codex-auth provider it refreshes credentials once, rebuilds the provider, and retries exactly once. The provider only surfaces `ProviderError::Status { status: 401 }`.
- Registered builtin tools (from [agent-tools](./agent-tools.md)): `edit` (`apply_patch` for OpenAI, `text_editor_20250728` for Anthropic), `bash` (uniform JSON `Bash`), `grep` (uniform JSON `Grep`), `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`). There are no `read`/`write` tools.
- Sending OpenAI-profile tools to Anthropic (or vice versa) is a hard `ProviderError::Provider`; the profile must match the provider.
- Wire details (RPC methods, how the daemon calls these adapters) live in [websocket-rpc](../websocket-rpc.md); the React client that drives sessions is documented in the [web UI](../../../packages/web/docs/web-ui.md) doc.
