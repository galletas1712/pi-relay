# agent-session

Durable session context and async run-loop wrapper around the `agent-core` FSM kernel.

## Responsibility

`agent-session` is the layer that turns the pure `AgentCoreLoop` FSM into a stateful, editable session. An `AgentSession` owns the core loop (deterministic state machine), a session-local `TranscriptStore` (append-only forest of `TranscriptEntry` nodes with one active leaf/path), an `ActionQueue` (FIFO of model/tool requests the session has handed out but hasn't heard back about), session-owned stateless model work, and an ephemeral event outbox for live observers. `session.drive()` is the only supported way to advance the FSM; it runs the core to quiescence and absorbs every freshly-produced `ContextItem` into the store, which is the sole owner of durable model-visible history.

The crate also owns the *edit* surface: `SummarizeSpan`, `Compact`, `Rewind`, and `ReplaceTranscript` are individual op structs that implement the `ContextEdit` trait. `AgentSession::edit` runs the quiescence gate (`can_edit_history`) once, dispatches to the op, then rehydrates the core loop from the new active path. `AgentSession::fork` is a direct method rather than a `ContextEdit` impl because it produces a new session instead of mutating the source.

`AgentRunner` is the async I/O shell. Inputs arrive via a cloneable `AgentInputHandle`; the runner calls `drive` in a loop and forwards each drained `SessionAction` to a caller-supplied handler.

What this crate does *not* own: no model calls, no tool execution, no cost tracking, no spawn/report routing, no control-plane scheduling. Those all live in the harness / `agent-orchestrator` and above. The session just drives one agent, stores its history, and emits side work such as stateless model compaction requests.

## Public interface

All exports are re-exported from `lib.rs`. Downstream callers (primarily `agent-orchestrator`, tests, and future daemon frontends) use only these.

**Composition types**
- `AgentSession` — core loop + transcript store + active path + action queue, the unit of agent state.
- `AgentRunner` — async wrapper that drives a session off an input channel.
- `AgentInputHandle`, `AgentInputHandleError`, `AgentInputReceiver` — sender/receiver pair for the runner's input channel.
- `SessionAction` — model/tool/cancel actions plus session-owned `RequestModelStateless`. `RequestModel` carries the model-context snapshot visible when the model request was made.
- `SessionInput`, `SessionInputError` — core inputs plus stateless model completions/failures.
- `SessionEvent` — ephemeral live activity (`RecordAppended`, `ActionRequested`, `ActionCompleted`, `ActionFailed`, `ContextEdited`).

**Durable state**
- `TranscriptStore` — append-only forest of `TranscriptEntry`s plus the session's active leaf. `Context` is a compatibility alias.
- `TranscriptEntry` — `{ id, parent_id, timestamp_ms, record }`. `SessionEntry` is a compatibility alias.
- `ModelContext` — read-only materialized view derived from one root-to-leaf path. `Transcript` is a compatibility alias.
- `ContextError` — `EntryNotFound`, `InvalidSpan`, `NotTurnBoundary`, `StalePlan`.

**Edit ops and support types**
- `ContextEdit` — trait every op implements.
- `SummarizeSpan { plan, summary }`, `Compact { plan, summary }`, `Rewind { leaf_id }`, `ReplaceTranscript { replacement }`.
- `PendingWork { background_tasks: usize }` — caller-declared invisible work.
- `HistoryEditError` — `Busy`, `ReplacementNotAtTurnBoundary`, `Context(ContextError)`.
- `SummarySpanPlan` — produced by `TranscriptStore::prepare_summary_span` / `Context::prepare_summary_span`.
- `CompactionPlan`, `CompactionSettings` — prefix-compaction policy produced by `TranscriptStore::prepare_compaction` / `Context::prepare_compaction`.
- `AutoCompactionSettings` — optional session policy that pauses a model request and emits stateless model compaction work when context is over budget.
- `StatelessModelRequest`, `StatelessModelRequestId`, `StatelessModelOutput`, `ModelContentBlock`, `ImageInput` — stateless side-model request/response vocabulary.

**Well-known injected-message kinds**
- `KIND_COMPACTION_SUMMARY = "compaction_summary"` + `compaction_summary(content, first_kept_entry_id, tokens_before)` builder.

**Re-exports from `agent-core`** (so callers have a single import home)
- `AgentInput`, `AgentInputError`, `AgentAction`, `ContextItem`, `TranscriptRecord`, `TurnId`, `ActionId`, `ToolCallId`, `InjectedMessage`, `TurnOutcome`.
- `AssistantMessage`, `AssistantItem`, `ToolCall`, `ToolResultMessage`, `ToolResultStatus`.

### Drive cycle

```rust
session.enqueue_input(AgentInput::follow_up("hello"))?;
session.drive();
let actions = session.drain_actions();
// caller executes actions out-of-band, feeds results back:
let SessionAction::RequestModel { action_id, turn_id, transcript } = &actions[0] else { unreachable!() };
let provider_request = build_provider_request(transcript);
session.enqueue_input(AgentInput::ModelCompleted { action_id: *action_id, turn_id: *turn_id, assistant })?;
session.drive();
```

`drive` records every visible `RequestModel` / `RequestTool` into the internal action queue before callers drain the observable action outbox. `enqueue_input` validates the input and clears the matching key when the corresponding completion or failure arrives. `ModelFailed` closes the turn as `Crashed`; `CancelTurn` clears every entry for that turn id.

With auto-compaction enabled, a core `RequestModel` may be held by the session while it emits `SessionAction::RequestModelStateless`. The harness runs that as a stateless side-model call and returns `SessionInput::ModelStatelessCompleted` or `SessionInput::ModelStatelessFailed` through the same input channel. Successful completion applies a compaction summary to the context, then releases the held `RequestModel`. Failure releases the held `RequestModel` unchanged so the agent still makes progress.

`drain_events()` returns live-only `SessionEvent`s that explain what happened without polluting the transcript. The transcript remains the model-visible durable context.

### History edits

```rust
// Pure query: no mutation, safe to call any time.
let plan = session.context().prepare_compaction(settings);
let span = session.context().prepare_summary_span(first_id, last_id)?;

// Mutating ops flow through AgentSession::edit. The quiescence gate runs once.
session.edit(pending, SummarizeSpan { plan: span, summary })?;       // Output = ()
session.edit(pending, Compact { plan, summary })?;                   // Output = ()
session.edit(pending, Rewind { leaf_id: Some(id) })?;                // Output = ()
let previous = session.edit(pending, ReplaceTranscript { replacement })?;
//                                                             Output = ModelContext

// Fork is a direct method because it produces a NEW session.
let forked: AgentSession = session.fork(pending, Some(&leaf_id))?;
```

## Internals

### Module map

| File | Contents |
| --- | --- |
| `src/lib.rs` | Module declarations + public re-exports (including `agent-core` passthroughs). |
| `src/action.rs` | `SessionAction`, `StatelessModelRequestId`. |
| `src/input.rs` | `SessionInput`, `SessionInputError`. |
| `src/event.rs` | Runtime-only `SessionEvent`, `SessionActionKind`, `ContextEditKind`. |
| `src/auto_compaction.rs` | `AutoCompactionSettings`, stateless model request/response types, compaction request rendering. |
| `src/session.rs` | `AgentSession` composition, `drive`, `enqueue_input`, `enqueue_session_input`, `drain_actions`, `drain_events`, `can_edit_history`, `edit`, `fork`, `rehydrate_core_from_context`. |
| `src/action_queue.rs` | Private `ActionQueue` (FIFO `VecDeque<PendingActionKey>`) + `record_drained` / `record_input`. |
| `src/transcript.rs` | `ModelContext` / `Transcript` read-only view: `is_turn_boundary`, `latest_compaction_summary`, crashed-tail patching. |
| `src/runner.rs` | `AgentRunner`, `AgentInputHandle`, `AgentInputHandleError`, `AgentInputReceiver` — async shell over `AgentSession`. |
| `src/context/mod.rs` | `TranscriptStore` / `Context` forest, `TranscriptEntry` / `SessionEntry`, entry/parent/leaf indexes, materialization, `is_turn_boundary`, `ContextError`. No kind-specific knowledge. |
| `src/context/edit.rs` | `ContextEdit` trait, `PendingWork`, `HistoryEditError`. |
| `src/context/span.rs` | Generic span-summary primitive: `SummarizeSpan`, `SummarySpanPlan`, `prepare_summary_span`, span-boundary validation. |
| `src/context/tokens.rs` | Internal approximate token estimation used by context planning and auto-compaction. |
| `src/context/ops/compaction.rs` | Prefix-compaction policy/op: `Compact`, `CompactionPlan`, `CompactionSettings`, `prepare_compaction`, `validate_plan_matches`, `KIND_COMPACTION_SUMMARY`, `compaction_summary`. |
| `src/context/ops/rewind.rs` | `Rewind` op. |
| `src/context/ops/replace.rs` | `ReplaceTranscript` op (returns previous `ModelContext` / `Transcript`). |

### Composition diagram

```
 AgentSession
  ├── core: AgentCoreLoop             (from agent-core — FSM + mailbox)
  ├── context: TranscriptStore        (append-only forest of TranscriptEntry)
  ├── action_queue: ActionQueue       (FIFO: visible in-flight model/tool actions)
  ├── pending_stateless_model: Option<...>   (session-owned side work)
  ├── action_outbox: VecDeque<SessionAction>
  └── event_outbox: VecDeque<SessionEvent>

 drive()       ─► core.drive() → drain records → append to context
              └► drain actions → maybe auto-compact → action outbox
 drain_actions ─► drain observable action outbox
 drain_events  ─► drain live activity event outbox
 enqueue_input ─► validate → action_queue.record_input(..) → core.enqueue_input(..)
 enqueue_session_input ─► core input OR stateless model completion/failure
 edit(op)      ─► can_edit_history? → op.apply(&mut context) → rehydrate core
```

All three components are load-bearing:
- **core** drives the FSM forward. It buffers records only until the session absorbs them.
- **transcript store** is durable history. It survives compaction because compaction is a branch, not a delete.
- **action_queue** answers "is any visible model/tool work in flight?" — together with pending stateless model work, it's the signal that gates history edits.

### Transcript Store Forest

Each `TranscriptEntry` has a `String id` (UUID v4), an `Option<String> parent_id`, a `timestamp_ms`, and one `ContextItem` (`TranscriptRecord` compatibility name). Entries form a forest: every entry has at most one parent, and any parent may have many children. The store keeps indexes by entry id, parent id, children-by-parent, and current leaves. It also tracks an `Option<String> active_leaf_id` — the current session path head. `append_record` attaches a new child under the active leaf and advances the pointer. `branch(id)` / `branch_at_turn_boundary(id)` move the active leaf onto an existing entry; subsequent appends grow a new path off that node. Nothing is ever deleted from that store.

`transcript()` / the future `model_context()` walk from the current leaf back to the root via `parent_id`, reverse, and materialize that full active path. Summary-span edits rebuild the active branch in model-visible order, so no compaction-specific adapter is needed.

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

`Esum` is a caller-provided injected summary entry; `E4'` / `E5'` are re-appended copies of the suffix records as descendants of `Esum`. The old span and suffix stay in `entries()` as an orphaned branch for audit. `Compact` is a prefix-oriented policy wrapper over this primitive.

### `ContextEdit` trait + `PendingWork`

```rust
pub trait ContextEdit {
    type Output;
    fn apply(self, ctx: &mut Context) -> Result<Self::Output, HistoryEditError>;
}
```

`AgentSession::edit` is the only place ops run. It first consults the quiescence gate:

```
can_edit_history(pending) :=
       core.is_idle()
    && context.is_turn_boundary()
    && !core.has_pending_work()
    && action_queue.is_empty()
    && action_outbox.is_empty()
    && pending_stateless_model.is_none()
    && pending.is_empty()
```

The session-owned checks cover state the session can see, including undrained observable actions such as `CancelTurn` that the harness still needs to execute. `PendingWork { background_tasks: usize }` is the counter for *invisible* work the caller is tracking on its own — worklog forks, background summarization calls — that must also finish before history is safe to touch. `PendingWork::NONE` is the zero value.

Op outputs:

| Op | `Output` | Summary |
| --- | --- | --- |
| `SummarizeSpan { plan, summary }` | `()` | Validates a contiguous active-branch span, replaces it with a summary, re-appends the suffix. |
| `Compact { plan, summary }` | `()` | Prefix-compaction wrapper that summarizes old context through `SummarizeSpan`. |
| `Rewind { leaf_id }` | `()` | `Some(id)` → `branch_at_turn_boundary(id)`; `None` → `reset_leaf()`. |
| `ReplaceTranscript { replacement }` | `ModelContext` / `Transcript` | Swaps the whole active store for one built from `replacement`; returns the previous model context. |

Core rehydration (`rehydrate_core_from_context`) runs *after* `apply` succeeds. It lives in `AgentSession::edit`, not the trait, because `apply` sees only `&mut Context` and can't touch the core loop. Rehydration rebuilds the core at the current `last_turn_id` while preserving the next `ActionId`, then defensively clears pre-edit action bookkeeping. The quiescence gate requires the observable action outbox to be empty before an edit starts, so callers cannot drop an undrained `CancelTurn`.

### `ActionQueue` semantics

Private to the crate. `PendingActionKey { action_id: ActionId, turn_id: TurnId, kind: Model | Tool }` lives in a `VecDeque` in FIFO insertion order. `record_drained(&[AgentAction])` pushes one entry per visible `RequestModel` / `RequestTool` and clears entries for a given `turn_id` on `CancelTurn`. `record_input(&AgentInput)` removes the matching key on `ModelCompleted` / `ModelFailed` / `ToolCompleted` and returns whether anything was cleared; removal is by key, not necessarily from the head, and preserves order among the survivors. Duplicates are kept — FIFO with no dedup. Stale completions (no matching key) are silent no-ops.

`is_empty() == true` ⇒ no in-flight actions ⇒ `can_edit_history` clears that check.

### Kind-free `context/mod.rs` + operation-local `KIND_*` constants

`context/mod.rs` owns the forest primitives — `TranscriptStore`, `TranscriptEntry`, `append_record`, `branch`, `is_turn_boundary_leaf`, `leaf_ids`, `child_ids` — and knows about exactly one record variant semantically: `ContextItem::Injected` / `TranscriptRecord::Injected` is treated as transparent for turn-boundary walks. It knows nothing about `"compaction_summary"` specifically.

The `KIND_COMPACTION_SUMMARY` constant and its builder live in `context/ops/compaction.rs`, the file for the policy that produces it. This keeps the forest store decoupled from higher-level semantics: new edit ops with new injected-message kinds can land without touching `mod.rs`.

### `AgentRunner` (async wrapper)

`AgentRunner` is the only async surface in the crate — the core loop itself is fully sync. It owns an `AgentSession` plus an `AgentInputReceiver` (the receive side of a `futures_channel::mpsc::unbounded` channel) plus a `FnMut(SessionAction) -> impl Future<Output = ()>` action handler. `run()` drives the session to quiescence, flushes drained actions through the handler, then loops awaiting the next `SessionInput`, enqueuing it, and repeating. `RequestModel` actions include the transcript snapshot from the moment the model request was made, so a model handler can build the provider request without maintaining a parallel event mirror. The action handler is a dispatch hook: it should register or spawn long-running model/tool work and return promptly, then enqueue completion/failure later through an `AgentInputHandle`. Awaiting the provider/tool call inline will intentionally block this simple runner from processing further inputs. `AgentInputHandle::channel()` hands back the matching sender; the handle is `Clone`, so orchestrator, model, tool, and stateless model tasks can all enqueue inputs back into the same session. The handle validates `SessionInput` before sending; invalid inputs return `AgentInputHandleError::Invalid` immediately.

Records are observed off the session's transcript (`runner.session().transcript()`); there is no record callback.

## Relationship to other crates

- **Upstream** `agent-core` — provides `AgentCoreLoop`, `AgentInput`, `AgentInputError`, `AgentAction`, `ContextItem` (`TranscriptRecord` compatibility alias), `TurnId`, `ActionId`, `TurnOutcome`, `InjectedMessage`, and the message/tool-call vocabulary. `agent-session` re-exports these so downstream has a single import path.
- **Downstream** `agent-orchestrator` — owns a `SessionRegistry<AgentSession>` keyed by `SessionId`, routes parent/child messages and reports, and delegates every history edit to `session.edit(pending, op)` / `session.fork(pending, leaf)`. It never reaches into `Context` internals directly; it calls `session.context().prepare_compaction(..)` as a pure query and lets the session dispatch the resulting op.

For cross-cutting context (control plane, cost aggregation, worklog forks, multi-agent spawn/report), see `rust/docs/architecture.md`.
