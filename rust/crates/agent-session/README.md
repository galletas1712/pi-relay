# agent-session

Durable session history and an async run-loop wrapper around the `agent-core`
FSM kernel.

## Responsibility

`agent-session` turns the pure `AgentCoreLoop` FSM into a stateful, editable
session. An `AgentSession` owns:

- `AgentCoreLoop` — deterministic turn/tool state.
- `TranscriptStore` — append-only forest of `TranscriptStorageNode`s with one
  active root-to-leaf path.
- `ActionQueue` — private bookkeeping for model/tool requests that have been
  exposed to the harness and have not completed yet.
- Compaction request state — queued or pending stateless model work used by
  requested compaction and auto-compaction.
- Action and event outboxes — ephemeral live outputs for the harness.

`session.drive()` is the only supported way to advance the FSM. It runs the core
to quiescence and absorbs every freshly produced `TranscriptItem` into the
store, which is the sole owner of durable model-visible history.

The session has two immediate history mutations: `compact(plan, summary)` and
`rewind(leaf_id)`. Both validate their operation-specific preconditions before
invalidating work. `compact` applies only to a plan that still matches the
current transcript at a turn boundary. `rewind` validates the requested target
first, then may interrupt active work before moving the leaf. Both preserve
queued user `Steer` / `FollowUp` inputs and rehydrate the core from the new
model context after a successful mutation. `request_compaction(settings)` is
the scheduled path; it queues compaction for the next safe model-context
barrier. `fork(leaf_id)` is separate because it creates a new session value
rather than mutating the source.

`AgentRunner` is the async I/O shell. Inputs arrive via a cloneable
`AgentInputHandle`; the runner calls `drive` in a loop and forwards each drained
`SessionAction` to a caller-supplied handler.

This crate does not own model calls, tool execution, cost tracking,
spawn/report routing, or control-plane scheduling. Those live in the harness /
`agent-orchestrator` and above.

## Public Interface

All intended downstream imports are re-exported from `lib.rs`.

**Composition types**

- `AgentSession` — constructors plus `drive`, `enqueue_input`,
  `enqueue_session_input`, `request_compaction`, `compact`, `rewind`, `fork`,
  `drain_actions`, `drain_events`, `drain_pending_inputs`, `model_context`, and
  `transcript_store`.
- `AgentRunner` — async wrapper that drives a session from an input channel.
- `AgentInputHandle`, `AgentInputHandleError`, `AgentInputReceiver` —
  sender/receiver pair for the runner.
- `SessionAction` — model/tool actions, session-wide `CancelSessionWork`, and
  session-owned `RequestModelStateless`. `RequestModel` carries the model
  context snapshot visible when the model request was made.
- `SessionInput`, `SessionInputError` — core inputs plus stateless model
  completions/failures.
- `SessionEvent` — live-only activity such as transcript append, action
  requested/completed/failed, and history edited.
- `SessionActionKind` — lightweight action event classifier.

**Durable state**

- `TranscriptStore` — append-only forest of `TranscriptStorageNode`s plus one
  active leaf.
- `TranscriptStorageNode` — `{ id, parent_id, timestamp_ms, item }`.
- `ModelContext` — read-only materialized view derived from the active path.
- `TranscriptStoreError` — `EntryNotFound`, `InvalidSpan`, `NotTurnBoundary`,
  `StalePlan`.

**History and compaction**

- `CompactionPlan`, `CompactionSettings` — prefix-compaction policy produced by
  `TranscriptStore::prepare_compaction`.
- `AutoCompactionSettings` — optional policy that queues compaction when the
  model context is over budget at a model-context barrier.
- `HistoryOperationError` — `Busy` or `Store(TranscriptStoreError)`.
- `StatelessModelRequest`, `StatelessModelRequestId`, `ModelContentBlock`,
  `ImageInput` — stateless side-model request vocabulary. Stateless completion
  currently returns text through `SessionInput::ModelStatelessCompleted`.
- `KIND_COMPACTION_SUMMARY = "compaction_summary"` and
  `compaction_summary(...)`.

**Re-exports from `agent-core`**

`AgentInput`, `AgentInputError`, `AgentAction`, `TranscriptItem`, `TurnId`,
`ActionId`, `ToolCallId`, `InjectedMessage`, `TurnOutcome`,
`AssistantMessage`, `AssistantItem`, `ToolCall`, `ToolResultMessage`, and
`ToolResultStatus`.

## Drive Cycle

```rust
session.enqueue_input(AgentInput::follow_up("hello"))?;
session.drive();

let actions = session.drain_actions();
let SessionAction::RequestModel {
    action_id,
    turn_id,
    model_context,
} = &actions[0] else {
    unreachable!()
};

let provider_request = build_provider_request(model_context);
session.enqueue_input(AgentInput::ModelCompleted {
    action_id: *action_id,
    turn_id: *turn_id,
    assistant,
})?;
session.drive();
```

`drive` records every visible `RequestModel` / `RequestTool` in the internal
action queue before callers drain the observable action outbox. Matching
`ModelCompleted`, `ModelFailed`, and `ToolCompleted` inputs clear those keys.
`ModelFailed` closes the turn as `Crashed`. Interrupts and stale-work
invalidation surface as `CancelSessionWork`, a best-effort, idempotent
instruction for the harness to cancel all external work for this session.

With auto-compaction enabled, the threshold policy checks context size only when
new model-context-producing work reaches a model barrier. If the context is over
budget, the session queues compaction through the same path as
`request_compaction(settings)`. When a compaction request starts, the session
emits `SessionAction::RequestModelStateless`; the harness runs that as a
stateless side-model call and returns `SessionInput::ModelStatelessCompleted {
text }` or `SessionInput::ModelStatelessFailed`. Success applies a compaction
summary to the transcript store, then releases any held `RequestModel`. Failure
releases the held model request unchanged so the agent still makes progress.
While compaction is pending, `drive()` does not consume queued `Steer` /
`FollowUp` inputs.

Restoring from transcript items or a transcript store is intentionally
quiescent. Open transcript tails are recovered as crashed, the core is rebuilt
idle at the recovered boundary, and auto-compaction is not started just because
the restored context is already over budget. Auto-compaction is checked later
when new input reaches a model-context barrier.

## History Operations

```rust
let plan = session
    .transcript_store()
    .prepare_compaction(settings)
    .expect("old context is compactable");

session.compact(plan, "summary")?;
session.rewind(Some(&leaf_id))?;
session.rewind(None)?;
session.request_compaction(settings);

let forked: AgentSession = session.fork(Some(&leaf_id))?;
```

`compact` and `rewind` are the only immediate mutations. Supplying a fresh model
context is a constructor concern (`from_model_context` / `from_transcript_store`),
not a history edit. Compaction internally replaces a planned span with an
injected summary and re-appends the kept suffix as a new branch; the old nodes
remain in the store for audit. A stale/non-boundary compaction plan or invalid
rewind target returns before cancellation or transcript mutation.

Scheduled compaction can start while the session is idle at a turn boundary, or
after the core emits `RequestModel` but before that action is exposed to the
harness. In the latter case the session holds the model action, emits
`RequestModelStateless`, applies the returned summary, and only then exposes the
original `RequestModel` with the updated `ModelContext`.

## Internals

### Module Map

| File | Contents |
| --- | --- |
| `src/lib.rs` | Module declarations and public re-exports. |
| `src/action.rs` | `SessionAction`, `StatelessModelRequestId`. |
| `src/input.rs` | `SessionInput`, `SessionInputError`. |
| `src/event.rs` | Runtime-only `SessionEvent`, `SessionActionKind`. |
| `src/auto_compaction.rs` | Auto-compaction settings, stateless model request types, compaction request rendering. |
| `src/session.rs` | `AgentSession`, drive/input/action lifecycle, immediate edits, scheduled compaction, restore rehydration. |
| `src/session/tests.rs` | Session lifecycle tests. |
| `src/action_queue.rs` | Private FIFO of visible in-flight model/tool actions. |
| `src/model_context.rs` | `ModelContext` read-only view and crashed-tail recovery. |
| `src/runner.rs` | `AgentRunner` async shell and input handle. |
| `src/transcript_store/mod.rs` | Transcript forest, entry/parent/leaf indexes, materialization, boundary checks. |
| `src/transcript_store/compaction.rs` | Prefix-compaction policy, planning, private span replacement, token estimation, summary injection, compaction summary kind. |

### Composition

```
 AgentSession
  ├── core: AgentCoreLoop
  ├── transcript_store: TranscriptStore
  ├── action_queue: ActionQueue
  ├── compaction_request_queue: VecDeque<QueuedCompactionRequest>
  ├── pending_compaction: Option<PendingCompaction>
  ├── action_outbox: VecDeque<SessionAction>
  └── event_outbox: VecDeque<SessionEvent>

 drive()        ─► core.drive()
               ├► drain transcript items → append to transcript store
               └► drain actions → maybe start compaction → action outbox
 enqueue_input  ─► validate → clear matching action_queue key → core.enqueue_input
 compact/rewind ─► invalidate session work → mutate transcript store → rehydrate core
 request_compaction
               └► queue compaction → run at next model-context barrier
```

### TranscriptStore Forest

Each `TranscriptStorageNode` has a UUID string id, an optional parent id,
timestamp, and one `TranscriptItem`. Entries form a forest: every entry has at
most one parent, and a parent may have many children. The store tracks one
active leaf; `model_context()` materializes exactly that root-to-leaf path.

`append_transcript_items` attaches new children under the active leaf.
`branch_at_turn_boundary(id)` moves the active leaf onto an existing boundary
entry; subsequent appends grow a new path off that node. Nothing is deleted.

Today each session owns an independent `TranscriptStore`. `fork(leaf)` copies
only the ancestor path from root to `leaf` into a new session; sibling branches,
abandoned descendants, queued inputs, in-flight actions, events, and other
sessions are not copied. A future shared store can make this a cheap second leaf
pointer without changing the public session operations.

Compaction in pictures:

```
Initial active path:
  E0 ── E1 ── E2 ── E3 ── E4 ── E5
                                ▲
                                leaf

After compaction over E2..E3:
  E0 ── E1 ── E2 ── E3 ── E4 ── E5      (old path remains in store)
          \
           Esum ── E4' ── E5'
                         ▲
                         leaf
```

`Esum` is an injected `compaction_summary`; `E4'` / `E5'` are re-appended copies
of the kept suffix.

### ActionQueue

Private to the crate. `PendingActionKey { action_id, turn_id, kind }` lives in a
`VecDeque`. `record_drained(&[AgentAction])` records visible `RequestModel` /
`RequestTool` work. `record_input(&AgentInput)` removes the matching key on
`ModelCompleted`, `ModelFailed`, or `ToolCompleted` and reports whether a live
action was cleared. Stale completions are silent no-ops. History operations and
interrupts do not wait on this queue; they clear it through session-wide
invalidation and emit `CancelSessionWork` when external work may be live.

### AgentRunner

`AgentRunner` is the only async surface in the crate. It owns an `AgentSession`,
an input receiver, and a `FnMut(SessionAction) -> impl Future<Output = ()>`
action handler. `run()` drives the session, flushes events and actions, then
awaits the next `SessionInput`.

`RequestModel` actions include the `ModelContext` snapshot needed to build the
provider request. The action handler should register or spawn long-running
model/tool work and return promptly, then enqueue completion or failure later
through an `AgentInputHandle`.

## Relationship To Other Crates

- **Upstream `agent-core`** — provides the FSM, mailbox input/action vocabulary,
  transcript item types, IDs, and message/tool-call structures. `agent-session`
  re-exports these for a single downstream import path.
- **Downstream `agent-orchestrator`** — owns `SessionRegistry<AgentSession>`,
  routes parent/child messages and reports through `enqueue_input`, invokes
  `compact`, `rewind`, `request_compaction`, and `fork`, and stays out of
  `TranscriptStore` internals.

For cross-cutting context such as control plane, usage, worklogs, and
multi-agent spawn/report, see `rust/docs/architecture.md`.
