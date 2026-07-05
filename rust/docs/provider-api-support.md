# Provider API Support and Limitations

This is the authoritative support matrix for pi-relay's model-provider
integrations. It describes what the current code actually sends, accepts, and
persists; it is not a list of everything offered by similarly named public
APIs.

**Audited:** 2026-07-04

**Revisions audited:**

- pi-relay PR #215 base `901c94be72021e2fd0db4c4c6e5497b3d865aa3b` and
  capability-discovery changes through the commit containing this document;
- pinned Codex source clone `../openai-codex` at
  `98d28aab54ed86714901b6619400598598876dd0`;
- current OpenAI and Anthropic contracts linked under
  [Sources](#sources), viewed on the audited date.

The OpenAI catalog implementation is based on a sanitized authenticated probe
and was exercised against the live backend on 2026-07-04. **Live** below means
a sanitized provider run recorded in [`../WORKLOG.md`](../WORKLOG.md);
**source** means current pi-relay code; **unit** means an in-process wire mock,
fixture, or unit test; **pinned** means behavior evidenced in the pinned Codex
source; and **official** means a current public provider contract. Public
documentation or private catalog metadata is not evidence that the private
Codex Responses transport accepts a literal wire value.

Table statuses are deliberately narrow: **Supported** is implemented for the
adapter, **Partial** implements only the described subset or passive replay,
**Unsupported** is not implemented (or cannot be claimed), and **Intentionally
not used** is an explicit architecture choice. The evidence tags, not the
status alone, distinguish unit/fixture coverage from historical live use.

## Transport scope

| Adapter | Actual transport | Important boundary |
| --- | --- | --- |
| OpenAI | Private ChatGPT/Codex subscription backend at `https://chatgpt.com/backend-api/codex`, authenticated with a ChatGPT bearer token and Codex CLI identity/routing headers. Ordinary turns use zstd-compressed Responses-compatible HTTP + SSE; compaction uses private unary `/responses/compact`. | This is **not** the public `api.openai.com` API-key transport. A public Responses feature is unavailable unless it is separately implemented and evidenced on this private backend. `[source, live, pinned]` |
| Anthropic | Public Claude Messages API at `https://api.anthropic.com/v1`, authenticated with `x-api-key`, plus a Claude-Code-style identity/attribution envelope pinned to `2.1.75`. Ordinary turns and native compaction use Messages SSE; counting uses `/messages/count_tokens`; capability lookup uses `/models/{id}`. | The pinned envelope is historically live-proven; it is not claimed to be the current Claude Code identity. Identity headers do not change the API contract or by themselves grant Zero Data Retention. Native compaction requires the `compact-2026-01-12` beta and a supported model. `[source, live, official]` |

## Architecture boundary

The provider abstraction is intentionally about lifecycle results, not a
cross-provider wire schema:

- [`agent-provider/src/lib.rs`](../crates/agent-provider/src/lib.rs) owns the
  provider-neutral `ModelRequest`, `ModelResponse`, `ProviderError`,
  `ProviderUsage`, model metadata, native compact/count contracts, terminal
  stop semantics, and the replay contract using `ProviderReplayItem` from
  `agent-vocab`.
- [`agent-provider/src/sse.rs`](../crates/agent-provider/src/sse.rs) owns generic
  SSE framing only. It does not interpret provider event types.
- Transcript materialization filters replay by provider and keeps raw JSON
  immutable. The daemon owns retry policy, auth refresh orchestration,
  compaction scheduling, persistence, and tool execution. In particular, the
  ordinary model loop still retries every provider error up to its existing
  attempt limit.
- [`openai.rs`](../crates/agent-provider/src/openai.rs) and
  [`anthropic.rs`](../crates/agent-provider/src/anthropic.rs) own headers,
  endpoints, body serialization, provider event/item schemas, and conversion
  into normalized results. There is deliberately no giant shared wire enum or
  generic provider validator.

### OpenAI output safety boundary

Ordinary OpenAI output is classified before it can become durable replay:

1. assistant `message` and pinned `agent_message` text shapes are validated
   where pi-relay projects semantic text;
2. `function_call` and `custom_tool_call` validate the fields required for
   local execution and become normalized tool calls;
3. known client-executed actions that pi-relay cannot execute in that wire form
   fail closed;
4. known provider-hosted or passive items require only an object with a
   nonempty string `type`, then replay value-for-value as raw JSON;
5. an unknown ordinary item type fails closed, because it may require a client
   response.

This avoids cloning unstable optional provider fields such as hosted-tool
`id`, `status`, `action`, `result`, or annotations. It accepts pinned Codex web
search variants with omitted fields and `status: "open"`, and current failed
image-generation variants with null or absent `result`; those compatibility
cases are fixture-tested, not newly live-probed. Pinned `agent_message`
projection and opaque `context_compaction` replay retain their actual wire
fields.

Canonical `/responses/compact` output has a separate contract: a nonempty
array, every entry an object with a nonempty string `type`, and exactly one
evidenced `compaction` or `compaction_summary` checkpoint. All item payloads
and their order are otherwise opaque and must be replayed exactly. Optional
assistant text is only a display projection and never replaces canonical
replay.

## Models, request controls, and state

| Capability | OpenAI private Codex adapter | Anthropic Messages adapter | Evidence and limitations |
| --- | --- | --- | --- |
| Model discovery and capability metadata | **Supported.** Authenticated `GET /models?client_version=0.142.3` installs one bounded account-scoped catalog. Model lookup is exact by slug; every ordinary and compact request resolves the selected model before request shaping. There is no static catalog, alias, prefix match, substitution, public `/v1/models` fallback, disk cache, or stale-success fallback. | **Supported.** `GET /models/{id}` is cached and merged over conservative static fallback metadata for input/output limits, effort, adaptive thinking, and compaction capability. | OpenAI discovery/request/cache behavior is source/mock-tested from the sanitized probe schema and its retrieval/resolution path was observed live. Anthropic discovery is source/mock-tested. `[source, unit, live, pinned]` |
| Context windows and automatic compaction thresholds | **Supported.** The adapter resolves `context_window` before `max_context_window` and recommends `min(auto_compact_token_limit, 90% of resolved context)`, deriving 90% when the explicit limit is null/missing. The daemon has no OpenAI static rows: without fresh authoritative metadata it has no proactive threshold, while reactive overflow recovery remains available. | **Supported.** Discovered/static windows drive policy; verified 1M windows default to a 500k threshold and other known windows use the generic policy. | The sanitized probe fixture yields 334,800 for a 372k GPT-5.6 window. GPT-5.4 uses its 272k current/default window (244,800), not 90% of its advertised 1M maximum. The paid Sonnet 5 E2E crossed the 500k gate. `[source, unit, live]` |
| Instructions / system prompt | **Supported.** Stable prompt is Responses `instructions`; dynamic context is a final user item. | **Supported.** Claude Code attribution plus a stable cacheable `system` block; dynamic context is a final uncached user message. | Request-shape tests cover both. `[source, unit]` |
| Maximum output | **Supported.** `max_output_tokens` is emitted only when configured; otherwise omitted. The private catalog advertises no output ceiling, so discovery does not invent or clamp one. | **Supported.** Messages requires `max_tokens`; pi-relay defaults to `min(64k, model ceiling)` and clamps explicit values to the resolved ceiling. | `[source, unit]` |
| Reasoning controls | **Partial.** Sends a configured public `reasoning.effort` (`none` through `max`) only when that exact string is advertised by the selected catalog entry, and requests encrypted reasoning replay; unsupported values fail locally without translation or clamping. Catalog-only strings such as `ultra` are tolerated but cannot be configured or emitted. | **Partial.** Sends metadata-gated adaptive thinking and `output_config.effort`; historical `none`/`minimal`→`low` normalization is adapter-local. No legacy manual thinking budget is generated. | The catalog reports Ultra for Sol/Terra, but pinned Codex converts Ultra to Max before Responses and uses it as the proactive MultiAgent V2 selector. Live literal Sol/Ultra and Terra/Ultra returned HTTP 400; Sol/High, Terra/High, Luna/Max, and GPT-5.4/Medium succeeded. pi-relay exposes no proactive mode and does not alias the value. `[source, unit, live, pinned]` |
| Parallel tool calls | **Supported.** Ordinary and compact request bodies use the selected catalog entry's exact `supports_parallel_tool_calls` value. | **Provider default.** No corresponding daemon-level control is exposed. | Catalog parsing and both OpenAI body paths are unit-tested. `[source, unit]` |
| Text format / verbosity | **Unsupported.** No `text` or `verbosity` control is exposed, although pinned Codex `CompactionInput` has optional `text`. | **Unsupported.** No structured text format or verbosity control is exposed. | Public OpenAI-only and pinned-source capability, not an adapter feature. `[source, pinned, official]` |
| Service tier | **Supported.** Hardcoded to `service_tier: "priority"` for ordinary and compact requests; it is intentionally not configurable and is sent even when the catalog does not advertise priority for that model. | **Unsupported.** The adapter omits `service_tier`, so Anthropic applies its default. | Catalog service-tier advertisement is not used as a downgrade/configuration mechanism. Anthropic publicly supports `auto` / `standard_only`, but pi-relay does not select or normalize it. `[source, unit, official]` |
| Prompt cache routing key | **Supported.** Sends explicit `prompt_cache_key`, else the stable pi-relay session/thread id. | **Unsupported.** Anthropic has no equivalent routing-key field. | OpenAI body behavior is unit-tested. `[source, unit]` |
| Prompt cache retention / markers | **Unsupported.** No explicit retention setting is sent on the private transport. | **Supported.** Explicit 1-hour cache control on the stable system prefix and 5-minute transcript breakpoints, including a deep-history marker past the lookback window. | Public OpenAI supports `in_memory` / `24h`; that does not establish private support. Anthropic cache reads/writes were observed live. `[source, unit, live, official]` |
| Safety identifier | **Unsupported.** Public `safety_identifier` is not sent. | **Unsupported.** No pi-relay request field is mapped. | `[source, official]` |
| Request metadata | **Unsupported.** Public Responses `metadata` is not sent. | **Unsupported.** Messages metadata is not sent. | Session/turn routing headers are transport identity, not API metadata. `[source, official]` |
| Manual stateless conversation replay | **Supported.** Always sends `store: false`, includes encrypted reasoning, and supplies the complete locally materialized input/replay array. | **Supported.** Sends the complete locally materialized Messages history, including exact thinking, hosted-tool, and compaction blocks. | Raw replay is provider-filtered, exact, and durable in Postgres. Corrupt replay fails locally instead of being reconstructed. `[source, unit, live]` |
| Provider-side `store` state | **Intentionally not used.** `store` is fixed false. | **Intentionally not used.** Messages requests are reconstructed from local state; pi-relay has no provider-side conversation object. | This keeps Postgres as the durable source of truth and avoids coupling recovery to provider state. `[source]` |
| `previous_response_id` | **Intentionally not used.** Full manual replay is sent on HTTP SSE turns. | **Unsupported.** Messages has no Responses-style id chaining in this adapter. | Public Responses and pinned private Codex WebSocket code support this concept, but the pi-relay adapter does not. `[source, pinned, official]` |
| Conversations API | **Intentionally not used.** No public Conversation object is created. | **Unsupported.** Messages has no equivalent object in this adapter. | Public OpenAI Conversations persist until deletion and are not ZDR eligible; local durable replay is the selected state model. `[source, official]` |

### Account- and client-version-sensitive Codex catalog

The private catalog is not a universal static model list. The sanitized
2026-07-04 probe returned ten models for client version `0.142.3`; GPT-5.6 did
not appear below `0.142.2`. The adapter therefore uses one
`CODEX_CLIENT_VERSION = "0.142.3"` constant for both the query and
Codex-shaped User-Agent. It caches the whole catalog in memory for five minutes,
scoped by Codex base URL plus account id (or a nonlogged token fingerprint when
the account id is absent). Concurrent cold callers share one detached refresh.

Sol, Terra, and Luna reported current/max windows of 372,000 and null automatic
limits, which derives a 334,800 recommendation. Sol and Terra advertised
`low`, `medium`, `high`, `xhigh`, `max`, and `ultra`; Luna omitted `ultra`; all
three omitted `none`. GPT-5.4 reported a 272,000 current/default window and a
1,000,000 maximum, so the current window remains authoritative for its 244,800
recommendation. The response had no maximum-output fields.

Reasoning-level catalog strings are retained as bounded metadata even when
pi-relay does not recognize them as wire efforts, so `ultra` and future unknown
values cannot make the catalog unusable. This tolerance is not a capability
claim. At pinned Codex revision
`98d28aab54ed86714901b6619400598598876dd0`,
`reasoning_effort_for_request` maps Ultra to Max before Responses request
construction, while the MultiAgent V2 session policy uses Ultra to select
`MultiAgentMode::Proactive`. A completed live release test confirmed why that
distinction matters: literal Ultra passed catalog validation but every Sol and
Terra `/responses` POST returned HTTP 400. The same run succeeded with
Sol/High, Terra/High, Luna/Max, and GPT-5.4/Medium. pi-relay therefore exposes
`max` as its highest raw effort and leaves proactive orchestration as a
separate, unimplemented capability.

A fresh catalog is mandatory for shaping a new OpenAI request. Cold,
authentication, transport, timeout, malformed-response, unknown-model, and
expired-refresh failures surface a typed catalog error. A short failure backoff
may return the same error without another GET, but a retained old snapshot is
never used as stale success. A 401 bypasses negative caching and enters the
daemon's existing one-time credential refresh/rebuild path used by ordinary
Codex calls. The Codex provider client disables redirects, and body-read
failures retain the received status so a truncated 401 cannot enter failure
backoff. The endpoint's weak ETag was not useful (`If-None-Match` still returned
200), so no conditional or disk cache is implemented. Provider-native
search/patch selector fields are ignored as non-authoritative input; only
consumed capability fields such as `supports_parallel_tool_calls` are
deserialized.

## Streaming, terminal behavior, compaction, and counting

| Capability | OpenAI private Codex adapter | Anthropic Messages adapter | Evidence and limitations |
| --- | --- | --- | --- |
| HTTP SSE generation | **Supported.** Reconciles added and completed output-item lifecycles when added events are present. Completed items are authoritative and need not be repeated by terminal `response.output`; compatible terminal overlap is validated without replacing the completed item, while safe terminal-only items fill their array indices. Reconciled output indices must be unique and contiguous and determine replay order. | **Supported.** Uses a bounded content-block state machine with checked contiguous indices, required block stops, strict known deltas, and malformed accumulated JSON failure. | Shared SSE framing handles chunk/frame mechanics only. The private Codex transport has live-emitted a terminal output array that omitted an already completed item. Historical ordinary turns exist for both providers. `[source, unit, live]` |
| Repeated top-level deltas | **Unsupported.** The parsed Responses item model has no equivalent cumulative top-level delta. | **Supported.** One or more `message_delta` events merge cumulative usage and nonconflicting stop details. Missing/null stop reasons are nonterminal; only a recognized non-null reason closes content. | Usage-only, null-reason, repeated-terminal, conflict, and post-terminal cases are fixture-tested against the current contract. `[unit, official]` |
| Successful terminal | **Supported.** Requires a valid `response.completed` and no pending added output items; terminal omission never completes a pending item. Optional terminal output is merged by index: completed items keep their exact payload, overlaps require stable type/identity compatibility, and terminal-only items cross the ordinary fail-closed item boundary. EOF or `[DONE]` is not success. The private minimal terminal without `output` remains accepted only with no pending items. | **Supported.** Requires `message_start`, closed content blocks, a recognized terminal stop reason, and `message_stop`; EOF alone is not success. | Unknown future event types may be ignored but never imply success. `[source, unit]` |
| Refusal | **Supported.** Refusal content becomes a refusal terminal and partial semantic output/replay is discarded. | **Supported.** `stop_reason: refusal` retains valid details and discards partial semantic output/replay. | `[unit]` |
| Incomplete / max output | **Supported.** `response.incomplete` is a typed non-success with status/reason. | **Supported.** `max_tokens` is a normalized terminal; `pause_turn`, context-window exhaustion, and unknown reasons are typed non-successes. | `[unit]` |
| Native compaction | **Supported.** Private unary `/responses/compact`; canonical returned output is installed and replayed exactly. Public inline `context_management` compaction is not sent. | **Supported.** Special paused Messages call with beta `compact-2026-01-12`, replacement instructions, no tools, and strict compaction-only stream parsing. | OpenAI standalone compaction has historical real-backend coverage. Anthropic has a paid production Sonnet 5 E2E; model support is capability/static gated. `[source, unit, live, official]` |
| Compaction replay | **Supported.** Exactly one native checkpoint is evidenced; the complete opaque returned array is replayed unchanged and in order. | **Supported.** Exactly one valid opaque compaction block is retained and replayed with the required beta/strategy; no local alternate summary is substituted. | Ordinary Anthropic turns reject inline compaction, preserving the daemon's durable checkpoint boundary. `[source, unit, live]` |
| Input token counting | **Partial.** No usable private endpoint: `/responses/input_tokens` returned a Cloudflare 403 challenge. The daemon anchors on completed usage, estimates only the local suffix, and retains reactive overflow recovery. | **Supported.** Calls `/messages/count_tokens` with the same local prompt/tool shape. Existing compaction is applied without triggering a new one, and original/effective occupancy is retained. | Public OpenAI `POST /v1/responses/input_tokens` exists but is not usable through this private adapter. Anthropic counting is mock-tested and exercised in the paid compaction path. `[source, unit, live, official]` |

## Tools, actions, and citations

“Partial” often means pi-relay can preserve a provider-hosted output without
offering that provider tool in its main request. Opaque replay is continuity,
not local execution support.

| Capability | OpenAI private Codex adapter | Anthropic Messages adapter | Evidence and limitations |
| --- | --- | --- | --- |
| Function / custom tools | **Supported.** JSON function calls and free-form custom calls become normalized local tool calls; results replay in matching output forms. | **Supported.** JSON `tool_use` calls and results become normalized local tool calls/results. | Local Bash, Grep, LoadSkill, web wrappers, delegation tools, and provider-specific edit declarations use this contract. `[source, unit, live]` |
| Shell and apply-patch / text editor | **Partial.** pi-relay offers Bash as a function and `apply_patch` as a custom free-form tool. Native `local_shell_call`, `shell_call`, and `apply_patch_call` output actions fail closed instead of being mistaken for passive output. | **Partial.** Bash is a local JSON function and Edit uses Anthropic `text_editor_20250728`; other native shell action families are not implemented. | Both local edit paths have unit coverage; Anthropic Bash/editor and OpenAI Bash have historical live coverage. `[source, unit, live]` |
| Computer use | **Unsupported.** A returned client `computer_call` fails closed; passive historical output objects can replay only when already known safe. | **Unsupported.** No computer tool is declared or executed. | Public APIs may offer computer tools; pi-relay has no computer harness. `[source, official]` |
| MCP / approvals | **Unsupported.** No MCP server is declared. Known provider-hosted MCP call/list outputs are opaque replay, while an approval request fails closed. | **Unsupported.** No MCP connector/server or approval lifecycle is declared. | MCP third-party retention/approval semantics are intentionally not hidden behind generic tool execution. `[source, official]` |
| Web search and fetch | **Partial.** `WebSearch` runs a provider-hosted web-search sidecar; ordinary `web_search_call` is opaque replay. `WebFetch` uses the local HTTP tool rather than an OpenAI hosted fetch. | **Supported.** Web search/fetch wrappers use hosted sidecar tools; fetch can fall back to the local HTTP implementation. Hosted result blocks retain their stable outer discriminants and otherwise replay opaquely, including when optional result metadata is omitted. | Anthropic hosted search/fetch was exercised historically live; omitted optional metadata is fixture-tested. OpenAI optional hosted-call variants are unit/fixture-tested, not newly live-probed. `[source, unit, live]` |
| File search / code interpreter | **Partial.** Known provider-hosted output items replay opaquely, but pi-relay does not declare these tools. | **Unsupported.** They are not declared by the adapter. | Output acceptance does not imply request support. `[source, official]` |
| Image generation | **Partial.** Known output items replay opaquely, including failed items with null/absent result, but pi-relay does not request image generation or normalize generated images. | **Unsupported.** No image-generation tool is declared. | OpenAI failed-result compatibility is fixture-tested against the current generated public contract. `[unit, official]` |
| Tool search / namespaces | **Partial.** Server-executed tool-search calls and outputs may replay opaquely. Client/unknown execution modes fail closed; pi-relay does not declare tool search or namespaces. | **Unsupported.** No tool-search declaration or namespace lifecycle is implemented. | Public OpenAI and Anthropic tool-search features are not live adapter capabilities. `[source, unit, official]` |
| Citations / annotations | **Partial.** Citation/annotation fields survive raw replay but are not projected into a normalized citation model. | **Partial.** Citation objects retain a stable outer discriminant and otherwise survive raw replay opaquely, including with omitted optional titles/file ids; they are not projected into a normalized citation model. | Text remains available; Anthropic semantic text plus exact citation replay is fixture-tested, while historical hosted-tool replay was live-proven. Structured citation UX is follow-up. `[source, unit, live]` |

## Transport modes

| Mode | OpenAI private Codex adapter | Anthropic Messages adapter | Evidence and limitations |
| --- | --- | --- | --- |
| HTTP + SSE | **Supported.** Ordinary generation uses private Responses-compatible SSE. | **Supported.** Ordinary and compact generation use Messages SSE. | `[source, unit, live]` |
| Public Responses WebSocket | **Unsupported.** The adapter never connects to `wss://api.openai.com/v1/responses`. | **Unsupported.** Messages has no corresponding mode in this adapter. | Public OpenAI WebSocket mode supports in-memory `previous_response_id` with `store=false`; that is public-contract-only here. `[official]` |
| Private Codex WebSocket | **Unsupported.** pi-relay uses private HTTP SSE only. | **Unsupported.** It is not an Anthropic transport. | Pinned Codex implements a private Responses WebSocket client and chaining, but pi-relay does not reuse it. `[source, pinned]` |
| Background responses | **Intentionally not used.** No background create/poll/cancel path. | **Unsupported.** No analogous background Messages path is implemented. | Public OpenAI background mode requires stored response state and is not ZDR compatible, conflicting with local durable replay. `[official]` |
| Batch | **Unsupported.** No public OpenAI Batch transport. | **Unsupported.** No Message Batches transport. | Both are public asynchronous products, not daemon lifecycle modes. `[source, official]` |

## Accounting and identifiers

| Data | OpenAI private Codex adapter | Anthropic Messages adapter | Evidence and limitations |
| --- | --- | --- | --- |
| Basic usage | **Supported.** Normalizes input, output, and total tokens. | **Supported.** Normalized input is provider raw `input_tokens + cache_read_input_tokens + cache_creation_input_tokens`; normalized total is that cache-inclusive input plus output, including across cumulative stream updates. | `[unit, official]` |
| Cached tokens | **Partial.** Normalizes `input_tokens_details.cached_tokens`; no cache-write metric is exposed. | **Supported.** Normalizes cache read/write counts, includes both in normalized input/total, and retains provider-native counters and TTL detail in raw usage. | Historical cache hits exist for both providers. `[unit, live, official]` |
| Reasoning / compaction / hosted-tool detail | **Partial.** Does not retain the full OpenAI usage object or normalized reasoning-token count. | **Supported.** `raw_provider_usage` retains provider-native merged usage fields, including unaggregated `input_tokens`, thinking detail, server-tool counts, inference/service tier fields, and compaction iterations without adding iterations to normalized message totals. | Provider-specific detail intentionally stays raw. `[source, unit]` |
| Response and request IDs | **Partial.** Validates the terminal response id but does not retain it; retains upstream `x-request-id`, Cloudflare ray, server model, Codex turn state, and reasoning-included headers when usage exists. | **Partial.** Generates a client request id; provider request ids are included in parsed error text but successful message/request ids are not normalized. | `[source, unit]` |
| Assigned service tier | **Unsupported.** The request is fixed to priority, but returned tier is not retained as response accounting. | **Partial.** If returned inside usage it survives raw JSON, but there is no normalized field. | `[source, official]` |

## Zero Data Retention and durable replay

pi-relay's Postgres transcript is intentionally durable and includes opaque
provider replay. Provider-side ZDR never means that pi-relay deletes its local
database, tool output, logs, backups, or exported transcripts.

| Concern | OpenAI private Codex adapter | Anthropic Messages adapter |
| --- | --- | --- |
| Provider-side guarantee | **Unsupported.** Public OpenAI API ZDR terms cannot be applied to the private ChatGPT subscription backend. `store: false` and local stateless replay are ZDR-aligned design choices, but private abuse logging and prompt-cache retention were not established by this audit. | **Partial.** Messages, token counting, prompt caching, and compaction are documented as ZDR eligible when the organization has a ZDR arrangement. The API defaults to up-to-30-day retention otherwise. Claude Code identity headers do not establish the workspace contract. |
| Server-side conversation state | **Intentionally not used.** `store`, Conversations, and background mode are avoided. Public Conversations persist until deletion and background responses persist for polling. | **Intentionally not used.** Every request is rebuilt from the local transcript. |
| Prompt cache | **Partial.** Private retention is unknown: pi-relay sends a cache routing key but no retention selector. Public OpenAI extended cache may retain derived KV tensors on GPU-local storage for up to 24 hours, and newer public models may require it; private behavior is not inferred. | **Supported.** Under the documented ZDR feature contract, cache representations/hashes are memory-only, with 5-minute or 1-hour TTLs. |
| Covered-model exception | **Unsupported.** No applicability claim can be made: an analogous exception was not established for this private product. | **Unsupported.** ZDR does not apply to covered Mythos-class models: as of 2026-06-09, Anthropic requires 30-day prompt/output retention for Mythos 5 and Fable 5 (and designated future covered models), including otherwise-ZDR workspaces. Fable must remain an explicit opt-in. |
| External tools | **Partial.** Hosted web access can contact external sites; public MCP is not used. Any third-party service has its own retention policy. | **Partial.** Hosted web search/fetch can contact external sites; MCP is not used. The organization must assess server-tool and destination policies separately. |

The deliberate state model is therefore: `store: false` where the private
Codex transport supports it, no provider Conversations/background state,
complete local replay, and exact provider-native compaction checkpoints. This
supports crash recovery and auditability without claiming provider-side ZDR
that has not been contractually or live verified.

## Sources

### pi-relay and pinned Codex

- Provider-neutral contracts:
  [`agent-provider/src/lib.rs`](../crates/agent-provider/src/lib.rs)
- OpenAI wire adapter and tests:
  [`agent-provider/src/openai.rs`](../crates/agent-provider/src/openai.rs)
- Anthropic wire adapter and tests:
  [`agent-provider/src/anthropic.rs`](../crates/agent-provider/src/anthropic.rs)
- Shared SSE framing:
  [`agent-provider/src/sse.rs`](../crates/agent-provider/src/sse.rs)
- Runtime metadata scheduling and counting:
  [`compaction.rs`](../crates/agent-daemon/src/provider_runtime/compaction.rs),
  [`context_accounting.rs`](../crates/agent-daemon/src/provider_runtime/context_accounting.rs)
- Hosted web sidecars:
  [`web_tools.rs`](../crates/agent-daemon/src/provider_runtime/web_tools.rs)
- Local/provider tool declarations:
  [`agent-tools/src/registry.rs`](../crates/agent-tools/src/registry.rs)
- Sanitized historical live evidence: [`../WORKLOG.md`](../WORKLOG.md)
- Pinned Codex:
  `codex-api/src/common.rs` (`CompactionInput`),
  `protocol/src/models.rs` (`ResponseItem`),
  `protocol/src/openai_models.rs` (`ModelInfo`), and
  `codex-api/src/endpoint/responses_websocket.rs`.

### Current official contracts

- OpenAI [Create a response](https://developers.openai.com/api/reference/resources/responses/methods/create)
- OpenAI [conversation state](https://developers.openai.com/api/docs/guides/conversation-state)
- OpenAI [compaction](https://developers.openai.com/api/docs/guides/compaction)
- OpenAI [counting tokens](https://developers.openai.com/api/docs/guides/token-counting)
- OpenAI [prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching)
- OpenAI [tools](https://developers.openai.com/api/docs/guides/tools)
- OpenAI [generated response-output item types](https://github.com/openai/openai-python/blob/main/src/openai/types/responses/response_output_item.py)
- OpenAI [WebSocket mode](https://developers.openai.com/api/docs/guides/websocket-mode)
- OpenAI [background mode](https://developers.openai.com/api/docs/guides/background)
- OpenAI [Batch API](https://developers.openai.com/api/docs/guides/batch)
- OpenAI [data controls](https://developers.openai.com/api/docs/guides/your-data)
- Anthropic [Messages streaming](https://platform.claude.com/docs/en/build-with-claude/streaming)
- Anthropic [context windows](https://platform.claude.com/docs/en/build-with-claude/context-windows)
- Anthropic [compaction](https://platform.claude.com/docs/en/build-with-claude/compaction)
- Anthropic [token counting](https://platform.claude.com/docs/en/build-with-claude/token-counting)
- Anthropic [prompt caching](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)
- Anthropic [Message Batches](https://platform.claude.com/docs/en/build-with-claude/batch-processing)
- Anthropic [service tiers](https://platform.claude.com/docs/en/api/service-tiers)
- Anthropic [API and data retention](https://platform.claude.com/docs/en/manage-claude/api-and-data-retention)
- Anthropic [standard retention](https://privacy.claude.com/en/articles/7996866-how-long-do-you-store-my-organization-s-data)
- Anthropic [Mythos-class retention](https://privacy.claude.com/en/articles/15425996-data-retention-practices-for-mythos-class-models)
