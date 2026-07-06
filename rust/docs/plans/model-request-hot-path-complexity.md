# Model-request hot-path complexity plan

## Status and authority

This plan is based on `origin/main` at
`b9803d90eb6b55a74425aea2d70f12354766183e` (#228), which includes the OpenAI
web-sidecar correctness prerequisite. Stage 0 was performed as local
measurement only and is not a production PR; Stage 1 is the first optimization
PR in this series.

The authoritative provider contracts remain:

- [`rust/docs/provider-api-support.md`](../provider-api-support.md)
- [`rust/docs/plans/provider-modernization-stack.md`](provider-modernization-stack.md)

If this plan conflicts with either document or with current provider request
fixtures, the provider contract wins. Update this plan rather than weakening
replay, terminal, capability, authentication, or native-compaction behavior.

## Objective

Remove repeated linear and quadratic work from every routine model request.
The target is one shared logical model input, one provider-generation body
serialization/compression, amortized-linear stream framing, O(1) action
correlation, and a fixed number of database round trips for a fixed logical
transition.

This is not a promise to eliminate all linear work. A normal full-replay turn
has an unavoidable **O(B)** active-context traversal/materialization and wire
serialization, and an unavoidable **O(S)** stream parse. Anthropic must still
serialize and upload its full `tools -> system -> messages` request even when
the provider reports a prompt-cache hit. Output construction and persistence
also remain linear in the bytes/items actually produced; the goal is to avoid
repeating those passes and to avoid one SQL round trip per item.

This plan removes work. It does not hide work by starting tool side effects
while the model is still streaming. Latency hiding through eager tool
execution is a separate problem with a much larger durability boundary.

### Complexity notation

| Symbol | Meaning |
| --- | --- |
| **B** | Model-visible entries/bytes on the active transcript branch. |
| **N** | All transcript nodes stored for a session, including inactive branches. |
| **S** | Bytes in one provider SSE response. |
| **K** | Outstanding model/tool actions in one session/turn. |
| **E** | Transcript entries and websocket events persisted by one transition. |
| **A** | Pending actions reconstructed during recovery. |
| **L** | Entries in the process-wide session-lock registry. |
| **R** | Entries in process-wide task registries. |
| **T** | Provider tool declarations in a profile. |

## Complexity budget

The table distinguishes the **routine hot path** from startup/cold recovery.
Stage 0 records the baseline rather than treating the estimates below as
measurements.

| Area | Current `origin/main` shape | Routine hot-path budget | Startup/cold-path budget |
| --- | --- | --- | --- |
| Warm activation | Driver-lock lookup scans **L** weakly-live entries; an empty dispatch can still load config and hydrate workspaces. | O(1) average lock lookup; no config/workspace load when there is no dispatch; no empty persistence transaction. | Not applicable; warm sessions must not invoke full rehydration. |
| Cold activation | Repeated recovery helpers and serial queries precede `load_stored_session`; all **N** nodes are loaded and rebuilt, roughly O(N log N). | Cold work must never move back into the queue-acceptance response; #208 remains authoritative. | Initially measure only. If Stage 8 is justified: one control/recovery pass, at most 8 SQL statements and 2 transactions before provider preparation, and O(B) active-branch materialization rather than O(N). |
| Context materialization and copies | `TranscriptStore::model_context` clones O(B), then action, dispatch, retry, title, accounting, and auth boundaries clone or rebuild B-sized data again. | Exactly one active-branch materialization; zero full-context clones before the first attempt; at most two live B-sized representations; no more than 2x B copied outside provider serialization buffers. | A recovered context may be materialized once for each genuinely distinct required branch; no speculative resident copy. |
| Logical request builds | Prompt, transcript, and tool projections are rebuilt for accounting, generation, title sidecars, and each retry. | One immutable logical input per claimed model action; unchanged retries share it. Sidecars share immutable prefix data but retain their distinct suffix and bounded input. | Capability discovery may be cold once per bounded provider/account cache; it is not charged to every warm request. |
| Generation serialization | OpenAI builds `Value`, serializes, and zstd-compresses; Anthropic builds `Value` and reqwest serializes it. Retries repeat work. | One body serialization per logical generation and one OpenAI compression. Unchanged transport/auth retries reuse identical prepared bytes. | Reprepare after a body-shaping capability/account/input change, never merely because an auth header changed. |
| Accounting | Claude builds and serializes a separate O(B) count request; OpenAI may clone suffix entries or render all B. Credentials and prompt/tool material are reloaded. | One shared logical input. OpenAI with usage anchor is O(suffix), without one at most one extra O(B) estimate. Claude may perform one operation-specific O(B) count serialization and one network count, then generation reuses the same immutable input. Zero synchronous credential filesystem reads after activation. | Cache misses and explicit credential refresh are separately counted cold operations. |
| SSE framing and parsing | Front scans, frame copies, `Vec::drain`, and line joining can approach O(S²); unterminated/error bodies are unbounded. | O(S) total framing; each byte examined at most 2-3 times; bounded frame, buffered, multiline, and error-body bytes. Preserve provider-specific semantic state machines. | Same budget; cold/warm does not change stream behavior. |
| Action completion | `VecDeque::position`, repeated tool identity scans, and action-outbox scans can make K completions O(K²). | O(1) average exact lookup/removal by `ActionId`; source-ordered release O(K) total for the batch; duplicate identity checks remain exact. | Recovery reconstructs the same ledger in O(A), not O(A²). |
| Persistence | Transcript entries, actions, and events are inserted serially; completion validity is read more than once; no-op transactions occur. SQL count grows with E. | No-tool provider completion through `session.idle`: at most 2 transactions and 8-10 SQL statements. Transcript/event statement count is independent of E through bounded batches. One authoritative completion CAS. | Recovery writes retain current fences; do not weaken #217/#221 to meet a statement target. |
| Registries | Session locks scan L on every acquire. Task paths perform `retain` scans over R. Provider connection lookup is already O(1) average. | Session-lock lookup and cleanup O(1) average and under 100 us p99 at 10,000 keys. Keep provider lookup O(1). Count task-registry scans; do not import the prototype's generation/supervisor rewrite. | Exact cleanup must not drop a replacement registered under the same key. Any future task-index PR must preserve #221 leases and #226 ordinary-task behavior. |
| Replay and recovery | Routine handlers can invoke recovery; pending model actions perform a per-action recursive context query (N+1 in A); cold session load reconstructs N. | No replay rewrite and no recovery query on an already-active ordinary turn. Full exact local provider replay remains the source of truth. | If measured and addressed in Stage 8, fetch required branches set-wise: at most 2 context round trips independent of A, plus bounded fixed recovery statements. |
| Tool/prompt registries | Tool projections are cloned/sorted in the registry and again in adapters, O(T log T) each request. | O(1) lookup of immutable, pre-sorted provider/profile snapshots; no lowercase allocation or sort per request. | Rebuild only when the registry/profile generation changes. |

The SQL and latency numbers are budgets, not reasons to combine unrelated
transactions or weaken transactional correctness. Stage 0 must record both
statement count and transaction duration before Stage 7 changes either.

## Clean-main baseline and constraints

### Already-landed modernization

The implementation starts after the following commits and preserves their
constraints:

| Commit / PR | Landed behavior | Constraint on this plan |
| --- | --- | --- |
| `2fda1ae` (#217) | Durable selected-subagent controls, exact-child fencing, restart reconciliation, and persisted control phases. | A no-op persistence check must include `control_interrupt_input_id`. Startup/recovery ordering remains control reconciliation before post-compaction recovery and stale marking. |
| `7cf7531983e5` (#218) | Modern Anthropic models/capabilities, bounded cache, current hosted tools, adapter-owned output limits, and the provider-neutral metadata surface. | Keep Claude policy and hosted-tool wire shapes adapter-owned. Do not replace conservative discovery or recreate daemon-owned provider policy. |
| `dc0aa2c7c914` (#219) | Strict stream lifecycle, explicit malformed SSE, terminal validation, immutable provider-tagged replay, refusal/incomplete handling, and exact output reconciliation. | The scanner stays below `SseEvent::{Json, MalformedJson, Done}` and the existing callback/provider lifecycle. Never introduce a looser normalized stream or rewrite raw replay. |
| `ca3e1dcc273a` (#220) | Authenticated account-scoped Codex capability discovery, no-redirect client, exact model/effort validation, and catalog-driven request shaping. | Prepared bodies are built only after current capability resolution. Preserve the one-time 401 refresh path and exact account/model cache semantics. |
| `eea8dfc7d5ba` (#221) | Durable native-compaction checkpoint installation plus dispatch owner/generation lease, heartbeat, startup reclaim, and terminal fencing. | Shared inputs are invalid after compaction changes the transcript. Do not create a second weaker generation system or bypass lease fences. |
| `014e5fdc55c6` (#222) | Native-only compaction cutover. | No local summary, trim-and-retry, remote-mode fallback, replay-free Claude checkpoint, or optional `compact` implementation may return. |
| `d39ac67fe4a4` (#223) | Dispatch panics enter terminal compensation; title refresh stopped forcing a rejected OpenAI output cap. | New work stays under the panic boundary. Keep title output locally bounded without restoring the rejected provider field. |
| `a9e4ab38428f` (#224) | Removed the already-applied one-time native-compaction migration and runbook. | No old migration or upgrade compatibility is reintroduced. |
| `7f0c6a59e7e4` (#226) | Re-dispatch no longer aborts a live ordinary non-leased task and strands its durable action. | Do not replace task registration with the prototype's generation-keyed registry. Exact cleanup must not abort/detach work differently. |
| `a59a447f1f3e` (#227) | Removed post-migration compatibility, legacy observation/wakeup paths, and retired provider aliases. | Canonical providers remain exactly `openai` and `claude`; do not accept `anthropic`, recreate backfills, or restore old RPC/store behavior. |

There is no merged #225 in this sequence. Its stranded-action invariant is
valid, but its periodic reaper branch is not part of this plan. The independent
NUL-sanitization fix `b0a4e23` (#212) also remains mandatory before any tool
result reaches PostgreSQL.

### Existing performance work

`217cc365e470` (#208, **Queue follow-up inputs before driving sessions**) has
already shipped the main request-latency improvement from the older
architecture proposal:

- `enqueue_session_input` durably inserts the queued input before spawning the
  background driver;
- the response no longer waits for cold runtime reconstruction;
- source validation and idempotent retries remain transactional; and
- startup sweeps active queues.

Do not reimplement "queue before drive." The remaining cold-path issue is the
background driver's total work.

The following branches forked before #208 and are stale concept sources only:

| Branch / PR | Safe concepts to reuse manually | Hazards; never merge the branch |
| --- | --- | --- |
| `0ac23c920aa4` / #201 | Timing separation for lock acquisition, recovery, store load, serialization, response bytes, and client apply/paint. For this plan, port only backend counters needed by Stage 0. | It contains the old synchronous idle-input path and edits high-churn `main.rs`/web files. It has no focused instrumentation tests. Web timing is a separate follow-up because this plan has no web changes. |
| `a9ba9614c0da` / #202 | Query-plan hypotheses for non-cancelled and follow-up ordering indexes, evaluated independently with `EXPLAIN (ANALYZE, BUFFERS)`. | Main already has #208's partial active queue index. #202 reuses its name with a different definition, so `CREATE INDEX IF NOT EXISTS` would silently leave deployed databases different from fresh ones. It also risks write amplification and omits newer canonical ordering details. No index/schema change belongs in these stages. |
| `52b70260cfd9` / #207 | Separate archive from bounded runtime continuation; active-branch/scoped loading; side-effect-light reads; measure fallback reasons. | Queue-first acceptance is already shipped. The branch's `consuming` lease mechanics conflict with current `xmin`/canonical-next queue fencing, and its worker design predates #217/#221. It must not assume provider-retained conversation state or lossy replay. |

`rust/docs/design-decisions.md` still says **"Idle Input Skips The Queue."**
That statement is stale relative to #208 and its
`follow_up_to_idle_session_is_durably_queued_before_drive` regression test.
Correct it in a separate documentation follow-up; do not use it as a design
premise here.

## Immediate correctness prerequisite: OpenAI web sidecar

Before performance stages, fix the current OpenAI `web_search` 400 in a
separate clean, focused PR. This plan intentionally contains no code for that
fix.

Two similarly named limits are distinct:

1. `WebSearchArgs.max_output_tokens` is a **local tool-result cap**. It is
   applied after the sidecar response by
   `sidecar_response_to_tool_result` /
   `limit_tool_output_with_max_tokens`. It is not forwarded to the provider.
2. `ModelRequest.max_tokens` is a **provider generation cap**. The web sidecar
   independently installs `Some(min(configured_or_8192, 8192))`; OpenAI
   serializes it as `max_output_tokens`, which some private Codex models reject
   with HTTP 400.

The clean prerequisite should omit the generation cap for the OpenAI web
sidecar while retaining a bounded Claude `max_tokens` and retaining the local
tool-result cap. Regression coverage must prove both halves. Do not delete or
reinterpret the model-visible tool argument. Performance profiles collected
through a failing sidecar would be misleading, which is why this correctness
fix comes first, but its code must not be mixed into Stage 0.

## Prefix-caching contract

This plan adds no generic cache-policy abstraction. The current fields
(`PromptSections`, `prompt_cache_key`, and
`transcript_cache_prefix_len`) and adapter-specific placement remain the
contract. Prepared-body and static-material caches may key exact immutable
inputs privately, but they must not invent provider-neutral marker semantics.

### OpenAI

Preserve:

- prompt-cache cohort priority: explicit configured key, otherwise stable
  pi-relay session ID, otherwise the existing non-daemon fallback;
- no new volatile action/request UUID in a daemon cache key or body;
- stable prompt in `instructions`, complete exact local replay in `input`, and
  dynamic context as the final synthetic user item;
- deterministic tool order;
- `store: false`, complete local replay, and no
  `previous_response_id` in the current HTTP path;
- window identity derived from session plus transcript/compaction generation;
  compaction starts a new window without changing the durable replay model; and
- `x-codex-turn-state` read and attached at physical-attempt time. A response
  can update turn state before SSE parsing completes, so it cannot be baked
  into prepared body bytes or stale headers.

The existing generated fallback UUID is only for callers with neither daemon
session identity nor provider session state. Normal daemon preparation always
supplies a stable session ID. Do not expand that fallback into a cache key,
revision, prepared-artifact identity, or persistence field.

### Anthropic

Preserve the cumulative cache shape **tools -> system -> messages**:

- tools are deterministic and carry no tool-level marker;
- the stable system block has the explicit one-hour marker;
- the latest cacheable transcript block has a five-minute marker;
- long history receives a second five-minute deep marker within the documented
  lookback window;
- attribution/fingerprint text remains stable for identical stable prompts;
- adaptive-thinking/request shape remains stable where current capability
  policy requires it; and
- count-token bodies remain operation-specific: no generation `max_tokens`
  and no generation transcript breakpoints.

An Anthropic prompt-cache hit reuses provider-side prefix computation. It does
not make the client request incremental: every new Messages generation still
serializes and uploads the full O(B) tools/system/messages body. Stage 5
removes duplicate local serialization and lets unchanged physical retries
share bytes; it cannot reduce this per-logical-turn wire lower bound.

## Implementation stages

Each numbered stage is a separate buildable and reviewable PR against clean
current main. Add the failing counter assertion, benchmark, or regression test
first; then make the smallest implementation change that passes it. Do not
carry dormant scaffolding for a later stage.

### Stage 0 - Measure the actual path

**Purpose**

Establish a reproducible baseline before choosing thresholds or claiming a
win. Extend the existing opt-in `PI_RELAY_PERF` approach; normal production
logging must not gain per-chunk or per-query noise.

**Files and symbol areas**

- `agent-session/src/transcript_store.rs`:
  `TranscriptStore::model_context` materialized entries/bytes.
- `agent-daemon/src/runtime/model.rs`:
  `run_model_turn`, `run_model_for_action_with_retries`, context clone/build
  counters, attempt count.
- `agent-daemon/src/provider_runtime/requests.rs`:
  `build_model_request`, `complete_model_request`.
- `agent-daemon/src/provider_runtime/context_accounting.rs`:
  `model_input_tokens_for_gate`, anchor/fallback/count timing.
- `agent-daemon/src/provider_runtime/auth_retry.rs`:
  physical sends, 401 refreshes, and request clones.
- `agent-provider/src/sse.rs`:
  bytes received, bytes scanned, frames, peak retained bytes.
- `agent-daemon/src/runtime/mod.rs`:
  lock wait, recovery/activation, empty persist/dispatch passes.
- `agent-store/src/postgres/{outputs,events,transcript,actions,sessions,queue}.rs`:
  narrow output-persistence/recovery statement counters and selected scoped
  repository-call counters. These are not exhaustive SQL tracing.
- A focused long-context benchmark target under the owning Rust crates; do not
  add a live-provider harness.

**Acceptance**

- A deterministic long-context fixture covers at least 1, 10, and 100 MiB
  logical contexts without contacting a provider.
- SSE benchmarks cover one-byte chunks, one large frame, and 10,000 small
  frames.
- Action benchmarks cover K = 1, 10, 100, and 1,000 reverse-order
  completions.
- One no-tool turn reports: context materializations/copy bytes, logical
  builds, serializations/compressions, accounting passes, physical sends,
  SSE scan bytes, scoped output SQL statements/transactions, and registry
  scans.
- Cold activation reports rows/bytes loaded and recovery SQL separately from
  the warm turn.
- Stage 0 reports disabled-instrumentation overhead from paired local samples
  for O(1), SSE, and reverse-K shapes. Measured empty-hook overhead is
  diagnostic, not a production acceptance gate.

Stage 0's SQL counters are intentionally narrow: `output_sql_statements` covers
scoped output persistence plus the idle-event insert, while
`recovery_sql_statements` covers the two `load_stored_session` queries.
`scoped_store_calls` is a high-level call diagnostic, not a statement estimate.
A later stage that needs a total SQL target must first add dedicated
total-query measurement; it must not infer a total from these counters.

**Stop / rollback**

If counters cannot be scoped to one session/action without process-global
test races, keep only local benchmark counters and tracing spans. Roll back any
always-on high-cardinality labels or body/content logging. Never record prompt,
replay, credentials, or prepared bytes.

### Stage 1 - O(1) in-memory session ledger

**Purpose**

Remove K² completion correlation without changing action durability or
execution order.

**Files and symbol areas**

- `agent-session/src/outstanding_actions.rs`:
  `OutstandingActions::{track_request,track_session_action,accept_completion,
  emit_events_after_core_accepts,clear}`.
- `agent-session/src/session.rs`:
  `AgentSession::queue_session_action`,
  `drop_completed_action_from_outbox`, `invalidate_session_work`, constructor
  and reset paths.
- `agent-session/src/session_tests.rs`: focused ledger behavior and scaling.

Replace the pending linear lookup with exact `ActionId` lookup, retain a
source-order queue, and keep accepted out-of-order completions keyed by exact
ID until the transcript proves they were accepted. Add queued/completed action
ID sets so completion and invalidation do not repeatedly scan
`action_outbox`; perform the one necessary ordered filter/drain once.

Tool call ID, tool name, turn ID, and action ID validation remains exact.
`ActionId` lookup is an optimization, not permission to accept a completion
whose full identity differs.

**Acceptance**

- Existing object-level session tests remain unchanged in meaning.
- New tests cover reverse completion order, duplicate call IDs/names across
  actions, stale completion after interrupt/history edit, model failure, and
  clearing every index together.
- K completions perform K hash lookups and O(K) total ordered release, with a
  benchmark slope near linear from 100 to 1,000.
- No schema, daemon runtime, provider, or RPC file changes.

**Stop / rollback**

Stop if source-order release cannot be expressed with one queue plus exact
maps without changing event order. Retain the current ledger rather than
adding durable cursors, `result_ready`, or generation IDs.

### Stage 2 - Amortized-linear bounded SSE scanner

**Purpose**

Make framing O(S) and bounded while preserving #219's exact adapter behavior.

**Files and symbol areas**

- `agent-provider/src/sse.rs`:
  `read_provider_json_sse_response`,
  `process_complete_sse_frames`, `process_final_sse_frame`,
  `process_sse_frame`, and `sse_frame_boundary`.
- Existing OpenAI/Anthropic parser tests remain semantic acceptance tests;
  adapter state machines should not need structural rewrites.

Insert an internal cursor-based decoder under the current callback API and
`SseEvent::{Json, MalformedJson, Done}`. Use scan offsets and occasional
amortized compaction/splitting rather than front `drain` and rescanning.
Parse single-line JSON from bytes; allocate one bounded scratch buffer only
for multiline `data:`. Bound frame bytes, retained bytes, queued boundaries,
multiline bytes, and non-2xx body bytes.

This stage does **not** expose a pull stream, raw-payload event, normalized
item stream, or eager callback to the daemon.

**Acceptance**

- Tests cover every LF/CRLF delimiter split, one-byte chunks, multiple frames
  per chunk, multiline data, comments/empty frames, `[DONE]`, malformed JSON,
  EOF, terminal-before-EOF, and compaction with a partial tail.
- Oversized unterminated frames, multiline payloads, pending buffers, and
  error bodies fail at documented limits.
- A scan counter proves total examined bytes are bounded by a small constant
  times S. Doubling S in tiny-chunk and many-frame benchmarks takes no more
  than approximately 2.2x.
- All existing OpenAI output reconciliation and Anthropic block-sequencing
  tests pass without expected-output changes.

**Stop / rollback**

Any change to malformed JSON ownership, terminal acceptance, replay ordering,
or provider error classification blocks the PR. Roll back to a smaller cursor
implementation rather than copying the prototype `sse.rs`.

### Stage 3 - Cheap short circuits and exact session-lock cleanup

**Purpose**

Remove fixed work that is provably unnecessary and the O(L) lock-map sweep.

**Files and symbol areas**

- `agent-daemon/src/runtime/mod.rs`:
  `dispatch_ready_actions`, `persist_active_outputs_with_control`,
  `session_driver_lock`, `SessionDriver::{acquire,try_acquire}`.
- Prefer a small private sibling such as
  `agent-daemon/src/runtime/session_locks.rs` for the weak lock registry and
  exact-key drop guard rather than growing `runtime/mod.rs`.
- `agent-daemon/src/state.rs`: only the lock-registry field/type.

Return from `dispatch_ready_actions` immediately after an empty pending-action
query, before loading config or ensuring workspaces. Skip persistence only
when every current obligation is absent: entries, session events, actions,
action update, consumed input, accepted input, provider replay attachment,
active-leaf change, and `control_interrupt_input_id`. Do not copy the
prototype's older `OutputBatch` predicate; it omits #217's control field.

The lock registry stores weak entries and removes exactly the same key/entry
on final guard drop. It must not scan unrelated sessions and an old guard must
not remove a newer replacement.

Task-registry `retain` scans are measured here but are not rewritten. A
separate future PR is allowed only if Stage 0 shows material cost and it can
preserve #221 lease ownership, registration IDs, shutdown serialization, and
#226 ordinary re-dispatch behavior without `ModelGenerationId` or a
supervisor.

**Acceptance**

- Empty dispatch performs one pending query and no config/workspace work.
- A true empty output pass opens no transaction.
- Each individual obligation, especially control interrupt and provider
  replay, defeats the no-op shortcut in focused tests.
- Same-session drivers remain mutually exclusive; different sessions proceed
  concurrently; drop/reacquire and old-drop/new-entry races are covered.
- At 10,000 registry keys, acquire/release touches one key and remains under
  the Stage 0 p99 budget.

**Stop / rollback**

Do not land the persistence shortcut if any durable obligation is inferred
rather than explicitly represented. Do not land weak cleanup if replacement
identity cannot be proven. Keep task scans rather than broadening this PR into
lifecycle redesign.

### Stage 4 - Shared logical input and zero first-attempt full-context clones

**Purpose**

Build provider-visible logical input once and share/borrow it through
accounting, sidecars, retries, and generation while preserving current
provider lifecycle APIs.

**Files and symbol areas**

- `agent-provider/src/lib.rs`:
  factor shared prompt/transcript/tool/model fields of `ModelRequest`,
  `ProviderTokenCountRequest`, and `ProviderCompactionRequest` into one
  immutable ref-counted logical input; preserve `ModelProvider` lifecycle
  methods and current error/result types.
- `agent-daemon/src/runtime/model.rs`:
  destructure/move the first `ModelContext` instead of cloning the dispatch and
  cloning on attempt one.
- `agent-daemon/src/provider_runtime/{requests,auth_retry,context_accounting,
  session_titles,sidecar}.rs`:
  build once, pass borrowed/ref-counted requests, and deep-copy only after a
  genuine body-shaping change.
- `agent-daemon/src/provider_runtime/transcript.rs`,
  `prompt.rs`, and `agent-tools/src/registry.rs`: produce immutable shared
  projections without changing content.
- `agent-provider/src/{openai,anthropic}.rs`: consume borrowed/shared logical
  fields while retaining all modern adapter behavior.

An `Arc` clone is acceptable; a B-sized transcript/prompt/tool clone is not.
Compatibility default methods may keep existing test providers buildable, but
built-in adapters must use the borrowed path. Do not remove
`ModelContext` from durable `SessionAction`: the prototype's replacement
depends on staging/generation claims and is outside this plan.

**Acceptance**

- The first successful attempt records one active context materialization and
  zero full-context clones after it.
- A retry observes the same logical transcript/tool allocation.
- Title scheduling does not clone B-sized data while holding its global map;
  sidecar input remains explicitly bounded.
- OpenAI and Anthropic generated JSON deep-equals current fixtures, including
  replay, tool order, capability-derived fields, refusal/incomplete handling,
  cache markers, and output limits.
- Codex model discovery, no-redirect transport, 401 refresh, and Anthropic
  metadata caches retain their existing tests.

**Stop / rollback**

Any request-body, cache-placement, replay, auth, capability, or native
compaction semantic diff is a blocker. Keep an extra `Arc` layer rather than
changing the public provider lifecycle into a giant shared wire abstraction.

### Stage 5 - Opaque provider-owned prepared bytes

**Purpose**

Serialize/compress one unchanged generation request once, while applying
fresh attempt state on every physical send.

**Files and symbol areas**

- `agent-provider/src/lib.rs`: one opaque prepared-model-request artifact; its
  provider-specific inner representation remains crate-private and contains
  no secrets.
- `agent-provider/src/openai.rs`:
  `responses_body_with_metadata`, direct encoding,
  `zstd_json_request`, `complete_responses`, window/turn-state header
  assembly.
- `agent-provider/src/anthropic.rs`:
  `prepare_messages_request`, typed/direct encoding, `complete`; keep count
  and compaction as distinct operation-specific views.
- `agent-daemon/src/provider_runtime/{requests,auth_retry,provider}.rs`:
  prepare after capability resolution, then reuse bytes across unchanged
  ordinary and auth retries.

The prepared artifact owns ref-counted exact bytes, content type/encoding,
required provider beta/body-shaping metadata, and a capability/input
generation. It excludes bearer/API keys, per-attempt request IDs, OpenAI sticky
turn state, and other mutable headers. Reprepare only when logical input or
body-shaping model/account capability changes. A same-account credential
refresh changes only auth headers.

Avoid an intermediate `serde_json::Value` where a typed provider-specific
serializable view can emit the same bytes directly. This is an internal
implementation detail, not a common cross-provider wire schema.

**Acceptance**

- A no-retry generation records one serialization; OpenAI records one zstd
  compression.
- Unchanged transport/provider retry and same-account 401 retry share the
  prepared byte allocation while the auth header refreshes.
- OpenAI reads session/window/turn headers at send time; a newly returned
  `x-codex-turn-state` is used by the next physical attempt when applicable.
- Anthropic generates a fresh `x-client-request-id` and applies current
  `x-api-key` per send.
- Decompressed OpenAI and raw Anthropic JSON deep-equal Stage 4 fixtures.
- Prepared-artifact `Debug`, metrics, and errors reveal neither body contents
  nor credentials.

**Stop / rollback**

If capability refresh can alter body shape without a provable generation/key,
reprepare conservatively. If attempt-time state leaks into bytes, retain one
serialization per attempt rather than cache an invalid artifact. Never
persist prepared bytes.

### Stage 6 - Reuse accounting input; cache immutable snapshots

**Purpose**

Stop rebuilding the same logical input around the compaction gate and stop
reloading immutable tools, prompts, and credentials on every request.

**Files and symbol areas**

- `agent-daemon/src/runtime/compaction.rs`: pass/return the shared logical
  input and accounting result through the automatic-compaction gate.
- `agent-daemon/src/provider_runtime/context_accounting.rs`: use the same
  input for Claude count and generation; borrow/consume OpenAI suffixes rather
  than cloning them.
- `agent-daemon/src/provider_runtime/compaction.rs`: carry the exact gate
  `tokens_before`; invalidate pre-compaction input after checkpoint install.
- `agent-tools/src/registry.rs`,
  `agent-daemon/src/provider_runtime/{prompt,requests}.rs`, and
  `agent-daemon/src/state.rs`: immutable, sorted provider/profile tool and
  stable-prompt snapshots.
- `agent-daemon/src/auth.rs`,
  `provider_runtime/{provider,auth_retry}.rs`, and `state.rs`: immutable,
  generation-tagged credential snapshot with explicit refresh replacement.

Reuse the existing bounded OpenAI and Anthropic model caches; add no duplicate
catalog. Cache only immutable snapshots. Never put tokens/API keys in cache
keys, `Debug`, traces, or prepared artifacts. A successful Codex refresh
atomically replaces the credential snapshot and provider connection as current
behavior requires.

**Acceptance**

- Below-limit compaction gating builds one logical input.
- Claude count and generation share prompt/transcript/tool allocations but
  serialize their documented distinct operation bodies.
- OpenAI anchored accounting is O(suffix) without a deep suffix clone; fallback
  performs at most one full estimate.
- A native compaction checkpoint discards pre-compaction prepared input and
  builds a new window/input; `tokens_before` is exactly the gate result.
- Provider/profile tool declarations are sorted once and looked up by shared
  snapshot; output fixtures do not change.
- After activation, a routine count/generation performs zero synchronous
  credential/config filesystem reads. Explicit refresh remains tested.

**Stop / rollback**

Do not cache a value without a complete invalidation source. Prefer a repeated
small immutable prompt clone over a broad stale cache. Roll back credential
caching if refresh atomicity, redaction, or account isolation is not proven.

### Stage 7 - Batch transcript/events and remove duplicate/no-op DB work

**Purpose**

Make database round trips reflect logical transitions rather than E.

**Files and symbol areas**

- `agent-store/src/postgres/transcript.rs`:
  extract a bounded generic multi-row transcript insert preserving sequence,
  conflict, returned records, and last-user-message timestamp semantics.
- `agent-store/src/postgres/events.rs`:
  separate event-row construction from a bounded multi-row insert returning
  ordered `EventFrame`s.
- `agent-store/src/postgres/outputs.rs`:
  `persist_outputs_tx`, reuse returned transcript rows, batch generic
  transcript/events, and keep current action/control semantics.
- `agent-daemon/src/runtime/{model,mod,outputs,events}.rs` and
  `agent-store/src/postgres/{actions,sessions}.rs`:
  remove duplicate action-completion preflight/no-op persist/reset operations
  only after one transactionally authoritative CAS covers the same fence.

This is a manual extraction of generic batching only. Do not copy the
prototype's generation-aware action completion, staged children, release
cursor, or schema. Preserve event payloads, event ID order, activity hints,
revision bumps, `INSERT ... RETURNING` behavior, idempotent conflicts, and
#217/#221 fences.

**Acceptance**

- Transcript/event batch sizes are hard bounded and preserve exact ordering.
- Existing event and store tests compare complete returned objects/frames.
- A no-tool model completion through `session.idle` uses at most 2
  transactions and 8-10 total statements under a dedicated total-query
  measurement added by this stage. Stage 0's narrow output counter is not that
  measurement.
- Statement count is flat as E grows through batch capacity; additional
  statements grow by bounded batches, not one per row.
- One authoritative completion CAS rejects stale attempt/lease; removing
  preflight reads does not permit stale output.
- Empty passes and already-zero automatic-compaction resets issue no write.

**Stop / rollback**

Any change to event order/IDs, revision visibility, action fencing, queue
publication, or conflict behavior blocks batching. Split transcript and event
batches into separate PRs if either cannot be reviewed independently; do not
add schema/index changes to rescue the benchmark.

### Stage 8 - Cold load only if measurement justifies it

**Purpose**

Reduce startup/idle reactivation work only after Stages 0-7 show cold
reconstruction is still material. Prefer active-branch loading over resident
state.

**Files and symbol areas**

- `agent-store/src/postgres/transcript.rs`:
  `load_stored_session`, `stored_transcript_entries`,
  `model_context_for_leaf`, and the recursive active-branch query.
- `agent-store/src/postgres/actions.rs`:
  `pending_actions_for_dispatch` and
  `pending_model_dispatch_from_row`; remove per-action context queries only
  through one set-based required-branch load.
- `agent-daemon/src/runtime/mod.rs`:
  `recover_if_needed`, `ensure_active_loaded`, and duplicate recovery/control
  passes.
- `agent-session/src/{session,transcript_store}.rs`:
  active-runtime rehydration without pretending the archive was deleted.

First implement an active-branch/scoped runtime load that leaves the complete
transcript forest in PostgreSQL for history/read operations. Keep #208 queue
acceptance asynchronous and #217/#221 startup ordering exact. If several
pending model actions genuinely require different leaves, fetch the required
paths set-wise and deduplicate shared ancestors.

Only if active-branch loading is insufficient may a separate, very small
bounded in-memory cache be considered. It must key exact current session,
transcript, and queue revisions, validate before reuse, evict after ambiguous
mutation failures, and have a measured hit-rate target. This is the sole
conditional exception to the "no resident layer by default" non-goal.

**Acceptance**

- A session with large inactive history materializes O(B) active bytes, not
  O(N), during ordinary cold drive.
- At most 8 total statements/2 transactions precede provider preparation,
  excluding first-use capability discovery, under dedicated cold-query
  measurement added by this stage. Stage 0's two-query recovery counter is not
  evidence for this total.
- Pending action recovery uses at most 2 context round trips independent of A.
- History switching, full archive reads, selected-child controls,
  post-compaction recovery, idempotent queued inputs, and exact replay all
  retain integration coverage.
- Benchmarks report p50/p95 cold latency, rows/bytes loaded, and any cache
  hit/miss/fallback reason.

**Stop / rollback**

Do not build this stage if cold activation is not a measured p95 contributor.
Prefer no cache when B is usually close to N. Abandon resident caching if every
mutation cannot return/validate an exact revision or if invalidation requires
cross-cutting RPC/schema/web changes. Roll back to active-branch load without
changing durable storage.

## Optional later transport experiment

Only after the preceding work is measured may OpenAI private Responses
WebSocket continuation be investigated. A successful turn-scoped connection
could send `previous_response_id` plus only the new input delta when every
non-input property and the borrowed replay prefix still match.

This is not applicable to Anthropic. It is also not permission to make
provider state durable or authoritative:

- Postgres retains complete exact replay;
- `store: false` remains;
- the response ID is ephemeral turn/connection optimization state;
- restart, mismatch, missing ID, capability change, or transport failure falls
  back to a full prepared request; and
- no Conversations/background API assumption enters recovery.

This optional work needs its own transport plan and evidence that the private
backend supports the exact contract. It is not required to complete this plan.

## Prototype extraction matrix

The current large uncommitted working tree remains **reference only**. It is
not a branch to finish, commit, or push for this effort. Every useful idea is
manually reimplemented against clean `origin/main`, with current tests and
provider contracts visible in the diff.

### Safe manual extractions

| Prototype concept | Destination stage | Required adaptation |
| --- | --- | --- |
| `ActionId` map, source-order queue, accepted completion map | Stage 1 | Retain current full completion identity and #217 session/control behavior. |
| Queued/completed action ID sets | Stage 1 | Initialize/clear every constructor, recovery, invalidation, and reset path. Filter ordered outbox once. |
| Incremental `scan_from`, delimiter boundaries, amortized buffer compaction, scanner limits/tests | Stage 2 | Place under main's callback and `SseEvent::{Json,MalformedJson,Done}`; retain strict #219 adapters. |
| Empty pending-dispatch shortcut | Stage 3 | Apply before config/workspace work on current `dispatch_ready_actions`. |
| Empty output-persistence shortcut | Stage 3 | Reimplement from current obligations and include `control_interrupt_input_id`; never copy the old predicate. |
| Weak session-lock registry with exact drop cleanup | Stage 3 | Integrate only the lock registry; prove an old drop cannot remove a replacement. |
| Borrowed provider calls / moving first `ModelContext` | Stage 4 | Forward-port onto the modern trait, metadata, errors, auth refresh, and native compaction. |
| Bounded generic transcript inserts | Stage 7 | Preserve sequence/conflict/timestamp/returning semantics. |
| Event row construction plus bounded batch insert | Stage 7 | Preserve every current event variant, payload, activity hint, ID, and order. |

### NEVER CHERRY-PICK WHOLESALE

Do not copy or cherry-pick the prototype versions of:

- `rust/crates/agent-provider/src/{lib,openai,anthropic,sse}.rs`;
- `rust/crates/agent-daemon/src/runtime/{model,dispatch,tasks,tool,compaction,
  supervision,streaming_turn}.rs`;
- `rust/crates/agent-daemon/src/provider_runtime/{auth_retry,connections,
  provider,session_titles,compaction}.rs`;
- `rust/crates/agent-daemon/src/{resident_sessions,main}.rs`;
- `rust/crates/agent-store/src/postgres/{schema,model_staging,compaction,
  actions,outputs}.rs`;
- `rust/crates/agent-store/src/lib.rs`;
- `rust/crates/agent-vocab` ID/provider rewrites;
- `packages/web/src/{App,agentApi,transcript,types}.*`;
- Cargo manifests/lockfiles, schema/migration/startup-backfill files, README
  upgrade instructions, or websocket/RPC documentation from the prototype.

A broad copy would remove or regress newer model metadata/capability errors,
explicit malformed-SSE behavior, terminal/replay correctness, native-only
compaction, selected-child control persistence, NUL sanitation, title output
budget behavior, ordinary-task redispatch safety, canonical provider names,
and post-migration cleanup. It would also expose staging-only
`result_ready`/event values over public APIs. The prototype's provider,
runtime, store, schema, and web changes form an eager-streaming correctness
unit; partial import is unsafe, and the whole unit is outside this objective.

## Non-goals

The following are explicitly not part of this plan:

- eager tool execution or any side effect before strict provider terminal
  success;
- a normalized completed-item/model-event stream;
- `ModelGenerationId`, generalized generation claims, output-item admission,
  durable model staging, `result_ready`, release cursors, or partial-stream
  recovery;
- `StreamingTurnCoordinator`, a shared supervisor, generation-keyed task
  registry, or live-tool test policy;
- a resident-session layer by default;
- schema, migration, RPC, websocket response-shape, or web UI changes;
- a live-provider test harness;
- reintroducing `anthropic` aliases, local compaction, old migration
  compatibility, text observation/wakeup compatibility, or replay-free
  checkpoints;
- provider-retained conversation durability, Conversations/background mode,
  or treating an OpenAI response ID as a recovery source;
- generic provider cache-policy/fingerprint persistence; and
- eliminating the necessary O(B) full-replay wire work or O(S) parse.

## Dependency order and review discipline

The buildable order is:

```text
separate web-sidecar correctness PR
  -> Stage 0 counters/benchmarks
  -> Stage 1 in-memory ledger
  -> Stage 2 SSE scanner
  -> Stage 3 no-op work + session locks
  -> Stage 4 shared logical input
  -> Stage 5 prepared bytes
  -> Stage 6 accounting/static snapshots
  -> Stage 7 generic persistence batching
  -> Stage 8 measured cold-load work, if any
  -> optional OpenAI WebSocket experiment, if separately justified
```

Stages 1-3 can be developed independently after Stage 0, but land separately
so profiles identify each gain. Stages 4-6 are ordered ownership layers:
prepared bytes must not precede a stable shared logical input, and accounting
reuse/caches must not precede explicit invalidation boundaries. Stage 7 keeps
Stage 0's narrow output counter for before/after comparison and adds a
dedicated total-query measure for its total-statement acceptance target. Stage
8 does the same for cold queries and is never pulled forward to "complete the
architecture."

If Stage 7 transcript and event batching cannot fit one reviewable PR, split
it into 7a and 7b without changing the order or adding schema. No stage grows
because an unrelated cold-path hardening opportunity appears; record that
opportunity and continue only with the current acceptance target.

## Success criteria and rollout

For every stage:

1. Land one stage/PR at a time from clean current main.
2. Add the red benchmark/counter assertion or behavioral regression first.
3. Capture before/after wall time, allocation/copy bytes, SQL count, and the
   relevant flame/profile view on the same fixtures.
4. Run provider body/replay/cache-marker tests whenever ownership or
   serialization changes, even if the intended JSON is identical.
5. Keep changes behind existing internal APIs where possible; no schema/RPC/web
   rollout is required.
6. Stop when the measured budget is met. Do not implement later scaffolding
   "while here."
7. Revert the stage independently if correctness fixtures change or p50/p95
   regresses outside noise; no later stage may be required to make an earlier
   stage safe.

The plan is complete when measurements show:

- one O(B) active-context materialization and no first-attempt B-sized clone;
- one unchanged generation serialization/compression and reusable prepared
  bytes across physical retries;
- O(S) bounded SSE framing under adversarial chunking;
- O(1) exact completion correlation with O(K) total ordered release;
- no routine empty config/workspace/persistence work;
- O(1) session-lock registry operations;
- no routine synchronous credential filesystem I/O or per-request tool sort;
- fixed/bounded SQL statement counts for transcript/event persistence; and
- cold loading is either demonstrated acceptable or improved through
  active-branch loading without a default resident layer.

These criteria do not require Anthropic wire work below O(B), eliminate output
linear work, or make all startup/recovery work constant. They require that any
remaining linear traversal is necessary, bounded to the relevant input/output,
and performed once per logical operation.

## Source and commit citations

Primary clean-main source areas audited for this plan:

- `agent-session/src/{outstanding_actions,session,transcript_store}.rs`
- `agent-daemon/src/runtime/{mod,model,dispatch,tasks,compaction}.rs`
- `agent-daemon/src/provider_runtime/{requests,auth_retry,context_accounting,
  prompt,connections,session_titles,web_tools}.rs`
- `agent-provider/src/{lib,sse,openai,anthropic,transcript,token_estimator}.rs`
- `agent-tools/src/registry.rs`
- `agent-store/src/postgres/{outputs,events,transcript,actions,sessions,queue,
  token_usage}.rs`

Historical citations:

- #208 `217cc365e470`, queue before background drive.
- #201 `0ac23c920aa4`, stale hot-read instrumentation concepts.
- #202 `a9ba9614c0da`, stale queue-index hypotheses.
- #207 `52b70260cfd9`, stale session-runtime hot-path architecture concepts.
- #217-#224, #226, and #227 are listed with their constraints in
  [Already-landed modernization](#already-landed-modernization).
- #225's unmerged reference is `5493a1773868`; it is not an implementation
  dependency.

All line-level implementation decisions must be rechecked against the
then-current main branch before each stage. Commit hashes above establish the
design baseline; they are not permission to replay old diffs over newer code.
