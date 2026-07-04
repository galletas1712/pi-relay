# agent-provider

> Part of the [Rust Agent Stack](../architecture.md) | [Provider API support](../provider-api-support.md) | [Design decisions](../design-decisions.md)

`agent-provider` is the model-IO boundary. It defines the `ModelProvider` trait and two adapters: `OpenAiProvider` (the private ChatGPT/Codex Responses-compatible backend, not public OpenAI API-key transport) and `AnthropicProvider` (Messages API). Each adapter turns a provider-neutral `ModelRequest` — stable prompt prefix, dynamic context, transcript items, tool definitions — into the exact wire envelope the upstream backend expects, streams the SSE response, and returns a single normalized `ModelResponse` plus the opaque per-turn replay state needed to continue the conversation later. The crate forbids `unsafe`.

See [design decisions](../design-decisions.md) for *why* the provider scope is small (two providers, ChatGPT/Codex subscription transport only, no plain OpenAI API key).

## Responsibilities

- Define the `ModelProvider` trait: `complete`, provider-neutral
  `model_metadata`, plus optional `compact` and `count_tokens`.
- Render `ModelRequest` into provider-native request bodies and headers.
- Stream and parse provider SSE into one `AssistantMessage` (`Text` / `ToolCall` items only).
- Capture per-item `ProviderReplayItem` sidecars so encrypted reasoning / thinking blocks replay verbatim on the next request.
- Map provider-native tool names to canonical pi-relay names only in semantic
  transcript/UI projections; opaque replay retains provider wire names.
- Discover/cache provider model capabilities and normalize only the input-window
  and automatic-compaction values consumed by the daemon scheduler.
- Surface provider errors with diagnostics, including typed catalog failures
  and context-overflow classification for the daemon's recovery logic.
- Estimate input tokens locally (`token_estimator`) for the runtime's pre-flight context gate.

It does **not** own auth acquisition/refresh, retry loops, scheduler lifecycle,
or tool execution — those live in the daemon and
[agent-tools](./agent-tools.md). Provider-specific threshold policy belongs in
the adapters; the daemon owns neutral precedence/clamping and persisted
compaction state.

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

`ModelResponse` = `{ assistant: AssistantMessage, provider_replay, usage: Option<ProviderUsage>, stop_reason, stop_details }`. `ModelStopReason` is `Complete`, `MaxOutputTokens`, `Refusal`, or `Compaction`. `Compaction` is valid only for the special paused Anthropic compact call; an ordinary model turn rejects it. Refusal details retain the optional provider category and human-readable explanation.

`ProviderCompactionRequest` / `ProviderCompactionResponse`, `ProviderTokenCountRequest` / `ProviderTokenCountResponse` mirror the same prompt/transcript/tool inputs for the non-`complete` methods. Compaction requests also carry optional provider-native custom instructions; token-count responses can retain provider-reported original input occupancy.

`ProviderModelMetadata` exposes only scheduler-consumed normalized values:
`max_input_tokens` is the resolved current/default input window, and
`recommended_auto_compact_tokens` is an optional adapter recommendation.
Provider-only request ceilings stay private to the adapter; for example,
Anthropic's output ceiling clamps Messages bodies without becoming daemon
scheduler metadata.

`ProviderUsage` carries token counts (`input`, `output`, `total`, `cache_read_input_tokens`, `cache_creation_input_tokens`), provider-native merged usage fields, and OpenAI debug metadata lifted off response headers (`upstream_request_id`, `cf_ray`, `server_model`, `codex_turn_state`, `reasoning_included`).

`ProviderError` variants: `Http`, `Timeout`, `Transient`, `Provider`,
`ModelCatalog { status, message }`, `Status { status, message }`,
`Incomplete { status, reason }`, `Json`, and typed `NativeCompaction`. The
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

Before rendering an ordinary or compact body, the adapter exact-resolves the
configured slug from authenticated
`GET /models?client_version=0.142.3`. The GET uses the same bearer/account,
originator, Codex-shaped User-Agent, installation-id, and residency identity as
generation, but no session/window/turn routing headers and no request body.
`CODEX_CLIENT_VERSION` is also the User-Agent version, preventing query/identity
drift.

The daemon owns one in-memory catalog cache shared by reconstructed OpenAI
provider handles. It is scoped by base URL plus account id, or by a
cryptographic token fingerprint when no account id exists; credentials are
never included in debug output. A complete successful catalog remains fresh
for five minutes, and concurrent callers share one detached refresh without
holding the cache lock over HTTP. Responses are bounded to 4 MiB and 256 unique
nonempty slugs; consumed positive limits and at most 16 nonempty efforts per
model are validated before the whole catalog installs atomically. Empty success
is authoritative. A generation/key guard prevents a late refresh from an old
account replacing the active catalog.

Cold or expired refresh failures surface `ProviderError::ModelCatalog`; an old
snapshot is never used to shape a new request. A 30-second backoff may reuse the
same explicit failure so the daemon's broad retry loop does not hammer
`/models`. HTTP 401 is not negative-cached and enters the daemon's existing
one-refresh/rebuild Codex auth path. There is no static/bundled OpenAI fallback,
alias/prefix match, model substitution, public `/v1/models` fallback, ETag
request, or disk cache.

The Responses body hardcodes the low-variance personal-use policy:

```
parallel_tool_calls = <catalog supports_parallel_tool_calls>
service_tier        = "priority"
store               = false
stream              = true
include             = ["reasoning.encrypted_content"]
tool_choice         = "auto"
reasoning.effort    = <exact catalog-supported ReasoningEffort>
prompt_cache_key    = <cohort key>
```

`store = false` makes every request stateless, so reasoning must be replayed from sidecars (see below). There is **no daemon-enforced output-token cap**: `max_output_tokens` is omitted unless `ModelRequest.max_tokens` supplies an explicit value.

The catalog's resolved input window is
`context_window.or(max_context_window)`: current/default wins over maximum.
OpenAI recommends the smaller of an explicit automatic limit and 90% of that
resolved window, or derives 90% when the explicit value is null/missing. Thus
the sanitized GPT-5.6 372k fixture yields 334,800, while GPT-5.4's 272k current
window yields 244,800 even though it also advertises a 1M maximum. The catalog
has no output-ceiling field, so the adapter does not invent one.

Reasoning support is exact and model-specific. The shared known vocabulary
includes `ultra`; the sanitized fixture advertises it for Sol/Terra but not
Luna, and advertises no `none` for those models. An explicitly configured known
effort absent from the selected model fails locally rather than being clamped
or translated. The static web model/effort picker is unchanged and does not
advertise `ultra`. Provider-native search and patch selector fields are ignored
as non-authoritative input; unknown future values cannot invalidate the
catalog, enable native shell/patch actions, or change the local tool registry.
`service_tier: "priority"` remains
unconditional for ordinary and compact calls even when the selected catalog
entry does not advertise priority.

`x-codex-turn-state` is sticky routing state scoped to a single `turn_id`: a value returned by an upstream request is replayed on later requests for the same turn (held in `OpenAiCodexSessionState`) and never leaks into the next turn. The `x-codex-window-id` carries a per-session window generation that bumps after compaction, mirroring Codex's "new window after compaction" signal — derived from the latest compacted turn id in the transcript when no session state is attached.

OpenAI has **no** `count_tokens` impl. The public API has a
`/responses/input_tokens` route, but the private backend route returned a
Cloudflare 403 challenge in the audited probe. The runtime reads
`usage.input_tokens` off `response.completed` and otherwise falls back to
reactive overflow recovery.

### Anthropic (Messages API)

Requests go to `https://api.anthropic.com/v1/messages`, streamed, authenticated with `x-api-key`, `anthropic-version: 2023-06-01`, and a Claude-Code-style User-Agent/`x-app: cli`/`X-Claude-Code-Session-Id` envelope. The only unconditional `anthropic-beta` value is the existing `claude-code-20250219` identity header required by that transport. Effort, one-hour cache TTL, fine-grained tool streaming, text editor, and the current hosted web tools are GA and do not send their retired beta headers. Any future beta header must be added only with the beta body/tool that needs it.

The provider retrieves model metadata from `GET /v1/models/{model_id}` through custom `reqwest` code (there is no official Rust SDK in this stack). Models GETs use the documented API version and credentials but do not copy the Messages-only Claude Code beta header. A process-wide cache shared by all reconstructed Anthropic provider handles holds at most 64 settled model ids and coalesces each model's refresh into one in-flight GET without holding the cache mutex during network I/O. In-flight entries are never evicted; if all eviction candidates are refreshing, they may temporarily exceed the bound until completion trims settled entries back to 64. Successful metadata is fresh for six hours. A refresh failure preserves stale last-known-good metadata and starts a separate one-minute retry backoff; the same backoff is a negative cache only when that model has never had a successful value. API-reported `max_input_tokens`, `max_tokens`, effort levels, and adaptive-thinking support shape requests and the daemon's proactive compaction threshold. `capabilities: null` still preserves authoritative token limits, and an authoritative `effort.xhigh: null` disables xhigh rather than inheriting static support. Static metadata keeps known options safe and available when discovery fails: Sonnet 5 and Fable 5 are 1M-input/128K-output models, as are the retained Opus 4.8/4.7 entries. Sonnet 4.5 retains its compatibility fallback of a 200K input window, a generic 170K compaction recommendation, the existing 64K output ceiling, and no assumed adaptive/effort capability. Unknown models conservatively retain the old 64K output ceiling and no assumed input window/capabilities; without a resolved input window they receive no automatic compaction threshold, and unsupported request fields remain disabled.

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

### Provider-native compaction

Both provider adapters implement the required `compact` trait method, and the
daemon uses it for every manual and automatic compaction. Anthropic also
validates the selected model before constructing the compact request; adapter
support alone does not imply that every Claude model supports native
compaction.

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
- **Anthropic** uses the Messages API with the provider-required beta `compact-2026-01-12`, `context_management.edits[0].type = "compact_20260112"`, the minimum valid 50K input-token trigger, and `pause_after_compaction = true`. It supplies the PI compaction prompt as replacement custom instructions, explicitly forbids tool calls, and supplies no tools. Because Anthropic rejects an assistant prefill for this operation, the adapter appends one minimal synthetic user instruction when the rendered transcript is assistant-ended or empty. For an assistant tool-use tail, it normalizes the complete following user run once: real results are retained in tool-use order, only missing results are synthesized, then all non-result user content follows in original order; duplicates and redundant empty user messages are dropped. The authoritative custom instructions remain in the context-management edit. Static support is limited to the documented model ids `claude-fable-5`, `claude-mythos-5`, `claude-mythos-preview`, `claude-opus-4-8`, `claude-opus-4-7`, `claude-opus-4-6`, `claude-sonnet-5`, and `claude-sonnet-4-6`; when Models API metadata includes `capabilities.context_management.compact_20260112`, that authoritative value overrides the static fallback. This static metadata remains necessary when authoritative model metadata is unavailable. Unknown and known-unsupported ids return a typed terminal native-compaction `unsupported` error before network dispatch. The compact call accepts only an eventual terminal `stop_reason = "compaction"` with one index-zero compaction block whose `content` is a non-null/non-empty string; ordinary completion, tools, refusal, max tokens, malformed/truncated streams, and missing/null/empty content are typed native-compaction errors.

The Anthropic compaction block is stored as opaque provider replay on the new `CompactionSummary` root. One strict rule defines valid replay: `type` is `compaction`, `content` is a nonempty string, and `encrypted_content` is absent, null, or a string. `content` is copied from the compaction delta; opaque encryption and all start/delta extension fields (including a top-level `name`) are retained unchanged. No cache fields or daemon metadata are injected into the provider block. Request rendering parses every Claude sidecar on the only transcript kinds that can emit it, `CompactionSummary` and `AssistantMessage`. Every `CompactionSummary` must have exactly one valid Claude compaction replay block; zero or multiple Claude blocks fail locally. Assistant replay may contain ordinary Claude block types, but corrupt JSON and malformed exact compaction blocks fail locally. Wrong-provider and non-emitted sidecars have no effect.

Subsequent Messages and token-count requests replay the complete block unchanged as an assistant message; the synthetic user summary follows it and carries pi-relay's generic checkpoint label plus the daemon's fresh delegation ledger. Rendering prepares the body and required beta header together, so replay is not rescanned for headers. The two request types intentionally use different strategy shapes. Ordinary Messages sets `trigger = { type: "input_tokens", value: <resolved model max_input_tokens> }` (clamped to the documented 50K minimum) plus `pause_after_compaction = true`. Anthropic documents only the 50K minimum, so the exact model ceiling is schema-valid. A paid production Sonnet 5 automatic E2E accepted this replay shape, resumed the same blocked model action after one native checkpoint, returned the exact requested sentinel JSON, and reduced the effective count from the 541,564-token gate to 15,628 tokens. This proves the tested continuation path, not provider-generated inline compaction during an ordinary response. Anthropic has no documented apply-only mode, so the paused ceiling trigger plus fail-closed parsing is required to protect durable state while pi-relay schedules normal replacement checkpoints at its lower threshold. A model with no safely resolved input ceiling fails replay request construction locally. Token counting retains the live-proven bare `[{ "type": "compact_20260112" }]` edit because Anthropic documents that counting applies existing blocks but does not trigger new compactions. Any compaction block, delta, or compaction stop returned by an ordinary Messages call—including non-paused compaction followed by text/tool use and paused/null/malformed forms—is rejected at the provider parser boundary and cannot become successful persistable replay. Ordinary pre-compaction calls include neither the compaction beta nor the edit. Token counting uses returned `input_tokens` as effective occupancy and retains `context_management.original_input_tokens` as diagnostics.

`ProviderUsage` keeps normalized counts and provider-native merged usage fields
in `raw_provider_usage`. For Anthropic, normalized input is raw
`input_tokens + cache_read_input_tokens + cache_creation_input_tokens`, and
normalized total adds output tokens. Compaction iterations remain excluded from
those normalized message totals. The raw fields retain the unaggregated input
components, `usage.iterations`, nested cache-creation TTL fields, and
`output_tokens_details.thinking_tokens` for billing/audit without double
counting.

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

The special Anthropic compact call uses separate strict state and requires
`message_start`, one index-zero compaction `content_block_start`, one matching
`compaction_delta`, matching `content_block_stop`, one or more `message_delta`
frames with exactly one eventual `stop_reason = "compaction"`, and final
`message_stop`. Pings and unknown future event types are ignored without
advancing structural state. The parser consumes through EOF so
duplicate/conflicting terminal reasons and trailing known frames are
observable, and rejects missing fields, wrong indices/types/order, multiple or
mixed blocks, pre-populated start content, malformed JSON, `[DONE]`, and
truncation. `http.rs` enforces a 45-second response-headers timeout; the SSE
reader enforces a 5-minute idle timeout. The ordinary Anthropic parser requires
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

- `ReasoningEffort::default()` is `Medium`. OpenAI exact-validates the known
  configured value against the selected catalog entry and never clamps it;
  `Ultra` is currently evidenced for GPT-5.6 Sol/Terra but not Luna.
  Anthropic normalizes historical `None`/`Minimal` requests to `Low` inside its
  adapter; Models API capability metadata determines whether the selected
  `low…max` effort is emitted. Sonnet 5, Fable 5, and Opus 4.8 support that
  range.
- Claude Fable 5 is intentionally an explicit opt-in UI choice: Anthropic requires 30-day data retention for Fable and does not offer Zero Data Retention for it. Do not silently select Fable for a ZDR workload.
- The single Codex **401 token-refresh retry** is *not* in this crate. The
  daemon (`provider_runtime/auth_retry.rs`) wraps
  `model_metadata`/`complete`/`compact`/`count_tokens`: on a 401 from a
  Codex-auth provider it uses the existing credential refresh, rebuilds the
  provider, and retries exactly once inside that provider call. The provider
  surfaces the status through either `ProviderError::ModelCatalog` or the
  ordinary HTTP error path.
- Registered builtin tools (from [agent-tools](./agent-tools.md)): `edit` (`apply_patch` for OpenAI, `text_editor_20250728` for Anthropic), `bash` (uniform JSON `Bash`), `grep` (uniform JSON `Grep`), `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`). There are no `read`/`write` tools.
- Sending OpenAI-profile tools to Anthropic (or vice versa) is a hard `ProviderError::Provider`; the profile must match the provider.
- Wire details (RPC methods, how the daemon calls these adapters) live in [websocket-rpc](../websocket-rpc.md); the React client that drives sessions is documented in the [web UI](../../../packages/web/docs/web-ui.md) doc.
