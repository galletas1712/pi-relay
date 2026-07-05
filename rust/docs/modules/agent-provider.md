# agent-provider

> Part of the [Rust Agent Stack](../architecture.md) | [Provider API support](../provider-api-support.md) | [Design decisions](../design-decisions.md)

`agent-provider` is the model-IO boundary. It defines the `ModelProvider` trait and two adapters: `OpenAiProvider` (the private ChatGPT/Codex Responses-compatible backend, not public OpenAI API-key transport) and `AnthropicProvider` (Messages API). Each adapter turns a provider-neutral `ModelRequest` — stable prompt prefix, dynamic context, transcript items, tool definitions — into the exact wire envelope the upstream backend expects, streams the SSE response, and returns a single normalized `ModelResponse` plus the opaque per-turn replay state needed to continue the conversation later. The crate forbids `unsafe`.

See [design decisions](../design-decisions.md) for *why* the provider scope is small (two providers, ChatGPT/Codex subscription transport only, no plain OpenAI API key).

## Responsibilities

- Define the `ModelProvider` trait: `complete`, plus optional `compact` and `count_tokens`.
- Render `ModelRequest` into provider-native request bodies and headers.
- Stream and parse provider SSE into one `AssistantMessage` (`Text` / `ToolCall` items only).
- Capture per-item `ProviderReplayItem` sidecars so encrypted reasoning / thinking blocks replay verbatim on the next request.
- Map provider-native tool names to canonical pi-relay names only in semantic
  transcript/UI projections; opaque replay retains provider wire names.
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
- `max_tokens: Option<u32>` — emitted as OpenAI `max_output_tokens` when set;
  omitted when unset
- `reasoning_effort: ReasoningEffort` — default `Medium`
- `prompt_cache_key: Option<String>` — explicit cache-cohort override
- `session_id: Option<String>` — Codex `thread_id` analog; doubles as cache cohort + routing headers
- `turn_id: Option<TurnId>` — scopes Codex sticky `x-codex-turn-state`

`ModelResponse` = `{ assistant: AssistantMessage, provider_replay, usage:
Option<ProviderUsage>, stop_reason, stop_details }`. `ModelStopReason` is
`Complete`, `MaxOutputTokens`, or `Refusal`. Refusal details retain the optional
provider category and human-readable explanation.

`ProviderCompactionRequest` / `ProviderCompactionResponse`, `ProviderTokenCountRequest` / `ProviderTokenCountResponse` mirror the same prompt/transcript/tool inputs for the non-`complete` methods.

`ProviderUsage` carries token counts (`input`, `output`, `total`, `cache_read_input_tokens`, `cache_creation_input_tokens`), provider-native merged usage fields, and OpenAI debug metadata lifted off response headers (`upstream_request_id`, `cf_ray`, `server_model`, `codex_turn_state`, `reasoning_included`).

`ProviderError` variants: `Http`, `Timeout`, `Transient`, `Provider`,
`Status { status, message }`, `Incomplete { status, reason }`, and `Json`. The
daemon's model-dispatch loop retries every ordinary-turn `ProviderError` up to
five attempts; the provider crate does not classify status codes as retryable
or non-retryable.

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
reasoning.effort    = <model-normalized ReasoningEffort>
prompt_cache_key    = <cohort key>
```

`store = false` makes every request stateless, so reasoning must be replayed from sidecars (see below). There is **no daemon-enforced output-token cap**: `max_output_tokens` is omitted unless `ModelRequest.max_tokens` supplies an explicit value.

`x-codex-turn-state` is sticky routing state scoped to a single `turn_id`: a value returned by an upstream request is replayed on later requests for the same turn (held in `OpenAiCodexSessionState`) and never leaks into the next turn. The `x-codex-window-id` carries a per-session window generation that bumps after compaction, mirroring Codex's "new window after compaction" signal — derived from the latest compacted turn id in the transcript when no session state is attached.

OpenAI has **no** `count_tokens` impl. The public API has a
`/responses/input_tokens` route, but the private backend route returned a
Cloudflare 403 challenge in the audited probe. The runtime reads
`usage.input_tokens` off `response.completed` and otherwise falls back to
reactive overflow recovery.

### Anthropic (Messages API)

Requests go to `https://api.anthropic.com/v1/messages`, streamed, authenticated with `x-api-key`, `anthropic-version: 2023-06-01`, and a Claude-Code-style User-Agent/`x-app: cli`/`X-Claude-Code-Session-Id` envelope. The only unconditional `anthropic-beta` value is the existing `claude-code-20250219` identity header required by that transport. Effort, one-hour cache TTL, fine-grained tool streaming, text editor, and the current hosted web tools are GA and do not send their retired beta headers. Any future beta header must be added only with the beta body/tool that needs it.

The provider retrieves model metadata from `GET /v1/models/{model_id}` through custom `reqwest` code (there is no official Rust SDK in this stack). Models GETs use the documented API version and credentials but do not copy the Messages-only Claude Code beta header. A process-wide cache shared by all reconstructed Anthropic provider handles holds at most 64 settled model ids and coalesces each model's refresh into one in-flight GET without holding the cache mutex during network I/O. In-flight entries are never evicted; if all eviction candidates are refreshing, they may temporarily exceed the bound until completion trims settled entries back to 64. Successful metadata is fresh for six hours. A refresh failure preserves stale last-known-good metadata and starts a separate one-minute retry backoff; the same backoff is a negative cache only when that model has never had a successful value. API-reported `max_input_tokens`, `max_tokens`, effort levels, and adaptive-thinking support shape requests and the daemon's proactive compaction threshold. `capabilities: null` still preserves authoritative token limits, and an authoritative `effort.xhigh: null` disables xhigh rather than inheriting static support. Static metadata keeps known options safe and available when discovery fails: Sonnet 5 and Fable 5 are 1M-input/128K-output models, as are the retained Opus 4.8/4.7 entries. Unknown models conservatively retain the old 64K output ceiling and no assumed input window/capabilities, so they cannot silently disable a known model's compaction fallback or receive unsupported request fields.

`max_tokens` is required by the Messages API. Explicit session limits are clamped to the discovered/static model ceiling. When a session has no explicit limit, pi-relay requests `min(64_000, model ceiling)`: this preserves the existing ordinary-turn budget instead of unexpectedly asking every 128K-capable model for its pathological maximum, while still respecting lower limits reported by the API.

Claude Sonnet 5 and Fable 5 default to adaptive thinking, so their bodies omit the redundant `thinking` field and put the selected `low…max` value in `output_config.effort`; Fable's adaptive thinking cannot be disabled. Opus 4.8 supports the same effort range but requires the explicit `thinking: { type: "adaptive" }` body. The adapter does not generate the legacy manual `enabled`/`budget_tokens` form for these model families.

Fable can return HTTP 200 with `stop_reason: "refusal"` before output or after
streaming partial text/tool/replay blocks. The provider returns a refusal-aware
terminal result, discards all partial assistant content and replay, and retains
nullable `stop_details.category`/`explanation`. The daemon records the action as
an error and surfaces that explanation instead of persisting an assistant
completion. It does not automatically retry or switch models.

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

Because OpenAI runs stateless (`store = false`) and Anthropic preserves thinking blocks across tool calls, both adapters store every accepted output item as a `ProviderReplayItem` sidecar attached to the transcript entry. On the next request these raw blocks are replayed verbatim ahead of any synthesized representation:

- OpenAI strictly validates semantic message/tool-call shapes and fails closed
  on known unsupported client actions or unknown ordinary item types. Known
  provider-hosted/passive items validate only the stable classification
  boundary (object plus nonempty `type`) and otherwise replay unchanged.
  Canonical `/responses/compact` items remain opaque after the same minimum
  shape check and exactly one native-checkpoint invariant.
- Anthropic replays the stored `thinking` / `redacted_thinking`, `text`, `tool_use`, and `server_tool_use` blocks.

When replay items exist for an assistant/compaction entry, `transcript_to_messages` / `transcript_to_response_items` emit them instead of reconstructing from `AssistantMessage`. Thinking blocks are intentionally **discarded** at the parse layer (they never become `AssistantItem`s — `AssistantItem` is `Text`/`ToolCall` only); they survive solely in the replay sidecar, keeping reasoning continuity without polluting the typed transcript.

Provider replay is provider-filtered and parsed for request construction, but
its JSON values and provider order are otherwise unchanged. This includes local
client-tool names such as `apply_patch` and
`str_replace_based_edit_tool`, hosted blocks such as `server_tool_use` and
`web_search_call`, and opaque extension items inside canonical OpenAI compact
output. Ordinary OpenAI output/replay uses an explicit known-item allowlist;
unknown types fail closed because the adapter cannot assume that a future item
requires no client response. Tool-name canonicalization is confined to the
separate semantic transcript/UI projection. Corrupt raw replay fails request
construction rather than being silently dropped.

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

### Compaction: provider-native vs local summary

`supports_remote_compaction()` is `true` for OpenAI, `false` for Anthropic.

- **OpenAI** posts a unary `ProviderCompactionRequest` to `/responses/compact`
  (JSON, 20-minute timeout). The body is a valid subset of Codex's current
  `CompactionInput` —
  `model`, `instructions`, `input`, `tools`, `parallel_tool_calls`,
  `reasoning.effort`, `service_tier`, `prompt_cache_key` — and omits
  streaming-only fields. Codex also forwards optional `text` controls; pi-relay
  has no model verbosity/text control yet, so it has no such value to forward.
  The complete returned `output` array is canonical
  replacement history: every item is retained unchanged and in provider order,
  including user/developer messages, reasoning, tool/hosted-tool items, and
  unknown extensions. Every item must be an object with a nonempty string
  `type`, and the response must contain exactly one native checkpoint across
  `type = "compaction"` and the currently evidenced Codex schema alias
  `compaction_summary`. Checkpoint and extension payloads are opaque; the alias
  is retained without rewriting. Assistant text may be projected separately as
  display summary, but is never substituted into replay. Missing, corrupt,
  duplicate, or non-native replay on a persisted OpenAI `CompactionSummary`
  fails request construction rather than synthesizing a user summary.
- **Anthropic** has no compaction endpoint at this boundary; the daemon generates
  a local text summary through the normal `complete` path and stores it as a
  `CompactionSummary` transcript item. The Anthropic adapter never constructs
  or consumes `ProviderCompactionRequest`.

A local `CompactionSummary` renders as a synthetic user message ("The
conversation history before this point was compacted into this summary…"), with
the active PI.md system prompt re-prepended when a stable prefix is present.

`ProviderUsage` keeps normalized counts and provider-native merged usage fields
in `raw_provider_usage`. For Anthropic, normalized input is raw
`input_tokens + cache_read_input_tokens + cache_creation_input_tokens`, and
normalized total adds output tokens. Raw fields retain provider extensions for
billing and audit without double counting.

### Streaming, timeouts, and tool naming

`sse.rs` parses provider SSE generically: it buffers chunks, splits on
`\n\n`/`\r\n\r\n` frame boundaries, collects multi-line `data:` payloads, and
reports `[DONE]` and malformed JSON frames to the adapter. Ordinary OpenAI
success requires `response.completed`; `response.incomplete` is a typed
non-success retaining status/reason, and refusal content produces a refusal
terminal that discards partial assistant output/replay. Output-item `added`
events, when present, must be followed by matching `done` events at the same
unique index and type; no item may remain pending at the terminal. Done-only
private streams remain compatible. Terminal `response.output`, when present,
may omit fully received done items. Done items remain authoritative; terminal
overlaps must have compatible type and stable identity, while terminal-only
items cross the ordinary item safety boundary and fill their terminal array
indices. The reconciled output indices are contiguous and materialized in
provider order rather than event-arrival order. Known messages and content
parts are shape-validated. Supported
`function_call`/`custom_tool_call` items become pi-relay tool calls; unsupported
client actions and arbitrary unknown output item types fail closed. Known
hosted/passive item types remain opaque replay.

Ordinary Anthropic success requires `message_stop` after one or more
`message_delta` events eventually provide an explicit `end_turn`,
`stop_sequence`, or `tool_use` reason. Cumulative usage and compatible stop
details merge across deltas; a missing/null stop reason is nonterminal.
Conflicting terminal reasons/details and content events after a terminal reason
fail closed. `max_tokens` and `refusal` retain their existing typed terminal
behavior. `pause_turn`, `model_context_window_exceeded`, and unknown nonempty
stop reasons are non-successes retaining the provider reason; partial assistant
output/replay is not returned. For both providers, EOF or `[DONE]` alone and
malformed JSON are failures. Unknown future event types remain ignorable but
never imply success.

`http.rs` enforces a 45-second response-headers timeout; the SSE reader enforces
a 5-minute idle timeout. The ordinary Anthropic parser requires
contiguous, checked content-block indices and an explicit `content_block_stop`
for every start. It validates the known start/delta schemas, rejects duplicate,
gapped, nonexistent, or mismatched block transitions, and fails malformed
accumulated tool-input JSON instead of substituting arguments. It accumulates valid
`input_json_delta`/`text_delta`/`thinking_delta`/`signature_delta` content and
defensively rejects all compaction content/deltas/stops before producing a
`ModelResponse`.

Tool-name mapping is centralized: `canonical_tool_name_for_provider` maps wire → pi-relay names; `openai_wire_tool_name` / `anthropic_wire_tool_name` map back. `transcript.rs::normalize_transcript_for_provider` canonicalizes historical tool-call names to the entry's recorded provider and bounds historical tool-result output via `agent-tools::limit_tool_output`.

### Token estimation

`token_estimator.rs` serializes each prompt section and transcript entry to its provider wire JSON and approximates tokens as `ceil(bytes / 4)`. Entries with replay sidecars are estimated from the exact serialized raw replay JSON. Replay rendering/serialization errors propagate instead of being masked as zero tokens. Base64 image data URLs are discounted to a fixed `~7373`-byte resized-image estimate (Codex-style) so large inline images don't inflate the count.

## Notes

- `ReasoningEffort::default()` is `Medium`. OpenAI accepts `Max` only for `gpt-5.6-sol`; other gpt-5.x models clamp it to `xhigh`. Anthropic normalizes legacy `None`/`Minimal` configuration to `low`; Models API capability metadata determines whether the selected `low…max` effort is emitted. Sonnet 5, Fable 5, and Opus 4.8 support the full range.
- Claude Fable 5 is intentionally an explicit opt-in UI choice: Anthropic requires 30-day data retention for Fable and does not offer Zero Data Retention for it. Do not silently select Fable for a ZDR workload.
- The single Codex **401 token-refresh retry** is *not* in this crate. The daemon (`provider_runtime/auth_retry.rs`) wraps `complete`/`compact`/`count_tokens`: on a 401 from a Codex-auth provider it refreshes credentials once, rebuilds the provider, and retries exactly once inside that provider call. The provider only surfaces `ProviderError::Status { status: 401 }`.
- Registered builtin tools (from [agent-tools](./agent-tools.md)): `edit` (`apply_patch` for OpenAI, `text_editor_20250728` for Anthropic), `bash` (uniform JSON `Bash`), `grep` (uniform JSON `Grep`), `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`, `interrupt_subagent`). There are no `read`/`write` tools.
- Sending OpenAI-profile tools to Anthropic (or vice versa) is a hard `ProviderError::Provider`; the profile must match the provider.
- Wire details (RPC methods, how the daemon calls these adapters) live in [websocket-rpc](../websocket-rpc.md); the React client that drives sessions is documented in the [web UI](../../../packages/web/docs/web-ui.md) doc.
