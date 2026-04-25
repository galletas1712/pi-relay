# agent-session

Durable session history and async run-loop wrapper around the `agent-core` FSM kernel.

## Responsibility

`agent-session` is the layer that turns the pure `AgentCoreLoop` FSM into a stateful, editable session. An `AgentSession` owns the core loop (deterministic state machine), a session-local `TranscriptStore` (append-only forest of `TranscriptStorageNode` nodes with one active leaf/path), an `ActionQueue` (FIFO of model/tool requests the session has handed out but hasn't heard back about), queued/session-owned maintenance, and an ephemeral event outbox for live observers. `session.drive()` is the only supported way to advance the FSM; it runs the core to quiescence and absorbs every freshly-produced `TranscriptItem` into the store, which is the sole owner of durable model-visible history.

The crate exposes two primitives for mutating session history. `AgentSession::edit(op)` is the immediate path: `SummarizeSpan`, `Compact`, `Rewind`, and `ReplaceModelContext` implement the `HistoryEdit` trait, and `edit` runs the quiescence gate (`can_edit_history`) before dispatching to the op and rehydrating the core loop. `AgentSession::request_maintenance(maintenance)` is the scheduled path: it queues session-owned maintenance, currently `SessionMaintenance::Compact`, and applies it at the next safe model-context barrier. `AgentSession::fork` is a direct method rather than a `HistoryEdit` impl because it produces a new session instead of mutating the source.

`AgentRunner` is the async I/O shell. Inputs arrive via a cloneable `AgentInputHandle`; the runner calls `drive` in a loop and forwards each drained `SessionAction` to a caller-supplied handler.

What this crate does *not* own: no model calls, no tool execution, no cost tracking, no spawn/report routing, no control-plane scheduling. Those all live in the harness / `agent-orchestrator` and above. The session just drives one agent, stores its history, and emits stateless model requests needed by scheduled maintenance.

## Public interface

All exports are re-exported from `lib.rs`. Downstream callers (primarily `agent-orchestrator`, tests, and future daemon frontends) use only these.

**Composition types**
- `AgentSession` — core loop + transcript store + active path + action queue, the unit of agent state. Constructors and runtime helpers include `new`, `from_transcript_items`, `from_model_context`, `from_transcript_store`, `drive`, `enqueue_input`, `enqueue_session_input`, `request_maintenance`, `drain_actions`, `drain_events`, `drain_pending_inputs`, `model_context`, `transcript_store`, `can_edit_history`, `edit`, and `fork`.
- `AgentRunner` — async wrapper that drives a session off an input channel.
- `AgentInputHandle`, `AgentInputHandleError`, `AgentInputReceiver` — sender/receiver pair for the runner's input channel.
- `SessionAction` — model/tool/cancel actions plus session-owned `RequestModelStateless`. `RequestModel` carries the model-context snapshot visible when the model request was made.
- `SessionInput`, `SessionInputError` — core inputs plus stateless model completions/failures.
- `SessionMaintenance` — scheduled history maintenance to apply at the next safe model-context barrier. Currently `Compact { settings }`.
- `SessionEvent` — ephemeral live activity (`TranscriptItemAppended`, `ActionRequested`, `ActionCompleted`, `ActionFailed`, `HistoryEdited`).
- `SessionActionKind`, `HistoryEditKind` — lightweight event classifiers surfaced by `SessionEvent`.

**Durable state**
- `TranscriptStore` — append-only forest of `TranscriptStorageNode`s plus the session's active leaf.
- `TranscriptStorageNode` — `{ id, parent_id, timestamp_ms, item }`.
- `ModelContext` — read-only materialized view derived from one root-to-leaf path.
- `TranscriptStoreError` — `EntryNotFound`, `InvalidSpan`, `NotTurnBoundary`, `StalePlan`.

**Edit ops and support types**
- `HistoryEdit` — trait every op implements.
- `SummarizeSpan { plan, summary }`, `Compact { plan, summary }`, `Rewind { leaf_id }`, `ReplaceModelContext { replacement }`.
- `HistoryEditError` — `Busy`, `ReplacementNotAtTurnBoundary`, `Store(TranscriptStoreError)`.
- `SummarySpanPlan` — produced by `TranscriptStore::prepare_summary_span`.
- `CompactionPlan`, `CompactionSettings` — prefix-compaction policy produced by `TranscriptStore::prepare_compaction`.
- `AutoCompactionSettings` — optional session policy that queues compaction maintenance when the model context is over budget at a model-context barrier.
- `StatelessModelRequest`, `StatelessModelRequestId`, `StatelessModelOutput`, `StatelessModelOutputSpec`, `ModelContentBlock`, `ImageInput` — stateless side-model request/response vocabulary.

**Well-known injected-message kinds**
- `KIND_COMPACTION_SUMMARY = "compaction_summary"` + `compaction_summary(content, first_kept_entry_id, tokens_before)` builder.

**Re-exports from `agent-core`** (so callers have a single import home)
- `AgentInput`, `AgentInputError`, `AgentAction`, `TranscriptItem`, `TurnId`, `ActionId`, `ToolCallId`, `InjectedMessage`, `TurnOutcome`.
- `AssistantMessage`, `AssistantItem`, `ToolCall`, `ToolResultMessage`, `ToolResultStatus`.

### Drive cycle

```rust
session.enqueue_input(AgentInput::follow_up("hello"))?;
session.drive();
let actions = session.drain_actions();
// caller executes actions out-of-band, feeds results back:
let SessionAction::RequestModel { action_id, turn_id, model_context } = &actions[0] else { unreachable!() };
let provider_request = build_provider_request(model_context);
session.enqueue_input(AgentInput::ModelCompleted { action_id: *action_id, turn_id: *turn_id, assistant })?;
session.drive();
```

`drive` tracks every visible `RequestModel` / `RequestTool` in the internal action queue before callers drain the observable action outbox. `enqueue_input` validates the input and clears the matching key when the corresponding completion or failure arrives. `ModelFailed` closes the turn as `Crashed`; `CancelTurn` clears every entry for that turn id.

With auto-compaction enabled, the threshold policy queues `SessionMaintenance::Compact` when the core reaches a model-context barrier and the context is over budget. The same maintenance queue is available to callers via `request_maintenance`, so user/tool-requested compaction and auto-compaction share the same lifecycle. When compaction maintenance starts, the session emits `SessionAction::RequestModelStateless`; the harness runs that as a stateless side-model call and returns `SessionInput::ModelStatelessCompleted` or `SessionInput::ModelStatelessFailed` through the same input channel. Successful completion applies a compaction summary to the transcript store, then releases any held `RequestModel`. Failure releases the held `RequestModel` unchanged so the agent still makes progress.

`drain_events()` returns live-only `SessionEvent`s that explain what happened without polluting the transcript store. `ModelContext` remains the model-visible view.

### History edits

```rust
// Pure query: no mutation, safe to call any time.
let plan = session.transcript_store().prepare_compaction(settings);
let span = session.transcript_store().prepare_summary_span(first_id, last_id)?;

// Mutating ops flow through AgentSession::edit. The quiescence gate runs once.
session.edit(SummarizeSpan { plan: span, summary })?;       // Output = ()
session.edit(Compact { plan, summary })?;                   // Output = ()
session.edit(Rewind { leaf_id: Some(id) })?;                // Output = ()
let previous = session.edit(ReplaceModelContext { replacement })?;
//                                                             Output = ModelContext

// Scheduled maintenance can be requested while busy. It runs at the next
// safe model-context barrier, possibly by holding a RequestModel until the
// stateless summary returns.
session.request_maintenance(SessionMaintenance::Compact { settings });

// Fork is a direct method because it copies a path into a NEW session.
let forked: AgentSession = session.fork(Some(&leaf_id))?;
```

## Internals

### Module map

| File | Contents |
| --- | --- |
| `src/lib.rs` | Module declarations + public re-exports (including `agent-core` passthroughs). |
| `src/action.rs` | `SessionAction`, `StatelessModelRequestId`. |
| `src/input.rs` | `SessionInput`, `SessionInputError`. |
| `src/event.rs` | Runtime-only `SessionEvent`, `SessionActionKind`. |
| `src/auto_compaction.rs` | `AutoCompactionSettings`, stateless model request/response types, compaction request rendering. |
| `src/maintenance.rs` | `SessionMaintenance` plus private pending-maintenance state. |
| `src/session.rs` | `AgentSession` composition, `drive`, `enqueue_input`, `enqueue_session_input`, `request_maintenance`, `drain_actions`, `drain_events`, `can_edit_history`, `edit`, `fork`, `rehydrate_core_from_transcript_store`. |
| `src/action_queue.rs` | Private `ActionQueue` (FIFO `VecDeque<PendingActionKey>`) + `record_drained` / `record_input`. |
| `src/model_context.rs` | `ModelContext` read-only view: `is_turn_boundary`, `latest_compaction_summary`, crashed-tail patching. |
| `src/runner.rs` | `AgentRunner`, `AgentInputHandle`, `AgentInputHandleError`, `AgentInputReceiver` — async shell over `AgentSession`. |
| `src/transcript_store/mod.rs` | `TranscriptStore` forest, `TranscriptStorageNode`, entry/parent/leaf indexes, materialization, `is_turn_boundary`, `TranscriptStoreError`. No kind-specific knowledge. |
| `src/transcript_store/edit.rs` | `HistoryEdit` trait, `HistoryEditKind`, `HistoryEditError`. |
| `src/transcript_store/span.rs` | Generic span-summary primitive: `SummarizeSpan`, `SummarySpanPlan`, `prepare_summary_span`, span-boundary validation. |
| `src/transcript_store/tokens.rs` | Internal approximate token estimation used by planning and auto-compaction. |
| `src/transcript_store/ops/compaction.rs` | Prefix-compaction policy/op: `Compact`, `CompactionPlan`, `CompactionSettings`, `prepare_compaction`, `validate_plan_matches`, `KIND_COMPACTION_SUMMARY`, `compaction_summary`. |
| `src/transcript_store/ops/rewind.rs` | `Rewind` op. |
| `src/transcript_store/ops/replace_model_context.rs` | `ReplaceModelContext` op (returns previous `ModelContext`). |

### Composition diagram

```
 AgentSession
  ├── core: AgentCoreLoop             (from agent-core — FSM + mailbox)
  ├── transcript_store: TranscriptStore        (append-only forest of TranscriptStorageNode)
  ├── action_queue: ActionQueue       (FIFO: visible in-flight model/tool actions)
  ├── maintenance_queue: VecDeque<SessionMaintenance>
  ├── pending_maintenance: Option<...>
  ├── action_outbox: VecDeque<SessionAction>
  └── event_outbox: VecDeque<SessionEvent>

 drive()       ─► core.drive() → drain transcript items → append to transcript store
              └► drain actions → maybe auto-compact → action outbox
 drain_actions ─► drain observable action outbox
 drain_events  ─► drain live activity event outbox
 enqueue_input ─► validate → action_queue.record_input(..) → core.enqueue_input(..)
 enqueue_session_input ─► core input OR stateless model completion/failure
 edit(op)      ─► can_edit_history? → op.apply(&mut transcript_store) → rehydrate core
 request_maintenance
              ─► queue maintenance → start at next safe model-context barrier
```

The main pieces are load-bearing:
- **core** drives the FSM forward. It buffers transcript items only until the session absorbs them.
- **transcript store** is durable history. It survives compaction because compaction is a branch, not a delete.
- **action_queue** answers "is any visible model/tool work in flight?" — together with queued or pending maintenance, it's the signal that gates immediate history edits.
- **maintenance_queue / pending_maintenance** model deferred session-owned history mutation without making auto-compaction a special edit path.

### TranscriptStore Forest

Each `TranscriptStorageNode` has a `String id` (UUID v4), an `Option<String> parent_id`, a `timestamp_ms`, and one `TranscriptItem` in its `item` field. Entries form a forest: every entry has at most one parent, and any parent may have many children. The store keeps indexes by entry id, parent id, children-by-parent, and current leaves. It also tracks an `Option<String> active_leaf_id` — the current session path head. `append_transcript_items` attaches new children under the active leaf and advances the pointer. `branch(id)` / `branch_at_turn_boundary(id)` move the active leaf onto an existing entry; subsequent appends grow a new path off that node. Nothing is ever deleted from that store.

`model_context()` walks from the current leaf back to the root via `parent_id`, reverses the entries, and materializes that full active path. Summary-span edits rebuild the active branch in model-visible order, so no compaction-specific adapter is needed.

A session currently owns its own `TranscriptStore`; the long-term copy-on-write shape is to hoist the forest into a shared `SessionStore` and let each session point at one leaf. Either way, the mental model stays the same: the store is the forest, and the model context is one materialized root-to-leaf path. `AgentSession::fork` currently creates a new independent session by copying only the ancestor path from root to the requested `leaf_id` into a fresh store; sibling branches, abandoned descendants, queued inputs, in-flight actions, events, and other already-forked sessions are not copied. With a future shared store, the fork can become a cheap second leaf pointer instead of a path copy.

Summary-span replacement in pictures:

```
Initial branch:
  E0 ── E1 ── E2 ── E3 ── E4 ── E5
                                ▲
                                leaf

After SummarizeSpan over E2..E3:
  E0 ── E1 ── E2 ── E3 ── E4 ── E5      (abandoned suffix; still in store)
          \
           Esum ── E4' ── E5'
                         ▲
                         leaf (new branch)
```

`Esum` is a caller-provided injected summary entry; `E4'` / `E5'` are re-appended copies of the suffix transcript items as descendants of `Esum`. The old span and suffix stay in `entries()` as an orphaned branch for audit. `Compact` is a prefix-oriented policy wrapper over this primitive.

### `HistoryEdit` trait

```rust
pub trait HistoryEdit {
    type Output;
    fn apply(self, store: &mut TranscriptStore) -> Result<Self::Output, HistoryEditError>;
}
```

`AgentSession::edit` is the only place ops run. It first consults the quiescence gate:

```
can_edit_history() :=
       core.is_idle()
    && transcript_store.is_turn_boundary()
    && !core.has_pending_work()
    && action_queue.is_empty()
    && action_outbox.is_empty()
    && maintenance_queue.is_empty()
    && pending_maintenance.is_none()
```

The session-owned checks cover state the session can see, including undrained observable actions such as `CancelTurn` that the harness still needs to execute and queued or in-flight maintenance such as compaction. Orchestrator-owned background work is policy above this layer: if the orchestrator wants to block edits while worklog forks or other side tasks are running, it should choose not to call `session.edit(...)`.

Scheduled maintenance uses a looser barrier than immediate edits. A compact request can start while the session is idle at a turn boundary, or after the core emits `RequestModel` but before the session exposes that action to the harness. In the latter case the session holds the model action, emits `RequestModelStateless`, applies the summary when it returns, and only then exposes the original `RequestModel` with an updated `ModelContext`. While maintenance is pending, `drive()` does not start new turns from queued user input.

Op outputs:

| Op | `Output` | Summary |
| --- | --- | --- |
| `SummarizeSpan { plan, summary }` | `()` | Validates a contiguous active-branch span, replaces it with a summary, re-appends the suffix. |
| `Compact { plan, summary }` | `()` | Prefix-compaction wrapper that summarizes old transcript items through `SummarizeSpan`. |
| `Rewind { leaf_id }` | `()` | `Some(id)` → `branch_at_turn_boundary(id)`; `None` → `reset_leaf()`. |
| `ReplaceModelContext { replacement }` | `ModelContext` | Swaps the whole active store for one built from `replacement`; returns the previous model context. |

Core rehydration (`rehydrate_core_from_transcript_store`) runs *after* `apply` succeeds. It lives in `AgentSession::edit`, not the trait, because `apply` sees only `&mut TranscriptStore` and can't touch the core loop. Rehydration rebuilds the core at the current `last_turn_id` while preserving the next `ActionId`, then defensively clears pre-edit action bookkeeping. The quiescence gate requires the observable action outbox to be empty before an edit starts, so callers cannot drop an undrained `CancelTurn`.

### `ActionQueue` semantics

Private to the crate. `PendingActionKey { action_id: ActionId, turn_id: TurnId, kind: Model | Tool }` lives in a `VecDeque` in FIFO insertion order. `record_drained(&[AgentAction])` pushes one entry per visible `RequestModel` / `RequestTool` and clears entries for a given `turn_id` on `CancelTurn`. `record_input(&AgentInput)` removes the matching key on `ModelCompleted` / `ModelFailed` / `ToolCompleted` and returns whether anything was cleared; removal is by key, not necessarily from the head, and preserves order among the survivors. Duplicates are kept — FIFO with no dedup. Stale completions (no matching key) are silent no-ops.

`is_empty() == true` ⇒ no in-flight actions ⇒ `can_edit_history` clears that check.

### Kind-free `transcript_store/mod.rs` + operation-local `KIND_*` constants

`transcript_store/mod.rs` owns the forest primitives — `TranscriptStore`, `TranscriptStorageNode`, `append_transcript_items`, `branch`, `is_turn_boundary_leaf`, `leaf_ids`, `child_ids` — and knows about exactly one item variant semantically: `TranscriptItem::Injected` is treated as transparent for turn-boundary walks. It knows nothing about `"compaction_summary"` specifically.

The `KIND_COMPACTION_SUMMARY` constant and its builder live in `transcript_store/ops/compaction.rs`, the file for the policy that produces it. This keeps the forest store decoupled from higher-level semantics: new edit ops with new injected-message kinds can land without touching `mod.rs`.

### `AgentRunner` (async wrapper)

`AgentRunner` is the only async surface in the crate — the core loop itself is fully sync. It owns an `AgentSession` plus an `AgentInputReceiver` (the receive side of a `futures_channel::mpsc::unbounded` channel) plus a `FnMut(SessionAction) -> impl Future<Output = ()>` action handler. `run()` drives the session to quiescence, flushes drained actions through the handler, then loops awaiting the next `SessionInput`, enqueuing it, and repeating. `RequestModel` actions include the `ModelContext` snapshot from the moment the model request was made, so a model handler can build the provider request without maintaining a parallel event mirror. The action handler is a dispatch hook: it should register or spawn long-running model/tool work and return promptly, then enqueue completion/failure later through an `AgentInputHandle`. Awaiting the provider/tool call inline will intentionally block this simple runner from processing further inputs. `AgentInputHandle::channel()` hands back the matching sender; the handle is `Clone`, so orchestrator, model, tool, and stateless model tasks can all enqueue inputs back into the same session. The handle validates `SessionInput` before sending; invalid inputs return `AgentInputHandleError::Invalid` immediately.

Transcript items are observed through the session's `model_context()` view; there is no transcript-item callback.

## Relationship to other crates

- **Upstream** `agent-core` — provides `AgentCoreLoop`, `AgentInput`, `AgentInputError`, `AgentAction`, `TranscriptItem`, `TurnId`, `ActionId`, `TurnOutcome`, `InjectedMessage`, and the message/tool-call vocabulary. `agent-session` re-exports these so downstream has a single import path.
- **Downstream** `agent-orchestrator` — owns a `SessionRegistry<AgentSession>` keyed by `SessionId`, routes parent/child messages and reports, delegates immediate history edits to `session.edit(op)`, schedules compaction through `session.request_maintenance(...)`, and creates copies with `session.fork(leaf)`. It never reaches into `TranscriptStore` internals directly; it calls transcript-store planning methods as pure queries and lets the session dispatch mutation.

For cross-cutting context (control plane, cost aggregation, worklog forks, multi-agent spawn/report), see `rust/docs/architecture.md`.
