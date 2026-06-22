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
                   prompt render, atomic session/output persist, initial dispatch
config.rs          --database-url / --bind, DATABASE_URL / PI_AGENTD_BIND
types.rs           RpcRequest/Response/Error, RpcMethod parse table, DispatchAction, RuntimeSession
state.rs           AppState: repo handle, active sessions, driver locks, task registry,
                   event broadcaster, tool registry, provider connections, workspaces
codec.rs           JSON <-> vocab parsing + transcript-store reconstruction helpers
auth.rs            credential loading (Codex/Anthropic) + Codex 401 token refresh
runtime/           SessionDriver facade plus concrete lifecycle phases:
                   events, outputs, task registry, dispatch, model, tool, compaction
workspaces/        workspace base refresh, local/git source handling, sanitization,
                   and session instantiation (btrfs/reflink/copy fallback)
rpc_views.rs       response shaping (snapshots, queue state, transcript views, server_time_ms)
model_metadata.rs  per-model context windows + 85% auto-compaction default limit
provider_runtime/  provider selection, model/web-tool execution, compaction, token accounting
subagents.rs       delegation subagent spawn core: role resolution, full vs
                   read-only workspace handling, child prompt + lifecycle events
delegation_tools.rs     delegation tool surface (delegate_writing_task /
                   delegate_readonly_tasks / inspect_delegation /
                   cancel_delegation / steer_subagent) plus delegation.* web RPCs
                   (start_full / start_readonly_fanout / status / cancel /
                   list) + homogeneity/one-delegation-per-parent guards
delegation_runner.rs    delegation barrier: all-terminal detect, attempt-fenced finish CAS,
                   idempotent handoff write, one steer to the parent; boot
                   crash sweep
handoff.rs         renders per-subagent final_message.md / transcript.md
                   from the durable transcript
```

Subagent work runs as **delegations** (`delegate_writing_task` /
`delegate_readonly_tasks` / `inspect_delegation` / `cancel_delegation` /
`steer_subagent`). Full subagents
reuse the parent's workspace dirs in place; read-only subagents get a forked
snapshot destroyed on return. Delegation subagents may emit
`subagent.spawned`/`subagent.running` progress events; their terminal hook fires
a single-flight, `attempt_id`-fenced barrier when all subagents of a delegation are
terminal. After the DB finish CAS wins, the runner writes the handoff directory
and then enqueues one `InputPriority::Steer` notification to the parent.
Completion is that steer/handoff, not a parent-visible per-child idle event. The
runner never decides the next delegation — the parent does, guided by workflow
skills. Cancellation is terminal and exports transcript-only files for the
cancelled subagents instead of running the normal completion handoff.

The web/inspector RPC surface remains `delegation.start_full`,
`delegation.start_readonly_fanout`, `delegation.status`, `delegation.cancel`, and `delegation.list`;
those names are client APIs, not the provider-visible model tool names.

`runtime/` keeps ordering-sensitive behavior in named phases instead of a generic
hook/event bus: queued inputs are persisted before dispatch, model dispatch is
gated before a provider task is spawned, and compaction resumes through the same
driver loop after its durable store update. The narrow extension precedent
remains `ToolRegistry`/`ToolExtension`, where the variation point is real and
does not own session durability.

`provider_runtime/` is itself split: `provider.rs`/`connections.rs` (selection + per-session connection cache), `requests.rs` (`run_model`), `auth_retry.rs` (Codex 401 retry wrapper), `compaction.rs` (remote/local compaction), `context_accounting.rs` (pre-dispatch token gate), `prompt.rs` (PI.md render + skill discovery), `skills.rs` (`LoadSkill`), `web_tools.rs` (web_search/web_fetch sidecars), `transcript.rs` (model-context normalization).

## Key types

- `AppState` (state.rs): cloneable handle shared by every connection and background task. Holds `Arc<PostgresAgentStore>`, `active: HashMap<session_id, Arc<Mutex<RuntimeSession>>>` (loaded live sessions), `session_driver_locks`, a `tasks` registry of running dispatch/compaction handles, a `broadcast::Sender<EventFrame>`, the `ToolRegistry`, the `ProviderConnectionRegistry`, the `WorkspaceManager`, and the `prompt_root` (nearest ancestor containing `PI.md`).
- `SessionDriver` (runtime/mod.rs): an RAII handle holding an owned guard on a per-session lock. All session-mutating handlers acquire one so work on a single session is strictly serialized while different sessions run concurrently.
- `RuntimeSession` (types.rs): an in-memory `AgentSession` plus its `SessionConfig`. Lives in `active` only while the session is doing work.
- `RpcMethod` (types.rs): the parse table mapping wire method strings to handlers. Unknown methods return `unknown_method`.
- `DispatchAction` (types.rs): a persisted action (`row_id`, `attempt_id`, `SessionAction`) paired with the `SessionConfig` to execute it under.

## How it works

### Accept and routing

`main` parses config, connects Postgres, migrates, and sweeps abandoned unfinished actions to stale. Each accepted TCP stream is upgraded to a websocket and handled in its own task. The connection loop multiplexes two sources: inbound request frames and the shared event broadcast.

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

`recover_if_needed` runs at the start of read and input handlers. If the session is already loaded in `active`, it is a no-op. Otherwise it resets abandoned `consuming` inputs, then short-circuits if the persisted active leaf is already a turn boundary. Only when the stored tail is an open turn does it rebuild the `AgentSession` from the stored snapshot, persist any newly closed entries via `recover_session`, and, if the session is ready to continue, drive it. Source-mutating handlers (delete, configure with model change, history.switch, turn.resume, compaction.request) instead call `ensure_idle_for_source_mutation`, which recovers and then rejects with `session_busy` if any work is in flight.

### Driving the loop

`drive_until_blocked` is the core pump. It loads the session if needed, then repeatedly: consumes a ready steer input, persists any session outputs, dispatches resulting actions, and otherwise pulls the next queued input. When no work remains it removes the session from `active`, emits `SessionIdle`, and clears the persisted event buffer if the session settled idle. Persisted outputs go through `persist_active_outputs`, which drains the live session, writes entries/events/actions in one store call, publishes the event frames, and returns `DispatchAction`s.

### Automatic tool dispatch

Tool actions are dispatched immediately. `spawn_claimed_dispatch` runs `run_tool_turn` in a registered background task: it marks the action running, ensures the workspace, executes the tool, feeds the `ToolResultMessage` back into the live session, drains, and re-drives. Runtime/local tools such as `LoadSkill`, the web tools (`web_search`/`web_fetch`), and delegation tools are handled in-daemon; provider-executed registry tools route through the `ToolRegistry` keyed by provider kind as appropriate. There is no approval interface — tools execute automatically.

### Model dispatch, retries, and auth recovery

Model actions are claimed atomically (`claim_pending_model_action`) before `run_model_turn` runs, so a single action is never executed twice. `run_model` assembles the prompt, builds the request from `SessionConfig`, picks a provider, and completes through `complete_with_auth_retry`. Provider connections are cached per `(session_id, provider)` in `ProviderConnectionRegistry`; OpenAI always routes through the ChatGPT/Codex subscription transport, Claude through the Anthropic API-key adapter.

Two retry layers exist:

- Transient provider errors retry up to `MODEL_PROVIDER_MAX_ATTEMPTS` (3) with 250ms/1s/3s backoff, re-checking that the action can still complete between attempts.
- A Codex 401 triggers exactly one `refresh_codex_credentials` cycle, which refreshes the ChatGPT token in `~/.codex/auth.json`, rebuilds the provider, and retries the same call once. This is the only auth fallback (see [Codex Auth Recovery](../design-decisions.md#codex-auth-recovery-is-narrow-and-explicit)).

`MaxOutputTokens` stops are recorded as an action error with the assistant content preserved; `Complete` feeds `ModelCompleted` back into the session.

### Auto-compaction with circuit breaker

Before a model action dispatches, `gate_model_dispatch` measures input tokens and blocks the action if it would exceed the model's auto limit. `model_metadata.rs` supplies per-model context windows; the default auto limit is 85% of the window (`window * 85 / 100`). Token accounting differs by provider: Claude uses the authoritative remote token-count preflight; OpenAI/Codex has no usable remote count endpoint, so it anchors on the latest provider-reported usage and estimates only the local transcript suffix appended after that point.

```
RequestModel ready to dispatch
  └─ eligible? (auto_enabled, not harness, leaf not last-failed, not suppressed)
       ├─ no  -> dispatch model
       └─ yes -> measure input tokens
            ├─ tokens < auto_limit -> dispatch model
            └─ tokens >= auto_limit -> block action, spawn compaction job
```

Compaction runs as its own background task (`run_compaction_job`). On success it records the new compacted root, marks the provider connection compacted (bumping the OpenAI window generation), and resumes the blocked model action from the compacted root. A reactive path also fires: if the provider rejects a running request with a context-overflow error, `recover_model_context_overflow_with_compaction` blocks the running action and spawns a mid-turn compaction instead of failing the turn.

The circuit breaker lives in compaction auto-state on session metadata. Each auto-compaction failure increments a consecutive-failure counter and records the failing leaf id; the eligibility check skips a leaf that just failed, skips when `suppressed`, and a successful model response (any usage) resets the failure counter. This prevents an endless compact/retry/overflow loop on a context that cannot be shrunk.

### Reconnect event replay

`events.subscribe` recovers the session, registers the subscription, and seeds a per-connection event high-water mark. Without `after_event_id` it attaches at the current head and replays nothing; with one it returns every persisted event after that id and advances the high-water mark past the replayed maximum. Live frames from the broadcast channel are forwarded only when the session is subscribed and the frame's id is past the high-water mark, so a replay and a concurrent live frame never duplicate. If the broadcast receiver lags, the loop falls back to reloading missed events from the store per subscribed session.

### Response shaping

`rpc_views.rs` converts store records to wire JSON. Session snapshots and active-branch syncs carry revision counters (`session_revision`, `queue_revision`, `transcript_revision`), `last_event_id`, and a `server_time_ms` wall-clock stamp the web client uses to anchor relative timers across reconnects.

## Notes

- `RuntimeSession` lives in `active` only while a session is working; the store is the source of truth and the in-memory session is dropped on idle, on persist failure, and when a boundary-scoped compaction starts.
- Recovery is mandatory before the first read/input on a cold session; skipping it would surface an open turn as a finished one.
- Source mutations require an idle session; metadata-only `session.configure` needs idle queue/actions but not full recovery. A session model cannot change once the first transcript entry exists (`provider_locked`).
- `ProviderKind` is `{ OpenAi, Claude }`. `codex` is not a provider kind — it is the auth transport OpenAI always uses.
- `client_input_id` makes idle-accept and busy-queue sends idempotent: a replayed id returns the prior outcome without re-enqueuing.
- The dev harness methods (`harness.model.complete` / `harness.model.fail`) let tests resolve model actions deterministically; harness sessions skip provider dispatch and auto-compaction entirely.
- Cross-references: action FSM and queue semantics in [agent-session](./agent-session.md); persistence, recovery rows, and the input ledger in [agent-store](./agent-store.md); provider adapters in [agent-provider](./agent-provider.md); tool surfaces in [agent-tools](./agent-tools.md); prompt rendering in [agent-prompt](./agent-prompt.md); the method contract in [websocket-rpc](../websocket-rpc.md); the client in the [web UI](../../../packages/web/docs/web-ui.md).
