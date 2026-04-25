# Rust agent stack — architecture and roadmap

This is the top-level plan for the Rust side of pi-relay. It describes the layer stack, the data model, the components that live in each layer, the runtime evolution from single-process to distributed, and the PR sequencing to get there.

Feature-specific design docs for worklogs, cost aggregation, and multi-agent
tooling are planned. Until those files exist, this document is the source of
truth for those roadmaps.

---

## 0. Principles

1. **Pure core, narrow boundaries.** The FSM is deterministic, has no I/O, and exposes a tiny public API. Everything that looks like a dependency of the FSM is a trait implemented outside the core.
2. **Data and policy split at layer boundaries.** Each layer owns data that the layer above can't see. Policy lives in the outermost layer. No reach-through access.
3. **Append-only, materialized on read.** Durable state is an append-only log. The "current view" is a function of the log plus ambient config — never a field that can drift.
4. **Traits are future process boundaries.** Every trait we introduce can, in principle, become a network protocol. Design APIs accordingly: async, request/response shapes, serializable types.
5. **One kind of thing, one place.** If we have two entry types that are "the same shape with a different tag," they collapse into one type with a tag. Applies to logs, events, injections.
6. **No features without consumers.** Primitives wait until the thing that consumes them exists. No speculative abstractions.

---

## 1. Layer stack

```
┌──────────────────────────────────────────────┐
│ 5. View layer — TUI, CLI, web UI, …          │
│    Holds Arc<dyn ControlPlane>. No direct    │
│    session access.                           │
└──────────────────────────────────────────────┘
                     ▲
                     │  ControlPlane trait (async RPC-shaped API)
                     ▼
┌──────────────────────────────────────────────┐
│ 4. Control plane — SessionRegistry,          │
│    routing, worklog triggers, ledger,        │
│    cross-session policy.                     │
└──────────────────────────────────────────────┘
                     ▲
                     │  owns a collection of AgentSession
                     ▼
┌──────────────────────────────────────────────┐
│ 3. Session — AgentSession, Context,          │
│    ContextEdit trait + ops, AgentRunner.     │
│    One per agent. Sole owner of durable log. │
└──────────────────────────────────────────────┘
                     ▲
                     │  owns an AgentCoreLoop
                     ▼
┌──────────────────────────────────────────────┐
│ 2. Core — AgentCoreLoop, Mailbox, AgentState.│
│    Pure FSM. No I/O, allocates only TurnId   │
│    and ActionId.                             │
└──────────────────────────────────────────────┘
                     ▲
                     │  produces records & actions
                     ▼
┌──────────────────────────────────────────────┐
│ 1. Vocabulary — TranscriptRecord,            │
│    AgentAction, AgentInput, AssistantMessage,│
│    ToolCall, ToolResult.                     │
│    Plain data. Shared by every layer above.  │
└──────────────────────────────────────────────┘
```

Each layer depends only on layers below. The view layer depends only on the control-plane trait, not on any session internals.

---

## 2. Data model

### Records, actions, inputs (layer 1, in `agent-core`)

```rust
// What the FSM produces as durable facts:
enum TranscriptRecord {
    TurnStarted { turn_id },
    UserMessage(String),
    AssistantMessage(AssistantMessage),
    ToolCallStarted { turn_id, tool_call },
    ToolResult(ToolResultMessage),
    TurnFinished { turn_id, outcome },
    Injected(InjectedMessage),
}

struct InjectedMessage {
    kind: String,
    content: String,
    metadata: BTreeMap<String, String>,
}

// What the FSM requests the outside world do:
enum AgentAction {
    RequestModel { action_id, turn_id },
    RequestTool { action_id, turn_id, tool_call },
    CancelTurn { turn_id },
}

// What the outside world feeds the FSM:
enum AgentInput {
    Interrupt,
    Steer    { from: Option<SessionId>, kind: Option<String>, content: String },
    FollowUp { from: Option<SessionId>, kind: Option<String>, content: String },
    ModelCompleted { action_id, turn_id, assistant },
    ModelFailed    { action_id, turn_id, error },
    ToolCompleted  { action_id, turn_id, result },
}
// `from` and `kind` are either both None (human/unknown origin) or both
// Some (agent-routed input such as a parent directive or child report).
// `action_id` must be copied from the matching RequestModel / RequestTool.
```

These are plain data shapes intended to be serializable at future persistence
and RPC boundaries, but the current crates do not derive or require serde yet.
The FSM never holds non-POD state beyond these.

### Log entries (layer 3, in `agent-session`)

```rust
struct SessionEntry {
    id: EntryId,
    parent_id: Option<EntryId>,
    timestamp_ms: u128,
    record: TranscriptRecord,
}
```

**Every durable "thing injected into the model's view" is a `TranscriptRecord::Injected(InjectedMessage)` with a kind tag.** One variant, one materialization path, one future storage/RPC shape. New feature -> new well-known kind string and metadata convention, not a new entry type. Ephemeral lifecycle signals remain `SessionEvent`s, not injected transcript records.

### Session events (layer 3, observable)

```rust
enum SessionEvent {
    RecordAppended { entry_id: EntryId, record: TranscriptRecord },
    ActionRequested { action: SessionAction },
    ActionCompleted { kind: SessionActionKind, id: String },
    ActionFailed { kind: SessionActionKind, id: String, error: String },
    ContextEdited { kind: ContextEditKind },
}
```

Each session currently exposes these through an event outbox; a later event bus
will turn them into subscribable streams. Events are runtime observations, not
additional transcript records, so observers such as a TUI, usage ledger, or
orchestrator can observe session activity without changing the durable context.

### Agent vs session identity

**An agent *is* a session.** `AgentId = SessionId`. A session's log is the agent's history. There is no separate "agent-level" state.

Multi-agent relationships (spawn parents, children) live in the `SessionRegistry`, not in the session itself.

---

## 3. Components

### Layer 2 — Core (`agent-core` crate)

Already landed in PR #62 + #63 refactors.

- **`AgentCoreLoop`** — FSM + mailbox + outboxes. Private fields. Public API: `new`, `resume_at_boundary`, `enqueue_input`, `drive`, `drain_records`, `drain_actions`, `is_idle`, `has_pending_work`, `last_turn_id`.
- **`AgentState`** — private. `Idle | RunningModel | RunningTools | ReadyToContinue`.
- **`Mailbox`** — private. Priority queue: Interrupt > ModelCompleted/ModelFailed/ToolCompleted > ContinueModel when `ReadyToContinue` > Steer > FollowUp.

### Layer 3 — Session (`agent-session` crate)

Partially landed in PR #63; decomposition planned.

- **`AgentSession`** — owns `AgentCoreLoop` + `Context`. The session is the sole owner of durable records. Runtime surface: `drive`, `enqueue_input`, `is_idle`, `has_pending_work`, `last_turn_id`, `transcript`, `drain_actions`. History-edit surface: `edit(pending, op)` dispatches a `ContextEdit` op struct; `fork(pending, leaf)` is a direct method that returns an unregistered child `AgentSession`.
- **`ContextEdit` trait + op structs** — each history-editing operation is its own struct (`SummarizeSpan { plan, summary }`, `Compact { plan, summary }`, `Rewind { leaf_id }`, `ReplaceTranscript { replacement }`) that implements `ContextEdit { type Output; fn apply(self, &mut Context) -> Result<Output, HistoryEditError> }`. The quiescence check runs once inside `AgentSession::edit` before dispatching to `apply`. Generic summary-span planning is a pure query on `Context` (`context.prepare_summary_span(first, last)`); prefix compaction is policy on top (`context.prepare_compaction(settings)`). `fork` stays a direct `AgentSession` method because it produces a new session value rather than mutating in place.
- **`Context`** — DAG of `SessionEntry`s with a leaf pointer. Pure data structure. Knows about branch-aware append, navigate, materialize.
- **`Transcript`** — materialized view of the current branch's records. Live transcripts preserve open turns; resume paths explicitly crash-recover any open tail.
- **`SessionAction`** — public harness-facing work item. `RequestModel { action_id, turn_id, transcript }` includes the transcript snapshot visible when the model request was made; tool and cancel actions carry only the ids/payloads needed to execute them. Session-owned stateless model requests are used for compaction side work.
- **`AgentRunner<HandleAction>`** — wraps an `AgentSession` + an input channel + an action handler. Its `run()` loop calls `session.drive()` and fans `SessionAction`s to the handler. Records auto-flow into the log; the runner does not expose them directly.
- **Future `SessionStore` trait** — pluggable durable storage. Planned defaults are `JsonlFileSessionStore` for disk and `InMemorySessionStore` for tests, swappable for `SqliteSessionStore` later.

### Layer 4 — Control plane (`agent-orchestrator` crate and new traits)

Partially landed as a placeholder; real shape below.

- **`SessionRegistry<S = AgentSession>`** — `HashMap<SessionId, S>` + parent-child map + helpers. Generic over session type so it can hold local `AgentSession` or remote `SessionHandle` without code changes. Pure data + lifecycle management.
- **`ControlPlane` trait** — the view's only handle on the system:
  ```rust
  trait ControlPlane: Send + Sync {
      async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CpError>;
      async fn enqueue_input(&self, id: &SessionId, input: AgentInput) -> Result<(), CpError>;
      async fn subscribe_events(&self, id: &SessionId) -> Result<EventStream, CpError>;
      async fn spawn_session(&self, req: SpawnRequest) -> Result<SessionId, CpError>;
      async fn request_boundary_op(&self, id: &SessionId, op: BoundaryOp) -> Result<(), CpError>;
  }
  ```
- **`LocalControlPlane`** (day-1 default) — implements `ControlPlane` by holding a `SessionRegistry<AgentSession>` and running sessions in-process. All methods are still `async fn` so the trait shape stays RPC-friendly, but local calls don't actually cross any boundary.
- **`RemoteControlPlane`** (future, daemon-day) — RPC client to a daemon that hosts `LocalControlPlane`.
- **`AgentOrchestrator`** — composition struct that wires everything together:
  ```rust
  struct AgentOrchestrator {
      registry: SessionRegistry,
      worklog_store: Arc<dyn AgentWorklogStore>,
      usage_ledger: Arc<dyn UsageLedger>,
      model_registry: Arc<dyn ModelRegistry>,
      tool_registry_factory: Arc<dyn ToolRegistryFactory>,
      event_bus: EventBus,
  }
  ```
  Orchestrator subscribes to every session's event stream, routes child-idle / child-report events to parents, triggers worklogs, records usage.

### Providers (new crates: `agent-providers`, `agent-tools-builtin`, `agent-model-*`)

- **`ModelProvider` trait** — `async fn complete(request: ModelRequest) -> ModelCompletion`. Each SDK gets an adapter crate (`agent-model-anthropic`, `agent-model-openai`).
- **`Tool` trait + `ToolRegistry`** — `trait Tool { async fn execute(args, ctx) -> ToolResult }`. Built-ins live in `agent-tools-builtin` (one file per tool). Extension-provided tools register through the same registry.
- **`Compactor`** — wraps a `ModelProvider` to summarize a `CompactionPlan`.
- **`AgentWorklogStore` trait** — per-agent side-store. Default file-backed impl (`{worklog_root}/{agent_id}.worklog`).
- **`UsageLedger` trait** — receives `UsageRecorded` events, aggregates, supports per-agent / per-tree queries.

---

## 4. Runtime model (evolution)

The code path is identical in all three modes; only the `ControlPlane` impl and the session-hosting strategy differ.

### Stage 1 — Single process (day 1)

```
┌────────────────────────────────────────────────┐
│ pi-relay CLI / TUI process                     │
│                                                │
│  ┌──────────────────────────────────────────┐  │
│  │ LocalControlPlane                        │  │
│  │   SessionRegistry<AgentSession>          │  │
│  │   (all sessions live in this process)    │  │
│  └──────────────────────────────────────────┘  │
│                                                │
│  View ──► Arc<dyn ControlPlane>                │
└────────────────────────────────────────────────┘
```

All sessions in one process. Control plane is a library. View is the same process.

### Stage 2 — Daemon (when detach becomes a requirement)

```
┌──────────────┐        ┌────────────────────────┐
│ TUI / CLI    │◄──RPC─►│ pi-relay daemon        │
│ (view-only)  │        │   LocalControlPlane    │
└──────────────┘        │   SessionRegistry      │
                        └────────────────────────┘
```

Same `LocalControlPlane` implementation, hosted by a daemon instead of the CLI. The TUI uses `RemoteControlPlane` which is a thin RPC client. View can close/reconnect without killing sessions. Multiple views can attach.

### Stage 3 — Distributed sessions (when scale becomes a requirement)

```
┌──────────┐      ┌───────────────────┐
│ TUI      │◄RPC─►│ Daemon            │
└──────────┘      │   Registry holds  │
                  │   SessionHandle   │
                  │   (remote clients)│
                  └─────┬──┬────────┬─┘
                    RPC │  │RPC     │RPC
                        ▼  ▼        ▼
                  ┌──────────┐ ┌──────────┐
                  │ Session  │ │ Session  │
                  │ process  │ │ process  │
                  │ (localhost)│(other host)│
                  └──────────┘ └──────────┘
```

Registry is generic: `SessionRegistry<SessionHandle>` instead of `SessionRegistry<AgentSession>`. A session process hosts exactly one session, runs its own `AgentRunner`, owns its local `SessionStore`. Control plane routes messages via RPC. Observers (usage ledger, worklog store) become shared services.

**The session layer code does not change across stages 1→2→3.** The control plane layer grows impls. The view layer never sees the difference.

---

## 5. Feature inventory

Each feature is a consumer of the layer stack. Here's how each one maps:

### Compaction

**Status**: data model landed (PR #63); executor pending.

- `context.prepare_summary_span(first, last)` is the generic primitive: replace a contiguous active-branch span with a summary and replay the suffix.
- `context.prepare_compaction(settings)` produces a `CompactionPlan` as prefix-compaction policy on top of that primitive.
- `Compactor::summarize(plan)` calls a `ModelProvider` to generate the summary string.
- `session.edit(pending, Compact { plan, summary })` applies the prepared summary span with a `compaction_summary` record.
- Orchestrator observes `SessionEvent::RecordAppended { record: TranscriptRecord::TurnFinished { .. } }` and checks thresholds; if tripped, drives the compaction pipeline.

### Rewind

**Status**: landed (PR #63).

- `session.edit(pending, Rewind { leaf_id: Some(leaf) })` moves the log's leaf pointer; the core is rehydrated from the new materialized view.

### Fork (as primitive)

**Status**: landed (PR #63).

- `session.fork(pending, Some(leaf))` returns an unregistered `AgentSession` with a copy of the ancestor path. Caller configures it (tool registry, initial injections, initial input) and registers it via `SessionRegistry`.

### Spawn (tool)

**Status**: not yet.

1. Parent agent's LLM emits `tool_call: spawn(prompt, tools, …)`.
2. `SpawnTool::execute` calls `orchestrator.spawn_child(parent_id, request)`.
3. Orchestrator: allocate child_id → construct a fresh `AgentSession` → configure the child's model/tool registry → append the requested spawn brief and optional worklog/context injections → enqueue the initial `FollowUp` → `registry.insert(child_id, child, parent=parent_id)` → start the child's `AgentRunner` task.
4. SpawnTool returns `ok({ child_id })` immediately. Parent turn continues.

The child is not a context fork of the parent. Model, tools, and inherited context are spawn-request policy: callers can keep them identical for delegation or choose a narrower/different setup for review, verification, or isolated sub-work.

### Multi-agent routing primitives

**Status**: landed.

`AgentOrchestrator::send_message(from, to, content)` and `send_report(from, content)` are the orchestrator-level routing primitives, both fire-and-forget. `send_message` validates that `to` is a direct child of `from` in the spawn tree and enqueues `AgentInput::steer_tagged(from, KIND_AGENT_DIRECTIVE, content)` on the child's mailbox; `send_report` validates that `from` has a spawn parent and enqueues `AgentInput::follow_up_tagged(from, KIND_AGENT_REPORT, content)` on the parent's mailbox. In both cases the paired `from` + `kind` tags propagate into the target's mailbox so the receiver materializes cross-session traffic as a typed `TranscriptRecord::Injected` rather than human user input. Invalid routes surface as `RouteError::{SenderNotFound, TargetNotFound, NotAChild, NoParent}`. These primitives back the `message` and `report` tools (TS parity: `packages/orchestrator/src/tools/{message,report}.ts`).

### Report (tool)

**Status**: not yet.

1. Child's LLM emits `tool_call: report(content)`.
2. `ReportTool::execute` calls `orchestrator.route_report(from=child_id, content)`.
3. Orchestrator looks up `parent = registry.parent(child_id)` and enqueues a tagged `FollowUp` on the parent's mailbox. The parent materializes it as `TranscriptRecord::Injected(InjectedMessage { kind: "agent_report", ... })` when the report starts a turn.
4. ReportTool returns `ok`. Child turn continues.

### agent_idle notification

**Status**: not yet. Requires event subscription (SessionEvent stream).

1. Child's FSM reaches `Idle` after a graceful `TurnFinished`.
2. Orchestrator's event subscriber sees `SessionEvent::RecordAppended { record: TranscriptRecord::TurnFinished { outcome: Graceful, .. }, .. }`.
3. If `registry.parent(child_id)` exists, enqueue a tagged `FollowUp` on the parent's mailbox. The parent materializes it as an injected `agent_idle` record when that input starts a turn.

### Worklog

**Status**: not yet. This section is the current worklog roadmap until a
dedicated design doc exists.

- Orchestrator observes appended `TurnFinished` records, gates on `is_likely_trivial_turn`, serializes per-agent, forks parent at boundary with a single-tool registry (`[WorklogUpdateTool]`) and a `WorklogFraming` injection.
- The fork's LLM optionally calls `worklog_update`, which writes to `AgentWorklogStore` (**not** to the session log).
- Fork session is discarded on idle. Output lives in the side-store; ancestor worklogs are injected into descendants at prompt-assembly time.

### Cost aggregation

**Status**: not yet. This section is the current cost-aggregation roadmap until
a dedicated design doc exists.

- Every `ModelProvider::complete` call carries a `UsageContext { agent_id, scope, turn_id, model, cache_scope }`.
- On completion, orchestrator emits `SessionEvent::UsageRecorded { ctx, usage }`.
- `UsageLedger` subscribes and aggregates. Queries walk the agent tree (supplied by registry) for roll-up.

### Pluggable session storage

**Status**: not yet.

- `SessionStore` trait: `append`, `set_leaf`, `load`.
- `AgentSession` holds a `Box<dyn SessionStore>`; every `append_*` on the log mirrors to the store.
- Default impls: `InMemorySessionStore`, `JsonlFileSessionStore`.

---

## 6. PR sequencing

Each row is one landable PR. Later PRs depend on their predecessors.

| # | PR | What it adds | Unlocks |
|---|---|---|---|
| 1 | **#63 foundation** | `agent-session`, `agent-orchestrator` crates + Transcript unification + boundary seal + InjectedMessage unification + session-aware runner | every item below |
| 2 | Session decomposition | `ContextEdit` trait + op structs + `SessionRegistry<S>` + orchestrator becomes composition struct | clean target for registry-level features |
| 3 | `SessionStore` | trait + in-memory + JSONL-file impls + wire into `AgentSession` | durable restart; resume-from-file; pluggable backends |
| 4 | `ControlPlane` trait | trait definition + `LocalControlPlane` impl + view-layer adapter | view/control separation; future daemon |
| 5 | `SessionEvent` stream | event bus + subscription on `AgentSession`; durable events mirror log writes | observers (TUI, ledger, idle watcher) |
| 6 | `ModelProvider` trait | trait + Anthropic adapter + `UsageContext` + retry wrapper | actual model calls; compaction executor; worklog fork model |
| 7 | `Tool` + `ToolRegistry` | trait + built-in tool pack (bash/read/write/edit/grep/find/ls) | tool execution; spawn/report/worklog tools |
| 8 | `Compactor` + auto-compaction | summarize plans via `ModelProvider`; orchestrator threshold watcher | production-grade context management |
| 9 | `UsageLedger` | trait + in-memory impl + roll-up queries | cost observability; TUI footer |
| 10 | Spawn + report + agent_idle | `SpawnTool`, `ReportTool`, idle-watcher in orchestrator; new injected-message kind constants | multi-agent operation |
| 11 | `AgentWorklogStore` + worklog fork | trait + file-backed impl + `WorklogUpdateTool` + orchestrator worklog scheduler | per-agent durable knowledge; ancestor worklog injection for spawned sub-agents |
| 12 | `PromptAssembly` | system-prompt assembly from tool/skill/persona sources; ancestor-worklog prefix injection | feature parity with TS `_rebuildSystemPrompt` |
| 13 | Daemon + `RemoteControlPlane` | host `LocalControlPlane` in a daemon; RPC client; TUI reconnect | detachable view |
| 14 | Distributed session processes | `SessionHandle` as a session-shaped RPC client; `SessionRegistry<SessionHandle>`; cross-host spawn | agents on different hosts |

Rough mapping to user-visible capability:
- After PR #8: a Rust agent can run a full conversational turn with compaction.
- After PR #10: multi-agent, spawn-and-report works locally.
- After PR #13: you can close the TUI and agents keep running.
- After PR #14: agents can live anywhere.

---

## 6a. Design decisions pinned in PR #63

These are documented here so the PRs that implement them stick to the intended shape.

### Compaction is stop-the-world per session

The session being compacted is frozen for the entire flow: `prepare → summarize → compact`. While frozen, the session queues incoming inputs (tool completions, child reports, steer, follow-up) and does not acknowledge them to the FSM until the flow completes or aborts. Tool calls the session had in flight when the flow started continue to run externally and their results queue; they're replayed to the FSM once compaction finishes.

This closes the liveness hole where repeated appends during an async summarize call starve the compaction plan's staleness fingerprint. The `Compactor` PR lands the mechanism (likely a `SessionPhase::EditingHistory { queued_inputs }` on `AgentSession` with `begin_history_edit` / `commit_history_edit` / `abort_history_edit` transitions); the `ContextEdit` trait's `apply` becomes a witness over the edit phase rather than holding `&mut self` across await.

**Parents can compact while children are still running.** Children are separate sessions with separate logs; a parent's compaction has no effect on any descendant.

### Child reports are mailbox inputs, not history edits

When a child emits `report(content)`, the orchestrator does **not** open `edit_history` on the parent. Instead the report enters the parent's mailbox as a tagged `AgentInput::FollowUp` via `follow_up_tagged(from, KIND_AGENT_REPORT, content)`. The parent's FSM materializes it as a `TranscriptRecord::Injected` in the log when it next transitions from `Idle` to `RunningModel` — not before. Same applies to future agent-visible notifications: use tagged mailbox input for messages that should become model-visible context, and keep purely live signals as `SessionEvent`s.

This keeps `AgentSession::edit`/`fork` for genuine structural edits (compact, rewind, fork, replace_transcript) and removes the `entry_count`-churning source of compaction plan staleness.

### Spawn is fire-and-forget and creates a fresh session

The `spawn` tool returns immediately with a `{ child_id }` handle; the child runs asynchronously in its own session. The child is **not** a fork of the parent. It's constructed fresh, with whatever system prompt, tool registry, and model the spawn call specifies.

Ancestor worklog injection is **configurable** per spawn: some use cases (planning, delegation) benefit from inheriting prior context; others (code review, adversarial verification) want truly independent eyes.

Spawn does not require the parent to be at a turn boundary — a spawn tool invocation during `RunningTools` is fine. Because spawn doesn't touch the parent's log or fork from the parent's state, there's no boundary constraint.

---

## 7. Non-goals

- No support for session transcripts that aren't at turn boundaries during structural ops. Compaction/rewind/fork always require a quiescent turn boundary. Enforced by `AgentSession::edit`/`fork`'s quiescence check.
- No speculative abstraction for "hooks" that don't have concrete use cases. When extensions land, their API grows out of what concrete consumers need, not ahead of them.
- No speculative support for non-Anthropic-shaped providers beyond what `ModelProvider` naturally allows. Any provider that fits `messages + tools → assistant with tool_calls` works; we don't pre-bake support for genuinely different paradigms (e.g. step-wise agent protocols) until we have one.
- No back-compat with the TS session format *as a byte format* — the JSONL backend may use the same layout for cross-implementation convenience, but we don't pin wire-format compatibility as a hard constraint.
- No in-tree persistent daemon until the detach requirement becomes concrete. The `ControlPlane` trait is enough to preserve the option.

---

## 8. Principles in tension (explicit trade-offs)

**Clean boundaries vs. API ergonomics.** The `session.edit(pending, SummarizeSpan { plan, summary })?` form is more verbose than a single `session.compactAtBoundary(plan, summary, work)` helper. Accepting the verbosity because it encodes which operations are history-edit-only in the type system.

**Distributed-ready API vs. sync simplicity.** `ControlPlane` is async + fallible even when called in-process. Paying this for day-1 local use to keep daemon-day migration trivial.

**Two tracking layers (log DAG + registry tree).** Not a duplication — the log DAG tracks intra-session branching and summary-span rewrites; the registry tracks inter-session spawn relationships. Use distinct terminology in code (`previous_entry_id` vs. `spawn_parent`) to reinforce.

**Unified `InjectedMessage` vs. per-kind entry types.** Unified. Every "summary / note / report injected at a boundary" is one enum variant with a kind tag. TS has 3+ entry types for this; we have 1 + extensible tag. New kinds don't require new materialization branches.

**Worklog *not* in session log.** Deliberate departure from what would be "one unified abstraction for everything." Worklog content is free-form, long-lived, agent-level knowledge. Putting it in the session log would make it ride along on compaction/rewind/fork in ways that are wrong. Side-store is correct even though it's a second storage surface.
