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
- Compaction request state — queued or pending remote compaction API work used
  by manual compaction and auto-compaction.
- Action and event outboxes — ephemeral live outputs for the harness.

`session.drive()` is the only supported way to advance the FSM. It runs the core
to quiescence and absorbs every freshly produced `TranscriptItem` into the
store, which is the sole owner of durable model-visible history.

`compact()` queues remote compaction for the next safe model-context barrier.
The harness receives `SessionAction::RequestCompaction { model_context }`,
calls the remote compaction API, and returns
`SessionInput::CompactionCompleted { replacement }`. The session installs that
replacement as a new active path in the transcript store. `rewind(leaf_id)` is
the immediate history mutation: it validates the target first, may interrupt
active work, then moves the active leaf and rehydrates the core. Queued user
`Steer` / `FollowUp` inputs survive both paths. `fork(leaf_id)` creates a new
session value rather than mutating the source.

This crate does not own model calls, tool execution, cost tracking,
spawn/report routing, or control-plane scheduling. Those live in the harness /
`agent-orchestrator` and above.

## Public Interface

All intended downstream imports are re-exported from `lib.rs`.

**Composition types**

- `AgentSession` — constructors plus `drive`, `enqueue_input`,
  `enqueue_session_input`, `compact`, `rewind`, `fork`, `drain_actions`,
  `drain_events`, `drain_pending_inputs`, `model_context`, and
  `transcript_store`.
- `AgentRunner` — async wrapper that drives a session from an input channel.
- `AgentInputHandle`, `AgentInputHandleError`, `AgentInputReceiver` —
  sender/receiver pair for the runner.
- `SessionAction` — model/tool actions, session-wide `CancelSessionWork`, and
  session-owned `RequestCompaction`. `RequestModel` and `RequestCompaction`
  carry the model context snapshot visible when the request was made.
- `SessionInput`, `SessionInputError` — core inputs plus compaction
  completions/failures.
- `SessionEvent` — live-only activity such as transcript append, action
  requested/completed/failed, and history mutation events.
- `SessionActionKind` — lightweight action event classifier.

**Durable state**

- `TranscriptStore` — append-only forest of `TranscriptStorageNode`s plus one
  active leaf.
- `TranscriptStorageNode` — `{ id, parent_id, timestamp_ms, item }`.
- `ModelContext` — read-only materialized view derived from the active path.
- `TranscriptStoreError` — `EntryNotFound`, `NotTurnBoundary`.

**Compaction and rewind**

- `AutoCompactionSettings` — optional threshold policy that queues compaction
  when the model context is over budget at a model-context barrier.
- `CompactionRequestId` — correlation id for remote compaction requests.
- `HistoryOperationError` — `Busy` or `Store(TranscriptStoreError)`.

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
budget, the session queues compaction through the same path as `compact()`.
When compaction starts, the session emits
`SessionAction::RequestCompaction { request_id, model_context }`; the harness
calls the remote compaction API and returns
`SessionInput::CompactionCompleted { request_id, replacement }` or
`SessionInput::CompactionFailed`. Success replaces the active path with the
returned context, then releases any held `RequestModel`. Failure releases the
held model request unchanged so the agent still makes progress. While
compaction is pending, `drive()` does not consume queued `Steer` / `FollowUp`
inputs.

Restoring from transcript items or a transcript store is intentionally
quiescent. Open transcript tails are recovered as crashed, the core is rebuilt
idle at the recovered boundary, and auto-compaction is not started just because
the restored context is already over budget. Auto-compaction is checked later
when new input reaches a model-context barrier.

## History Operations

```rust
session.compact();
let SessionAction::RequestCompaction {
    request_id,
    model_context,
} = &session.drain_actions()[0] else {
    unreachable!()
};
let replacement = call_remote_compaction_api(model_context).await?;
session.enqueue_session_input(SessionInput::CompactionCompleted {
    request_id: *request_id,
    replacement,
})?;

session.rewind(Some(&leaf_id))?;
session.rewind(None)?;

let forked: AgentSession = session.fork(Some(&leaf_id))?;
```

`compact()` is asynchronous from the session's point of view: it queues a remote
compaction request and applies the returned replacement context when the harness
feeds back `CompactionCompleted`. The replacement is installed as a new active
path in `TranscriptStore`; old nodes remain in the store for audit. Compaction
can start while the session is idle at a turn boundary, or after the core emits
`RequestModel` but before that action is exposed to the harness. In the latter
case the session holds the model action, emits `RequestCompaction`, applies the
replacement context, and only then exposes the original `RequestModel` with the
updated `ModelContext`.

`rewind` is immediate. It validates the target boundary before cancellation or
mutation, then invalidates live work, moves the leaf, and rehydrates the core.

## Internals

### Module Map

| File | Contents |
| --- | --- |
| `src/lib.rs` | Module declarations and public re-exports. |
| `src/action.rs` | `SessionAction`. |
| `src/input.rs` | `SessionInput`, `SessionInputError`. |
| `src/event.rs` | Runtime-only `SessionEvent`, `SessionActionKind`. |
| `src/compaction.rs` | Auto-compaction settings, request ids, and token estimation. |
| `src/session.rs` | `AgentSession`, drive/input/action lifecycle, remote compaction, rewind, restore rehydration. |
| `src/session/tests.rs` | Session lifecycle tests. |
| `src/action_queue.rs` | Private FIFO of visible in-flight model/tool actions. |
| `src/model_context.rs` | `ModelContext` read-only view and crashed-tail recovery. |
| `src/runner.rs` | `AgentRunner` async shell and input handle. |
| `src/transcript_store/mod.rs` | Transcript forest, entry/parent/leaf indexes, materialization, boundary checks. |

### Composition

```
 AgentSession
  ├── core: AgentCoreLoop
  ├── transcript_store: TranscriptStore
  ├── action_queue: ActionQueue
  ├── compaction_request_queue: VecDeque<CompactionRequestSource>
  ├── pending_compaction: Option<PendingCompaction>
  ├── action_outbox: VecDeque<SessionAction>
  └── event_outbox: VecDeque<SessionEvent>

 drive()       ─► core.drive()
              ├► drain transcript items → append to transcript store
              └► drain actions → maybe start compaction → action outbox
 enqueue_input ─► validate → clear matching action_queue key → core.enqueue_input
 compact()    ─► queue compaction → run at next model-context barrier
 rewind       ─► invalidate session work → move transcript leaf → rehydrate core
```

### TranscriptStore Forest

Each `TranscriptStorageNode` has a UUID string id, an optional parent id,
timestamp, and one `TranscriptItem`. Entries form a forest: every entry has at
most one parent, and a parent may have many children. The store tracks one
active leaf; `model_context()` materializes exactly that root-to-leaf path.

`append_transcript_items` attaches new children under the active leaf.
`branch_at_turn_boundary(id)` moves the active leaf onto an existing boundary
entry; subsequent appends grow a new path off that node. Remote compaction
replacement resets the active leaf to the root and appends the replacement
context as a new path. Nothing is deleted.

Today each session owns an independent `TranscriptStore`. `fork(leaf)` copies
only the ancestor path from root to `leaf` into a new session; sibling branches,
abandoned descendants, queued inputs, in-flight actions, events, and other
sessions are not copied. A future shared store can make this a cheap second leaf
pointer without changing the public session operations.

Remote compaction replacement in pictures:

```
Initial active path:
  E0 -- E1 -- E2 -- E3 -- E4 -- E5
                                ^
                                leaf

After the compaction API returns replacement context:
  E0 -- E1 -- E2 -- E3 -- E4 -- E5      (old path remains in store)
  R0 -- R1 -- R2
              ^
              leaf
```

The replacement path is whatever the harness maps back from the remote
compaction API output.

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
provider request. `RequestCompaction` actions include the `ModelContext`
snapshot needed to call the remote compaction API. The action handler should
register or spawn long-running model/tool/compaction work and return promptly,
then enqueue completion or failure later through an `AgentInputHandle`.

## Relationship To Other Crates

- **Upstream `agent-core`** — provides the FSM, mailbox input/action vocabulary,
  transcript item types, IDs, and message/tool-call structures. `agent-session`
  re-exports these for a single downstream import path.
- **Downstream `agent-orchestrator`** — owns `SessionRegistry<AgentSession>`,
  routes parent/child messages and reports through `enqueue_input`, invokes
  `compact`, `rewind`, and `fork`, and stays out of `TranscriptStore`
  internals.

For cross-cutting context such as control plane, usage, worklogs, and
multi-agent spawn/report, see `rust/docs/architecture.md`.
