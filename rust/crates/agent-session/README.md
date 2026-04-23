# agent-session

Durable session context and async run-loop wrapper around the `agent-core` FSM kernel.

## Responsibility

`agent-session` is the layer that turns the pure `AgentCoreLoop` FSM into a stateful, editable session. An `AgentSession` owns three things: the core loop (deterministic state machine), a `Context` (append-only DAG of `SessionEntry` nodes — the durable transcript), and an `ActionQueue` (FIFO of model/tool requests the session has handed out but hasn't heard back about). `session.drive()` is the only supported way to advance the FSM; it runs the core to quiescence and absorbs every freshly-produced `TranscriptRecord` into the context, which is the sole owner of durable history.

The crate also owns the *edit* surface: `Compact`, `Rewind`, and `ReplaceTranscript` are individual op structs that implement the `ContextEdit` trait. `AgentSession::edit` runs the quiescence gate (`can_edit_history`) once, dispatches to the op, then rehydrates the core loop from the new context. `AgentSession::fork` is a direct method rather than a `ContextEdit` impl because it produces a new session instead of mutating the source.

`AgentRunner` is the async I/O shell. Inputs arrive via a cloneable `AgentInputHandle`; the runner calls `drive` in a loop and forwards each drained `AgentAction` to a caller-supplied handler.

What this crate does *not* own: no model calls, no tool execution, no cost tracking, no spawn/report routing, no control-plane scheduling. Those all live in `agent-orchestrator` and above. The session just drives one agent and stores its history.

## Public interface

All exports are re-exported from `lib.rs`. Downstream callers (primarily `agent-orchestrator`, tests, and future daemon frontends) use only these.

**Composition types**
- `AgentSession` — core loop + context + action queue, the unit of agent state.
- `AgentRunner` — async wrapper that drives a session off an input channel.
- `AgentInputHandle`, `AgentInputReceiver` — sender/receiver pair for the runner's input channel.

**Durable state**
- `Context` — append-only DAG of `SessionEntry`.
- `SessionEntry` — `{ id, parent_id, timestamp_ms, record }`.
- `Transcript` — read-only materialized view derived from a record slice.
- `ContextError` — `EntryNotFound`, `NotTurnBoundary`, `StalePlan`.

**Edit ops and support types**
- `ContextEdit` — trait every op implements.
- `Compact { plan, summary }`, `Rewind { leaf_id }`, `ReplaceTranscript { replacement }`.
- `PendingWork { background_tasks: usize }` — caller-declared invisible work.
- `HistoryEditError` — `Busy`, `ReplacementNotAtTurnBoundary`, `Context(ContextError)`.
- `CompactionPlan`, `CompactionSettings` — produced by `Context::prepare_compaction`.

**Well-known Custom kinds**
- `KIND_COMPACTION_SUMMARY = "compaction_summary"` + `compaction_summary(content, first_kept_entry_id, tokens_before)` builder.
- `KIND_BRANCH_SUMMARY = "branch_summary"` + `branch_summary(content, from_id)` builder.

**Re-exports from `agent-core`** (so callers have a single import home)
- `AgentInput`, `AgentAction`, `TranscriptRecord`, `TurnId`, `CustomMessage`, `TurnOutcome`.

### Drive cycle

```rust
session.enqueue_input(AgentInput::follow_up("hello"));
session.drive();
let actions = session.drain_actions();
// caller executes actions out-of-band, feeds results back:
session.enqueue_input(AgentInput::ModelCompleted { turn_id, assistant });
session.drive();
```

`drain_actions` records every `RequestModel` / `RequestTool` into the internal action queue; `enqueue_input` clears the matching key when the corresponding completion arrives. `CancelTurn` clears every entry for that turn id.

### History edits

```rust
// Pure query: no mutation, safe to call any time.
let plan = session.context().prepare_compaction(settings);

// Mutating ops flow through AgentSession::edit. The quiescence gate runs once.
session.edit(pending, Compact { plan, summary })?;                   // Output = ()
session.edit(pending, Rewind { leaf_id: Some(id) })?;                // Output = ()
let previous = session.edit(pending, ReplaceTranscript { replacement })?;
//                                                             Output = Transcript

// Fork is a direct method because it produces a NEW session.
let forked: AgentSession = session.fork(pending, Some(&leaf_id))?;
```

## Internals

### Module map

| File | Contents |
| --- | --- |
| `src/lib.rs` | Module declarations + public re-exports (including `agent-core` passthroughs). |
| `src/session.rs` | `AgentSession` composition, `drive`, `enqueue_input`, `drain_actions`, `can_edit_history`, `edit`, `fork`, `rehydrate_core_from_context`. |
| `src/action_queue.rs` | Private `ActionQueue` (FIFO `VecDeque<PendingActionKey>`) + `record_drained` / `record_input`. |
| `src/transcript.rs` | `Transcript` read-only view: `is_turn_boundary`, `latest_compaction_summary`, `branch_summaries`, crashed-tail patching. |
| `src/runner.rs` | `AgentRunner`, `AgentInputHandle`, `AgentInputReceiver` — async shell over `AgentSession`. |
| `src/context/mod.rs` | `Context` DAG, `SessionEntry`, leaf navigation, `is_turn_boundary`, `ContextError`. No kind-specific knowledge. |
| `src/context/edit.rs` | `ContextEdit` trait, `PendingWork`, `HistoryEditError`. |
| `src/context/compaction.rs` | `Compact` op, `CompactionPlan`, `CompactionSettings`, `prepare_compaction`, `validate_plan_matches`, `materialize_context`, `KIND_COMPACTION_SUMMARY`, `compaction_summary`. |
| `src/context/rewind.rs` | `Rewind` op, `KIND_BRANCH_SUMMARY`, `branch_summary`. |
| `src/context/replace.rs` | `ReplaceTranscript` op (returns previous `Transcript`). |

### Composition diagram

```
 AgentSession
  ├── core: AgentCoreLoop       (from agent-core — FSM + mailbox)
  ├── context: Context          (append-only DAG of SessionEntry)
  └── action_queue: ActionQueue (FIFO: drained-but-not-yet-reported actions)

 drive()       ─► core.drive()           → drain records → append to context
 drain_actions ─► core.drain_actions()   → action_queue.record_drained(..)
 enqueue_input ─► action_queue.record_input(..) → core.enqueue_input(..)
 edit(op)      ─► can_edit_history? → op.apply(&mut context) → rehydrate core
```

All three components are load-bearing:
- **core** drives the FSM forward. It buffers records only until the session absorbs them.
- **context** is durable history. It survives compaction because compaction is a fork, not a delete.
- **action_queue** answers "is any work in flight?" — it's the signal that gates history edits.

### The Context DAG

Each `SessionEntry` has a `String id` (UUID v4), an `Option<String> parent_id`, a `timestamp_ms`, and one `TranscriptRecord`. Entries sit in a `Vec<SessionEntry>` with a `HashMap<String, usize>` side-index for O(1) lookup by id. The context tracks an `Option<String> leaf_id` — the active branch head. `append_record` attaches a new child under `leaf_id` and advances the pointer. `branch(id)` / `branch_at_turn_boundary(id)` reparent the leaf onto an existing entry; subsequent appends then grow a new branch off that node. Nothing is ever deleted.

`transcript()` walks from the current leaf back to the root via `parent_id`, reverses, and hands the record sequence to `compaction::materialize_context`, which starts the slice at the most recent `KIND_COMPACTION_SUMMARY` on the path (if any).

Fork-based compaction in pictures:

```
Initial branch:
  E0 ── E1 ── E2 ── E3 ── E4
                          ▲
                          leaf

After Compact with plan.first_kept_entry_id = E3:
  E0 ── E1 ── E2 ── E3 ── E4            (abandoned; still in DAG)
              \
               Esum ── E3' ── E4'
                              ▲
                              leaf (new branch)
```

`Esum` is a `Custom` entry with `kind = KIND_COMPACTION_SUMMARY`; `E3'` / `E4'` are re-appended copies of the kept records as descendants of `Esum`. The old E3/E4 stay in `entries()` as an orphaned branch for audit. The materialized transcript starts at `Esum`.

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
    && pending.is_empty()
```

The first four checks cover state the session can see. `PendingWork { background_tasks: usize }` is the counter for *invisible* work the caller is tracking on its own — worklog forks, background summarization calls — that must also finish before history is safe to touch. `PendingWork::NONE` is the zero value.

Op outputs:

| Op | `Output` | Summary |
| --- | --- | --- |
| `Compact { plan, summary }` | `()` | Validates plan freshness, forks at the pre-cut boundary, appends `Esum`, re-appends the kept records. |
| `Rewind { leaf_id }` | `()` | `Some(id)` → `branch_at_turn_boundary(id)`; `None` → `reset_leaf()`. |
| `ReplaceTranscript { replacement }` | `Transcript` | Swaps the whole context for one built from `replacement`; returns the previous transcript. |

Core rehydration (`rehydrate_core_from_context`) runs *after* `apply` succeeds. It lives in `AgentSession::edit`, not the trait, because `apply` sees only `&mut Context` and can't touch the core loop. Rehydration rebuilds `AgentCoreLoop::resume_at_boundary(last_turn_id)` and clears the action queue (any keys left belong to the pre-edit run).

### `ActionQueue` semantics

Private to the crate. `PendingActionKey { turn_id: TurnId, kind: Model | Tool { tool_call_id } }` lives in a `VecDeque` in FIFO insertion order. `record_drained(&[AgentAction])` pushes one entry per `RequestModel` / `RequestTool` and clears entries for a given `turn_id` on `CancelTurn`. `record_input(&AgentInput)` removes the matching key on `ModelCompleted` / `ToolCompleted`; removal is by key, not necessarily from the head, and preserves order among the survivors. Duplicates are kept — FIFO with no dedup. Stale completions (no matching key) are silent no-ops.

`is_empty() == true` ⇒ no in-flight actions ⇒ `can_edit_history` clears that check.

### Kind-free `context/mod.rs` + operation-local `KIND_*` constants

`context/mod.rs` owns the DAG primitives — `Context`, `SessionEntry`, `append_record`, `branch`, `is_turn_boundary_leaf` — and knows about exactly one record variant semantically: `TranscriptRecord::Custom` is treated as transparent for turn-boundary walks. It knows nothing about `"compaction_summary"` or `"branch_summary"` specifically.

Each `KIND_*` constant and its builder (`compaction_summary`, `branch_summary`) lives in the file for the op that produces it (`context/compaction.rs`, `context/rewind.rs`). This keeps the DAG code decoupled from any specific higher-level semantics: new edit ops with new `Custom` kinds can land without touching `mod.rs`.

### `AgentRunner` (async wrapper)

`AgentRunner` is the only async surface in the crate — the core loop itself is fully sync. It owns an `AgentSession` plus an `AgentInputReceiver` (the receive side of a `futures_channel::mpsc::unbounded` channel) plus a `FnMut(AgentAction) -> impl Future<Output = ()>` action handler. `run()` drives the session to quiescence, flushes drained actions through the handler, then loops awaiting the next input, enqueuing it, and repeating. `AgentInputHandle::channel()` hands back the matching sender; the handle is `Clone`, so orchestrator, model, and tool tasks can all enqueue inputs back into the same session.

Records are observed off the session's transcript (`runner.session().transcript()`); there is no record callback.

## Relationship to other crates

- **Upstream** `agent-core` — provides `AgentCoreLoop`, `AgentInput`, `AgentAction`, `TranscriptRecord`, `TurnId`, `TurnOutcome`, `CustomMessage`, and the message/tool-call vocabulary. `agent-session` re-exports these so downstream has a single import path.
- **Downstream** `agent-orchestrator` — owns a `SessionRegistry<AgentSession>` keyed by `SessionId`, routes parent/child messages and reports, and delegates every history edit to `session.edit(pending, op)` / `session.fork(pending, leaf)`. It never reaches into `Context` internals directly; it calls `session.context().prepare_compaction(..)` as a pure query and lets the session dispatch the resulting op.

For cross-cutting context (control plane, cost aggregation, worklog forks, multi-agent spawn/report), see `rust/docs/architecture.md`.
