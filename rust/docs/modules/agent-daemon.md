# agent-daemon

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`pi-agentd` is a thin websocket JSON-RPC control plane backed by Postgres. It accepts websocket connections, routes JSON-RPC requests to handlers, serializes per-session work behind a driver lock, recovers crashed sessions before first touch, dispatches model and tool actions to background tasks, and replays events to reconnecting clients. It owns no durable state of its own: Postgres ([agent-store](./agent-store.md)) is authoritative, and the daemon holds only process-local projections needed to drive in-flight work. The full method contract is in [websocket-rpc](../websocket-rpc.md); this doc describes the module split and runtime mechanics.

## Responsibilities

- Websocket accept loop, JSON-RPC parsing/routing, and one handler per method.
- Schema migration and stale-action sweep at startup.
- Per-session serialization via `SessionDriver` locks.
- Crash/restart recovery of open transcript tails before any read or write touches a session.
- Live session loading, queued-input consumption, action completion, and `SessionIdle` settling.
- Background dispatch of model requests, tool calls, and compaction jobs.
- Provider selection, credential loading, Codex 401 refresh, and auto-compaction gating.
- Reconnect event replay through `events.subscribe(after_event_id)` and broadcast-lag recovery.

## Module split

The daemon is split by responsibility, not by RPC method family (see [Daemon Modules Have Narrow Jobs](../design-decisions.md#daemon-modules-have-narrow-jobs)). Handlers stay protocol glue: validate params, call the repo/runtime, shape JSON.

```
main.rs            websocket accept + JSON-RPC routing + most RPC handlers
session_start.rs   explicit session-start pipeline: workspace materialization,
                   MCP selection validation, prompt render, atomic
                   session/manifest/output persist, initial dispatch
config.rs          strict XDG daemon startup policy, model defaults,
                   and one-time non-overwriting packaged role/workflow
                   bootstrap
types.rs           RpcRequest/Response/Error, RpcMethod parse table, DispatchAction, RuntimeSession
state.rs           AppState: repo handle, active sessions, driver locks, task registry,
                   event broadcaster, tool registry, provider connections, runtime hosts
codec.rs           JSON <-> vocab parsing + transcript-store reconstruction helpers
auth.rs            credential loading (Codex/Anthropic) + Codex 401 token refresh
runtime/           SessionDriver facade plus concrete lifecycle phases:
                   events, outputs, task registry, dispatch, model, tool, compaction
runtime_hosts.rs   framed-JSON TCP registry for connected runtimes; proxies workspace
                   and MCP commands to the session's runtime host
rpc_views.rs       response shaping (snapshots, queue state, transcript views, server_time_ms)
provider_runtime/  provider selection, model metadata scheduling, model/web-tool
                   execution, compaction, token accounting
                   (MCP snapshot reconstruction from the persisted session manifest)
subagents.rs       delegation subagent spawn core: role resolution, full vs
                   read-only workspace handling, configured role-model selection,
                   child prompt + lifecycle events
delegation_tools.rs     delegation tool surface (delegate_writing_task /
                   delegate_readonly_tasks / inspect_delegation /
                   cancel_delegation / steer_subagent / interrupt_subagent)
                   plus delegation.* web RPCs
                   (start_full / start_readonly_fanout / status / cancel /
                   steer_subagent / list) + homogeneity/one-delegation-per-parent guards
delegation_runner.rs    delegation barrier: all-terminal detect, attempt-fenced finish CAS,
                   idempotent handoff write, one steer to the parent; boot
                   crash sweep
handoff.rs         renders per-subagent task_prompt.md / final_message.md /
                   transcript.md from durable delegation/session state
```

`config.rs` resolves the general configuration root as
`$XDG_CONFIG_HOME/pi-relay/agentd` or `$HOME/.config/pi-relay/agentd`. It
parses only the required strict `config.toml` startup-policy schema: root
`database_url`, optional frontend `bind`, optional `runtime_bind`, an optional
default parent provider, and per-global-role subagent providers. `pi-agentd`
accepts no configuration arguments. Invalid configuration fails startup.
Every configured subagent-provider key must match a global role skill after
catalog bootstrap; runtime and workspace roles instead inherit their parent's
provider unless explicitly overridden when spawned. MCP server definitions
(`$XDG_CONFIG_HOME/pi-relay/runtime/mcp.toml`) and OAuth credentials live on
each runtime host (see `agent-runtime`), not in the control plane.
Startup copy-bootstraps bundled
role/workflow `SKILL.md` files only when absent and then records completion, so
later deletions remain user-owned. Skill/role resolution is explicit
workspace/home first, configured catalog second, packaged fallback last;
workflow names are deduplicated across the latter two sources and roles remain
hidden from ordinary `LoadSkill`.

Subagent work runs as **delegations** (`delegate_writing_task` /
`delegate_readonly_tasks` / `inspect_delegation` / `cancel_delegation` /
`steer_subagent` / `interrupt_subagent`). Full subagents
reuse the parent's workspace dirs in place; read-only subagents get a forked
snapshot destroyed on return. Delegation subagents may emit
`subagent.spawned`/`subagent.running` progress events; their terminal hook fires
a single-flight, `attempt_id`-fenced barrier when all subagents of a delegation are
terminal. After the DB finish CAS wins, the runner writes the handoff directory
and then enqueues one `InputPriority::Steer` daemon observation to the parent.
That observation includes the same structured snapshot shape as
`inspect_delegation`, with `outcome` and compact handoff file
references.
Completion is that typed wakeup observation/handoff, not a parent-visible per-child idle event. The
runner never decides the next delegation — the parent does, guided by workflow
skills. Cancellation is terminal and exports transcript-only files for the
cancelled subagents instead of running the normal completion handoff.

Interrupting child controls are durable and generation-fenced. The captured
generation is the active turn plus its complete deterministic unfinished
attempt set, including parallel tools; reconciliation atomically synthesizes a
valid interrupted transcript boundary and settles all remaining captured rows.
If the active leaf is already a boundary, reconciliation keeps that leaf
unchanged and atomically interrupts only captured boundary-hosted actions, or
records an `already_between_turns` no-op when there are none.
`interrupt_subagent` uses a non-message ledger marker, so replay returns the
prior phase without injecting text or interrupting newer work. A periodic
reconciler also recovers ownerless ready steers, using nonblocking per-session
driver acquisition and bounded diagnostic backoff rather than accumulating
duplicate waiters.

Delegation completion wakeups are rendered as provider-neutral daemon
observations. The durable transcript entry is a typed
`daemon_tool_observation`, not an ordinary user message and not a fake assistant
tool choice. It records the daemon-authored `inspect_delegation` observation
with a stable local tool-call id, arguments, status, concise summary, and bounded
snapshot JSON. Provider adapters translate this typed item into adjacent
synthetic tool call/result pairs for OpenAI and Anthropic request bodies; the UI
renders it as a daemon/system observation card. A text fallback renderer remains
available for diagnostics and unsupported contexts.

The snapshot never inlines full transcript bodies or raw subagent task prompts.
Task prompts are materialized as per-subagent `task_prompt.md` handoff files,
final messages are exposed through `final_message.md` file references, and
`outcome` stays inline because workflows branch on it.

Normal top-level parent model requests do not receive a daemon-generated
delegation dashboard. They are transcript-driven: durable delegate tool results
and typed wakeup observations already live in history, so the provider input stays as stable
PI/system prompt plus transcript history.

Compaction is the special case, but the live ledger is not a provider input. For
top-level parent sessions, the provider compacts only transcript/model history
(including any older point-in-time ledgers already present as prior summary
text). After the provider returns, the daemon appends a fresh
`## Delegation state at compaction time` section to the stored compaction
summary. The ledger lists every delegation row for that parent session across
all statuses (`running`, `done`, `done_with_failures`, `cancelled`, `failed`),
with bounded subagent/progress details, `outcome` control data when
available, and artifact paths. It does not refresh artifacts or inline
transcript or final-message bodies. A `running` entry is a point-in-time compaction fact, not a
final outcome; later completion observations or `inspect_delegation` provide fresh
state. If older ledger text remains in prior summaries, the newly appended
ledger supersedes it by being the latest section. Subagent compactions do not
receive or append the parent ledger, sibling subagent state, or `## Current
delegations` information; subagents summarize only their own role contract,
delegated task, transcript/model history, and tool results/facts.

The web/inspector RPC surface remains `delegation.start_full`,
`delegation.start_readonly_fanout`, `delegation.status`, `delegation.cancel`, and `delegation.list`;
those names are client APIs, not the provider-visible model tool names.

`runtime/` keeps ordering-sensitive behavior in named phases instead of a generic
hook/event bus: queued inputs are persisted before dispatch, model dispatch is
gated before a provider task is spawned, and compaction resumes through the same
driver loop after its durable store update. The narrow extension precedent
remains `ToolRegistry`/`ToolExtension`, where the variation point is real and
does not own session durability.

`provider_runtime/` is itself split: `provider.rs`/`connections.rs` (selection + per-session connection cache), `requests.rs` (`run_model`), `auth_retry.rs` (Codex 401 retry wrapper), `compaction.rs` (provider-native compaction and parent-only post-compaction delegation ledger append), `context_accounting.rs` (pre-dispatch token gate), `prompt.rs` (PI.md render + skill discovery + stable prompt sections), `skills.rs` (`LoadSkill`), `web_tools.rs` (web_search/web_fetch sidecars), `transcript.rs` (model-context normalization). The adjacent `delegation_context.rs` builds the bounded compaction ledger for top-level parent sessions.

## Key types

- `AppState` (state.rs): cloneable handle shared by every connection and background task. Holds `Arc<PostgresAgentStore>`, `active: HashMap<session_id, Arc<Mutex<RuntimeSession>>>` (loaded live sessions), `session_driver_locks`, a `tasks` registry of running dispatch/compaction handles, a `broadcast::Sender<EventFrame>`, the `ToolRegistry`, the `ProviderConnectionRegistry`, the `WorkspaceManager`, and the `prompt_root` (nearest ancestor containing `PI.md`).
- `SessionDriver` (runtime/mod.rs): an RAII handle holding an owned guard on a per-session lock. All session-mutating handlers acquire one so work on a single session is strictly serialized while different sessions run concurrently.
- `RuntimeSession` (types.rs): an in-memory `AgentSession` plus its `SessionConfig`. Lives in `active` only while the session is doing work.
- `RpcMethod` (types.rs): the parse table mapping wire method strings to handlers. Unknown methods return `unknown_method`.
- `DispatchAction` (types.rs): a persisted action (`row_id`, `attempt_id`, `SessionAction`) paired with the `SessionConfig` to execute it under.

## How it works

### Accept and routing

`main` parses config, connects Postgres, and migrates. Before stale-action
cleanup it deterministically reconciles durable selected-subagent controls,
then recovers durable post-compaction dispatch intents. This ordering lets an
already-committed exact-child interrupt settle its captured generation before
any resumed model runner is registered; the following stale sweep protects
either class if recovery remains retryable. Each accepted TCP stream is
upgraded to a websocket and handled in its own task. The connection loop
multiplexes two sources: inbound request frames and the shared event broadcast.

```
TcpListener.accept -> spawn handle_socket
  loop select:
    reader.next  -> parse RpcRequest -> dispatch_request -> RpcResponse (id, ok, result|error)
    events_rx    -> if session subscribed and event_id past high-water, forward frame
```

Malformed JSON returns an `invalid_json` error with a null id rather than dropping the connection. `dispatch_request` matches the parsed `RpcMethod` to one async handler each.

### Per-session driver locks

Every handler that touches session state calls `SessionDriver::acquire`, which fetches (or lazily creates) an `Arc<Mutex<()>>` keyed by session id and takes an owned guard for the request's duration. The lock map is pruned of unreferenced entries on each acquire. This makes a single session's mutations serial regardless of how many connections target it, while leaving distinct sessions fully concurrent.

### Recovery before first touch

`recover_if_needed` runs at the start of read and input handlers. If the session is already loaded in `active`, it is a no-op. Otherwise it resets abandoned `consuming` inputs, then short-circuits if the persisted active leaf is already a turn boundary. Only when the stored tail is an open turn does it rebuild the `AgentSession` from the stored snapshot, persist any newly closed entries via `recover_session`, and, if the session is ready to continue, drive it. Source-mutating handlers (delete, configure with model change, history.switch, turn.resume, compaction.request) and the source-snapshotting `history.fork` handler instead call `ensure_idle_for_source_mutation`, which recovers and then rejects with `session_busy` if any work is in flight. Both `history.switch` and `history.fork` also reject while any delegation for the source session is running. Their store transactions lock the source session row before rechecking that invariant; delegation creation locks the same parent row before checking and inserting its running record, so the idle-only contract is race-safe.

Fork keeps the source driver lock across the current-cwd clone and store transaction, clones only a project session's owned managed cwd, and creates an independent top-level session with the complete transcript forest and an exact switch-valid active boundary. Daemon-managed local/MCP tool futures hold the per-cwd mutation guard for their lifetime. Fork and read-only subagent snapshotting acquire the same exact-cwd guard while holding the parent driver and wait for an active tool future to finish. Full subagents share the parent cwd and are not snapshot-guarded. Cancellation does not hold the parent driver: it wins the delegation cancellation transition and aborts or interrupts child work, dropping the daemon-managed tool future and its cwd guard. Delegation artifacts live under `.pi-handoff`, which is excluded from every fork/read-only clone.

### Driving the loop

`drive_until_blocked` is the core pump. It loads the session if needed, then repeatedly: consumes a ready steer input, persists any session outputs, dispatches resulting actions, and otherwise pulls the next queued input. When no work remains it removes the session from `active`, emits `SessionIdle`, and clears the persisted event buffer if the session settled idle. Persisted outputs go through `persist_active_outputs`, which drains the live session, writes entries/events/actions in one store call, publishes the event frames, and returns `DispatchAction`s.

### Automatic tool dispatch

Tool actions are dispatched immediately. `spawn_claimed_dispatch` runs `run_tool_turn` in a registered background task: it marks the action running, ensures the workspace, executes the tool, feeds the `ToolResultMessage` back into the live session, drains, and re-drives. Runtime/local tools such as `LoadSkill`, the web tools (`web_search`/`web_fetch`), and delegation tools are handled in-daemon; provider-executed registry tools route through the `ToolRegistry` keyed by provider kind as appropriate. There is no approval interface — tools execute automatically.

### Model dispatch, retries, and auth recovery

Ordinary model actions are claimed atomically (`claim_pending_model_action`)
before `run_model_turn` runs. The narrower post-compaction resume path uses the
durable lease described below instead: a unique owner/generation is registered
in the retained intent before provider work starts, and completion is fenced by
that lease. `run_model` assembles the prompt, builds the request from
`SessionConfig`, picks a provider, and completes through
`complete_with_auth_retry`. Provider connections are cached per
`(session_id, provider)` in `ProviderConnectionRegistry`; OpenAI always routes
through the ChatGPT/Codex subscription transport, Claude through the Anthropic
API-key adapter.

Two retry layers exist:

- Model dispatch retries every `ProviderError` up to `MODEL_PROVIDER_MAX_ATTEMPTS` (5) with 250ms/1s/3s backoff, re-checking that the action can still complete between attempts. After exhaustion, the provider diagnostic is recorded on the failed model action; context-overflow errors still feed the reactive compaction recovery path instead of becoming ordinary turn failures.
- A Codex 401 can also trigger exactly one inner `refresh_codex_credentials` cycle inside a single provider call, refreshing the ChatGPT token in `~/.codex/auth.json`, rebuilding the provider, and retrying the same call once. This is the only auth fallback (see [Codex Auth Recovery](../design-decisions.md#codex-auth-recovery-is-narrow-and-explicit)).

`MaxOutputTokens` stops are recorded as an action error with the assistant content preserved; `Complete` feeds `ModelCompleted` back into the session.

### Auto-compaction with circuit breaker

Before a model action dispatches, `gate_model_dispatch` asks every provider for
`ModelProvider::model_metadata`, measures input tokens, and blocks the action if
it would exceed the resolved automatic limit. Adapters own discovery and
provider-specific threshold recommendations: OpenAI's authenticated catalog
uses 90% of its resolved current/default context window (or a smaller explicit
provider limit), while Anthropic preserves the verified 1M→500k policy and
uses the generic 85% recommendation otherwise. The daemon no longer contains
OpenAI model-id, effort, context-window, or threshold rows. Metadata lookup
uses the same one-time Codex 401 refresh/rebuild path as generation. Token
accounting differs by provider: Claude uses the authoritative remote
token-count preflight; OpenAI/Codex has no usable remote count endpoint, so it
anchors on the latest provider-reported usage and estimates only the local
transcript suffix appended after that point.

The daemon reads scheduler controls only from `/compaction/config`; sibling
fields on `/compaction` are ordinary metadata and have no scheduler effect.
Unknown nested fields, including the store-owned
`max_consecutive_failures`, are ignored. A malformed active scheduler field
disables automatic compaction. Precedence is:

1. a valid explicit session context/automatic limit, safely clamped;
2. the adapter's recommended automatic limit;
3. the neutral 85% policy only when the adapter returned an authoritative
   resolved input window;
4. no proactive threshold, with reactive provider-overflow recovery still
   enabled.

The effective limit is floored at 8,000 without exceeding the selected window;
a smaller known window disables automatic compaction. Metadata failures do not
invent a threshold, while reactive overflow remains available when a request
can proceed without a proactive limit. These scheduler controls never select
the compaction algorithm: manual and automatic jobs always call
`ModelProvider::compact` once with the full selected transcript.

```
RequestModel ready to dispatch
  └─ eligible? (auto_enabled, not harness, leaf not last-failed, not suppressed)
       ├─ no  -> dispatch model
       └─ yes -> measure input tokens
            ├─ tokens < auto_limit -> dispatch model
            └─ tokens >= auto_limit -> block action, spawn compaction job
```

Compaction runs as its own background task (`run_compaction_job`). On success
it records the new compacted root, marks the provider connection compacted
(bumping the OpenAI window generation), and resumes the blocked model action.
Boundary compaction installs only the summary root. Mid-turn compaction appends
the open turn's exact user instructions after that root while leaving summarized
assistant/tool/daemon output out of the new branch; the final retained user is
therefore the resumed context leaf. The success transaction also leaves an
attempt-fenced durable dispatch intent on that exact pending action. Claim
retains that intent and atomically assigns a unique owner, incrementing
generation, and a 30-second lease; the registered runner renews it every 10
seconds. Daemon startup
preserves marked pending and running rows, validates the row/attempt/compacted
leaf, reconstructs the runtime even though `CompactionSummary` is a transcript
boundary, and claims pending or expired work. An unexpired lease is not
concurrently dispatched. A process-lifetime watchdog remains armed while the
daemon runs, normally sleeping until the next database-derived lease expiry;
heartbeat loss/error and leased-runner exit wake it immediately, and transient
database/recovery errors use bounded backoff instead of terminating recovery.
Heartbeat loss stops renewal but does not cancel the model future: the same
runner may just have atomically completed or entered reactive compaction and
must still register that durable successor. If ownership was actually lost, a
replacement registration after expiry aborts the old handle. Completion/error
is fenced by owner/generation and clears the intent in the same transaction.
Only typed structural marker corruption terminally records a model error;
SQL/pool/query/commit/context/runtime-load failures retain the marker for
watchdog retry. Harness sessions use the same lease but retain manual completion
ownership instead of starting an internal provider runner.

The in-process task registry uses an opaque registration id for each runner.
Installing a replacement generation explicitly aborts the old registered
handle, and runner cleanup removes the row entry only when that id still owns
it. A late generation-one exit therefore cannot unregister or classify a
generation-two runner. Shutdown and registration share one lock: shutdown marks
the state closing and drains dispatch runners/watchdog atomically with respect
to registration, while a rejected handle never crosses its provider start
barrier. A concurrently claimed durable lease remains for next-boot recovery.

This protocol is **at least once**, not exactly once. A crash after the provider
accepted a request but before the terminal Postgres commit can cause another
call after lease expiry. Neither the OpenAI/Codex nor Anthropic adapter supplies
a provider idempotency key for these model requests, so pi-relay cannot
eliminate that duplicate-call/cost window; the row/attempt/owner fences only
prevent duplicate durable completion. Replace this narrow payload-backed
protocol only after a general durable dispatch outbox/lease covers the same
claim, heartbeat, startup reclaim, and terminal-clear transitions.

A reactive path also fires: if the provider rejects a running request with a
context-overflow error, `recover_model_context_overflow_with_compaction` blocks
the running action and spawns a mid-turn compaction instead of failing the turn.
A compaction that blocks a concrete model action remains `MidTurn` even when its
source leaf is itself a `CompactionSummary`; the scope records the blocked
row/attempt so both completion and failure must resume or terminally fail it.

Both supported providers use native compaction unconditionally. Unsupported
model/capability, Anthropic's below-50K minimum, unexpected stop,
transport/HTTP, protocol/content, and context overflow are typed terminal
failures. A failed native request is not replaced by another algorithm and is
not retried with transcript history omitted. Persisted unknown metadata cannot
change execution.

The circuit breaker lives in compaction auto-state on session metadata. Each auto-compaction failure increments a consecutive-failure counter and records the failing leaf id; the eligibility check skips a leaf that just failed, skips when `suppressed`, and a successful ordinary model completion (including a max-output response with usage, but not refusal or an unexpected compaction stop) resets the failure counter. Compaction success commits the updated breaker/recompaction state with checkpoint installation and blocked-model resumption; failure commits failure metadata while terminally failing both blocked model and compaction actions. One immediate recompaction is allowed after a successful compacted request still overflows. If the request still overflows after a second successful compaction, recovery falls through to the ordinary terminal model-error path instead of starting an infinite loop. After success commits, the daemon reloads the compacted runtime and persisted config before claiming the pending model action. An installation/config/workspace/claim error compensates by terminally failing the unfinished action. After a successful claim, the daemon verifies that spawn synchronously registered a runner and applies the same fenced compensation if it did not; if another path already owns that exact lease generation, the live-runner check avoids a same-generation concurrent dispatch or false failure. Task-wrapper errors evict the stale live projection and re-run durable recovery after unregistering the compaction task. This prevents both endless compact/retry/overflow cycles and stranded blocked, pending, or unowned running model actions.

### Reconnect event replay

`events.subscribe` recovers the session, registers the subscription, and seeds a per-connection event high-water mark. Without `after_event_id` it attaches at the current head and replays nothing; with one it returns every persisted event after that id and advances the high-water mark past the replayed maximum. Live frames from the broadcast channel are forwarded only when the session is subscribed and the frame's id is past the high-water mark, so a replay and a concurrent live frame never duplicate. If the broadcast receiver lags, the loop falls back to reloading missed events from the store per subscribed session.

### Response shaping

`rpc_views.rs` converts store records to wire JSON. Session snapshots and active-branch syncs carry revision counters (`session_revision`, `queue_revision`, `transcript_revision`), `last_event_id`, and a `server_time_ms` wall-clock stamp the web client uses to anchor relative timers across reconnects.

## Notes

- `RuntimeSession` lives in `active` only while a session is working; the store is the source of truth and the in-memory session is dropped on idle, on persist failure, and when a boundary-scoped compaction starts.
- Recovery is mandatory before the first read/input on a cold session; skipping it would surface an open turn as a finished one.
- Source mutations require an idle session; metadata-only `session.configure`
  needs idle queue/actions but not full recovery. A session model cannot change
  once the first transcript entry exists (`provider_locked`). Provider-adjacent
  defaults such as reasoning effort may persist while work is active: queued
  input and action route snapshots keep already accepted/open-turn work stable,
  and the active runtime is not overwritten.
- `ProviderKind` is `{ OpenAi, Claude }`. `codex` is not a provider kind — it is the auth transport OpenAI always uses.
- `client_input_id` makes idle-accept and busy-queue sends idempotent: a replayed id returns the prior outcome without re-enqueuing.
- The dev harness methods (`harness.model.complete` / `harness.model.fail`) let tests resolve model actions deterministically; harness sessions skip provider dispatch and auto-compaction entirely.
- Cross-references: action FSM and queue semantics in [agent-session](./agent-session.md); persistence, recovery rows, and the input ledger in [agent-store](./agent-store.md); provider adapters in [agent-provider](./agent-provider.md); tool surfaces in [agent-tools](./agent-tools.md); prompt rendering in [agent-prompt](./agent-prompt.md); the method contract in [websocket-rpc](../websocket-rpc.md); the client in the [web UI](../../../packages/web/docs/web-ui.md).
