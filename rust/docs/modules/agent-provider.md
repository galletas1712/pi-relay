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
- Surface provider errors with diagnostics, including context-overflow classification for the daemon's recovery logic.
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

`ModelResponse` = `{ assistant: AssistantMessage, provider_replay, usage: Option<ProviderUsage>, stop_reason, stop_details }`. `ModelStopReason` is `Complete`, `MaxOutputTokens`, `Refusal`, or `Compaction`. `Compaction` is valid only for the special paused Anthropic compact call; an ordinary model turn rejects it. Refusal details retain the optional provider category and human-readable explanation.

`ProviderCompactionRequest` / `ProviderCompactionResponse`, `ProviderTokenCountRequest` / `ProviderTokenCountResponse` mirror the same prompt/transcript/tool inputs for the non-`complete` methods. Compaction requests also carry optional provider-native custom instructions; token-count responses can retain provider-reported original input occupancy.

`ProviderUsage` carries token counts (`input`, `output`, `total`, `cache_read_input_tokens`, `cache_creation_input_tokens`), provider-neutral raw provider usage JSON, and OpenAI debug metadata lifted off response headers (`upstream_request_id`, `cf_ray`, `server_model`, `codex_turn_state`, `reasoning_included`).

`ProviderError` variants: `Http`, `Timeout`, `Transient`, `Provider`, `Status { status, message }`, `Json`, and typed `NativeCompaction`. The daemon's model-dispatch loop retries every ordinary-turn `ProviderError` up to five attempts; the provider crate does not classify status codes as retryable or non-retryable.

- `is_context_overflow()` — status `413`, or messages matching `prompt is too long` / `context_length_exceeded` / `context …(length|window|too large|exceed|maximum)`. A bare 400 is *not* treated as overflow (Anthropic `count_tokens` returns 400 for unsupported server tools).
- `retry_diagnostic()` — returns status / timeout / reqwest diagnostic details that the daemon records after retry exhaustion.

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

- OpenAI renders `stable_prefix` as Responses `instructions`, then transcript history, then `dynamic_context` as a final synthetic user input when present.
- Anthropic renders an attribution `system[0]` header, then `stable_prefix` as a `cache_control` system block. Transcript messages come next; `dynamic_context`, when present, is appended as a final uncached user message so the stable/system and transcript prefix remain cacheable.

Normal daemon model requests usually leave `dynamic_context` empty. Parent
delegation preservation is handled after provider compaction returns: the daemon
appends `## Delegation state at compaction time` to the stored compaction
summary for top-level parent sessions only. The compaction provider input does
not receive live parent/sibling delegation state, and subagent compactions do
not append it afterward.

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

Requests go to `https://api.anthropic.com/v1/messages`, streamed, authenticated with `x-api-key`, `anthropic-version: 2023-06-01`, and a Claude-Code-style User-Agent/`x-app: cli`/`X-Claude-Code-Session-Id` envelope. The only unconditional `anthropic-beta` value is the existing `claude-code-20250219` identity header required by that transport. Effort, one-hour cache TTL, fine-grained tool streaming, text editor, and the current hosted web tools are GA and do not send their retired beta headers. Any future beta header must be added only with the beta body/tool that needs it.

The provider retrieves model metadata from `GET /v1/models/{model_id}` through custom `reqwest` code (there is no official Rust SDK in this stack). Models GETs use the documented API version and credentials but do not copy the Messages-only Claude Code beta header. A process-wide cache shared by all reconstructed Anthropic provider handles holds at most 64 settled model ids and coalesces each model's refresh into one in-flight GET without holding the cache mutex during network I/O. In-flight entries are never evicted; if all eviction candidates are refreshing, they may temporarily exceed the bound until completion trims settled entries back to 64. Successful metadata is fresh for six hours. A refresh failure preserves stale last-known-good metadata and starts a separate one-minute retry backoff; the same backoff is a negative cache only when that model has never had a successful value. API-reported `max_input_tokens`, `max_tokens`, effort levels, and adaptive-thinking support shape requests and the daemon's proactive compaction threshold. `capabilities: null` still preserves authoritative token limits, and an authoritative `effort.xhigh: null` disables xhigh rather than inheriting static support. Static metadata keeps known options safe and available when discovery fails: Sonnet 5 and Fable 5 are 1M-input/128K-output models, as are the retained Opus 4.8/4.7 entries. Unknown models conservatively retain the old 64K output ceiling and no assumed input window/capabilities; without a resolved input window they receive no automatic compaction threshold, and unsupported request fields remain disabled.

`max_tokens` is required by the Messages API. Explicit session limits are clamped to the discovered/static model ceiling. When a session has no explicit limit, pi-relay requests `min(64_000, model ceiling)`: this preserves the existing ordinary-turn budget instead of unexpectedly asking every 128K-capable model for its pathological maximum, while still respecting lower limits reported by the API.

Claude Sonnet 5 and Fable 5 default to adaptive thinking, so their bodies omit the redundant `thinking` field and put the selected `low…max` value in `output_config.effort`; Fable's adaptive thinking cannot be disabled. Opus 4.8 supports the same effort range but requires the explicit `thinking: { type: "adaptive" }` body. The adapter does not generate the legacy manual `enabled`/`budget_tokens` form for these model families.

Fable can return HTTP 200 with `stop_reason: "refusal"` before output or after streaming partial text/tool/replay blocks. The provider returns a refusal-aware terminal result, discards all partial assistant content and replay, and retains nullable `stop_details.category`/`explanation`. The daemon records the action as an error and surfaces that explanation instead of persisting an assistant completion. It does not automatically retry or switch models.

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

The attribution `system[0]` fingerprint is derived from the **stable prefix** (not the first user message) so sessions sharing the same system prompt share the cached prefix; it falls back to a first-user-text digest only for callers that supply no stable prefix. Both daemon compaction paths supply the stable system-prompt section. `ProviderUsage` reports `cache_read_input_tokens` / `cache_creation_input_tokens` for both providers (OpenAI exposes only `input_tokens_details.cached_tokens`).

### Reasoning continuity (provider replay)

Because OpenAI runs stateless (`store = false`) and Anthropic preserves thinking blocks across tool calls, both adapters store every parsed output item as a `ProviderReplayItem` sidecar attached to the transcript entry. On the next request these raw blocks are replayed verbatim ahead of any synthesized representation:

- OpenAI replays the stored `reasoning` (encrypted via `reasoning.encrypted_content`), `message`, `function_call`, and `custom_tool_call` items.
- Anthropic replays the stored `thinking` / `redacted_thinking`, `text`, `tool_use`, and `server_tool_use` blocks.

When replay items exist for an assistant/compaction entry, `transcript_to_messages` / `transcript_to_response_items` emit them instead of reconstructing from `AssistantMessage`. Thinking blocks are intentionally **discarded** at the parse layer (they never become `AssistantItem`s — `AssistantItem` is `Text`/`ToolCall` only); they survive solely in the replay sidecar, keeping reasoning continuity without polluting the typed transcript.

Replay records canonicalize local client-tool names to pi-relay names (e.g. `apply_patch`/`str_replace_based_edit_tool` → `Edit`, `web_search` → `WebSearch`) but keep provider-hosted blocks (`server_tool_use`, `web_search_call`) under their native wire names so a stateless replay is byte-for-byte and the web UI can still pair hosted result blocks.

Daemon-authored observations, such as delegation completion wakeups carrying an
`inspect_delegation`-equivalent snapshot, are not provider replay and are not
stored as fake assistant choices. The durable transcript item is
`daemon_tool_observation`; provider adapters synthesize a tool call/result pair
only while building a request:

- OpenAI Responses receives adjacent `function_call` and
  `function_call_output` items using the item's stable local `call_id`. The
  synthetic call omits provider-generated-looking `id` and `status` fields.
- Anthropic Messages receives an adjacent assistant `tool_use` message and user
  `tool_result` message. Non-`toolu_...` internal ids are deterministically
  adapted to Anthropic-style ids.

Request-shape tests pin the adjacency rules and ensure ordinary assistant tool
pairs are not split. The model sees an `inspect_delegation` result in the same
shape as a real tool result, while the transcript/UI semantics remain explicit:
the daemon authored the observation.

### Provider-native compaction

Both provider adapters implement the required `compact` trait method, and the
daemon uses it for every manual and automatic compaction. Anthropic also
validates the selected model before constructing the compact request; adapter
support alone does not imply that every Claude model supports native
compaction.

- **OpenAI** posts a unary `ProviderCompactionRequest` to `/responses/compact` (JSON, 20-minute timeout). The body matches Codex's `CompactionInput` — `model`, `instructions`, `input`, `tools`, `parallel_tool_calls`, `reasoning.effort`, `service_tier`, `prompt_cache_key` — and omits streaming-only fields. The response is parsed into replacement history: the opaque `compaction`/`compaction_summary` item (both wire types accepted) plus real assistant/user messages, surfaced as `ProviderCompactionResponse { summary, provider_replay, usage }`. Synthetic scaffolding messages (cwd preamble, prior compaction summaries, billing header) are dropped.
- **Anthropic** uses the Messages API with the provider-required beta `compact-2026-01-12`, `context_management.edits[0].type = "compact_20260112"`, the minimum valid 50K input-token trigger, and `pause_after_compaction = true`. It supplies the PI compaction prompt as replacement custom instructions, explicitly forbids tool calls, and supplies no tools. Because Anthropic rejects an assistant prefill for this operation, the adapter appends one minimal synthetic user instruction when the rendered transcript is assistant-ended or empty. For an assistant tool-use tail, it normalizes the complete following user run once: real results are retained in tool-use order, only missing results are synthesized, then all non-result user content follows in original order; duplicates and redundant empty user messages are dropped. The authoritative custom instructions remain in the context-management edit. Static support is limited to the documented model ids `claude-fable-5`, `claude-mythos-5`, `claude-mythos-preview`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-5`, and `claude-sonnet-4-6`; when Models API metadata includes `capabilities.context_management.compact_20260112`, that authoritative value overrides the static fallback. This static metadata remains necessary when authoritative model metadata is unavailable. Unknown and known-unsupported ids return a typed terminal native-compaction `unsupported` error before network dispatch. The compact call accepts only an eventual terminal `stop_reason = "compaction"` with one index-zero compaction block whose `content` is a non-null/non-empty string; ordinary completion, tools, refusal, max tokens, malformed/truncated streams, and missing/null/empty content are typed native-compaction errors.

The Anthropic compaction block is stored as opaque provider replay on the new `CompactionSummary` root. One strict rule defines valid replay: `type` is `compaction`, `content` is a nonempty string, and `encrypted_content` is absent, null, or a string. `content` is copied from the compaction delta; opaque encryption and all start/delta extension fields (including a top-level `name`) are retained unchanged. No cache fields or daemon metadata are injected into the provider block. Request rendering parses every Claude sidecar on the only transcript kinds that can emit it, `CompactionSummary` and `AssistantMessage`. A replay-free historical summary remains renderable; a summary with Claude replay must have exactly one valid compaction block. Assistant replay may contain ordinary Claude block types, but corrupt JSON and malformed exact compaction blocks fail locally. Wrong-provider and non-emitted sidecars have no effect.

Subsequent Messages and token-count requests replay the complete block unchanged as an assistant message; the synthetic user summary follows it and carries pi-relay's generic checkpoint label plus the daemon's fresh delegation ledger. Rendering prepares the body and required beta header together, so replay is not rescanned for headers. The two request types intentionally use different strategy shapes. Ordinary Messages sets `trigger = { type: "input_tokens", value: <resolved model max_input_tokens> }` (clamped to the documented 50K minimum) plus `pause_after_compaction = true`. Anthropic documents only the 50K minimum, so the exact model ceiling is schema-valid. A paid production Sonnet 5 automatic E2E accepted this replay shape, resumed the same blocked model action after one native checkpoint, returned the exact requested sentinel JSON, and reduced the effective count from the 541,564-token gate to 15,628 tokens. This proves the tested continuation path, not provider-generated inline compaction during an ordinary response. Anthropic has no documented apply-only mode, so the paused ceiling trigger plus fail-closed parsing is required to protect durable state while pi-relay schedules normal replacement checkpoints at its lower threshold. A model with no safely resolved input ceiling fails replay request construction locally. Token counting retains the live-proven bare `[{ "type": "compact_20260112" }]` edit because Anthropic documents that counting applies existing blocks but does not trigger new compactions. Any compaction block, delta, or compaction stop returned by an ordinary Messages call—including non-paused compaction followed by text/tool use and paused/null/malformed forms—is rejected at the provider parser boundary and cannot become successful persistable replay. Ordinary pre-compaction calls include neither the compaction beta nor the edit. Token counting uses returned `input_tokens` as effective occupancy and retains `context_management.original_input_tokens` as diagnostics.

`ProviderUsage` keeps normalized top-level counts and `raw_provider_usage`. For Anthropic, normalized totals use only top-level message counts because those exclude compaction iterations. The raw object retains `usage.iterations`, nested cache-creation TTL fields, and `output_tokens_details.thinking_tokens` for billing/audit without double counting.

### Streaming, timeouts, and tool naming

`sse.rs` parses provider SSE generically: it buffers chunks, splits on `\n\n`/`\r\n\r\n` frame boundaries, collects multi-line `data:` payloads, treats `[DONE]` as terminal, and reports malformed JSON frames to the adapter. Ordinary OpenAI/Anthropic generation retains the legacy behavior of skipping malformed JSON frames; the special Anthropic compact call uses separate strict state and requires `message_start`, one index-zero compaction `content_block_start`, one matching `compaction_delta`, matching `content_block_stop`, one or more `message_delta` frames with exactly one eventual `stop_reason = "compaction"`, and final `message_stop`. Pings and unknown future event types are ignored without advancing structural state. The parser consumes through EOF so duplicate/conflicting terminal reasons and trailing known frames are observable, and rejects missing fields, wrong indices/types/order, multiple or mixed blocks, pre-populated start content, malformed JSON, `[DONE]`, and truncation. `http.rs` enforces a 45-second response-headers timeout; the SSE reader enforces a 5-minute idle timeout. The ordinary parser assembles `content_block_start`/`_delta`/`_stop` events and accumulates streamed `input_json_delta`/`text_delta`/`thinking_delta`/`signature_delta`, but defensively rejects all compaction content/deltas/stops before producing a `ModelResponse`.

Tool-name mapping is centralized: `canonical_tool_name_for_provider` maps wire → pi-relay names; `openai_wire_tool_name` / `anthropic_wire_tool_name` map back. `transcript.rs::normalize_transcript_for_provider` canonicalizes historical tool-call names to the entry's recorded provider and bounds historical tool-result output via `agent-tools::limit_tool_output`.

### Token estimation

`token_estimator.rs` serializes each prompt section and transcript entry to its provider wire JSON and approximates tokens as `ceil(bytes / 4)`. Entries with replay sidecars are estimated from the raw replay JSON. Base64 image data URLs are discounted to a fixed `~7373`-byte resized-image estimate (Codex-style) so large inline images don't inflate the count.

## Notes

- `ReasoningEffort::default()` is `Medium`. OpenAI accepts `Max` for `gpt-5.6-sol`, `gpt-5.6-terra`, and `gpt-5.6-luna`; older gpt-5.x models clamp it to `xhigh`. Anthropic rejects `None`/`Minimal`; Models API capability metadata determines whether the selected `low…max` effort is emitted. Sonnet 5, Fable 5, and Opus 4.8 support the full range.
- Claude Fable 5 is intentionally an explicit opt-in UI choice: Anthropic requires 30-day data retention for Fable and does not offer Zero Data Retention for it. Do not silently select Fable for a ZDR workload.
- The single Codex **401 token-refresh retry** is *not* in this crate. The daemon (`provider_runtime/auth_retry.rs`) wraps `complete`/`compact`/`count_tokens`: on a 401 from a Codex-auth provider it refreshes credentials once, rebuilds the provider, and retries exactly once inside that provider call. The provider only surfaces `ProviderError::Status { status: 401 }`.
- Registered builtin tools (from [agent-tools](./agent-tools.md)): `edit` (`apply_patch` for OpenAI, `text_editor_20250728` for Anthropic), `bash` (uniform JSON `Bash`), `grep` (uniform JSON `Grep`), `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`). There are no `read`/`write` tools.
- Sending OpenAI-profile tools to Anthropic (or vice versa) is a hard `ProviderError::Provider`; the profile must match the provider.
- Wire details (RPC methods, how the daemon calls these adapters) live in [websocket-rpc](../websocket-rpc.md); the React client that drives sessions is documented in the [web UI](../../../packages/web/docs/web-ui.md) doc.
