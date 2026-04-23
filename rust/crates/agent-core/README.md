# agent-core

A deterministic, pure-Rust FSM kernel for a single agent's turn-by-turn loop — no I/O, no async, no persistence.

## Responsibility

This crate owns one thing: given the current FSM state plus a queued input, decide what should happen next. "What happens next" means two drained outputs — a list of `TranscriptRecord`s that capture durable facts about the turn (turn started, assistant said this, tool call started, tool result, turn finished), and a list of `AgentAction`s that are *requests* for the outside world to run a model call, run a tool, or cancel an in-flight turn.

The FSM is pure: it has no `tokio`, no filesystem access, no network calls, no time source, and no persistent state beyond a small mailbox plus the live control state (`AgentState`). The crate's `Cargo.toml` has zero runtime dependencies. Callers drive the loop synchronously; anything asynchronous (model HTTP calls, tool subprocesses, on-disk logs) lives above this layer in `agent-session` and its runner.

The transcript the loop produces is handed to the caller. `agent-core` does not persist it, does not re-read it, and does not index into past turns. Once records are drained, the core forgets them. The only "memory" the core carries across turns is `last_turn_id: TurnId` and whatever is queued in the mailbox.

See `rust/docs/architecture.md` for how this layer fits underneath `agent-session` and the control plane.

## Public interface

The exported surface is intentionally small. From `lib.rs`:

- `AgentCoreLoop` — the FSM driver. Owns a `Mailbox`, an `AgentState`, `last_turn_id`, and two outboxes (records and actions).
- `AgentInput` — everything the outside world can push into the loop: `Interrupt`, `Steer`, `FollowUp`, `ModelCompleted`, `ToolCompleted`. `Steer` and `FollowUp` carry optional `from` / `kind` tags (both present or both absent) identifying the source of the input.
- `AgentAction` — side-effect requests the caller must execute: `RequestModel`, `RequestTool`, `CancelTurn`.
- `TranscriptRecord` — durable append-only record variants. Enumerated in the Internals section below.
- `CustomMessage` — payload for `TranscriptRecord::Custom`, carrying a `kind` tag, a `content` string, and a `BTreeMap<String, String>` metadata map.
- `TurnOutcome` — `Graceful`, `Interrupted`, or `Crashed`; attached to `TurnFinished`.
- `AssistantMessage`, `AssistantItem`, `ToolCall`, `ToolResultMessage`, `ToolResultStatus` — message shapes shared with the caller. `ToolResultMessage::interrupted` / `crashed` are helpers for synthesizing terminal results.
- `TurnId`, `ToolCallId` — newtype `u64` ids.

`AgentState`, `AgentEvent`, `Mailbox`, and `TurnOrigin` are deliberately **not** re-exported; they are implementation details. Callers observe liveness through `AgentCoreLoop::is_idle()` and `AgentCoreLoop::has_pending_work()`.

### Caller cycle

Downstream wrappers (canonically `AgentSession` in `agent-session`) follow this cycle:

```
caller                       AgentCoreLoop
  |                               |
  |-- enqueue_input(input) ------>|  (push into mailbox)
  |                               |
  |-- drive() ------------------->|  (advance FSM to quiescence)
  |                               |     state transitions run;
  |                               |     records + actions accumulate
  |                               |     in outboxes
  |                               |
  |<-- drain_records() -----------|  (TurnStarted, UserMessage,
  |                               |   AssistantMessage, ToolResult,
  |                               |   TurnFinished, ...)
  |                               |
  |<-- drain_actions() -----------|  (RequestModel / RequestTool /
  |                               |   CancelTurn)
  |                               |
  |  (caller executes actions,    |
  |   feeds results back via      |
  |   enqueue_input(ModelCompleted|
  |   / ToolCompleted))           |
  |                               |
  +------------- loop ------------+
```

A typical session wrapper (see `agent-session/src/session.rs`) forwards `enqueue_input` straight to the core, calls `drive()`, then absorbs the drained records into its durable `Context`. The session never exposes the core directly: it funnels every input through itself so it can track pending model/tool work for edit-quiescence checks and so records always flow into durable storage.

`drain_pending_inputs()` is also exposed as an introspection hook: it pulls every queued user input (steer before follow-up) back out of the mailbox without advancing the FSM, preserving each entry's `from` / `kind` tags. Notifications and the interrupt flag are left untouched. Intended for tests and orchestrator-level routing diagnostics.

### Key invariant

`agent-core` owns no persistent state outside its mailbox plus the live `AgentState`. Every record it produces is handed off via `drain_records()` and then forgotten. If the caller wants to resume a session after a restart, it uses `AgentCoreLoop::resume_at_boundary(last_turn_id)` to build a fresh, idle core seeded only with the next turn id — the transcript is rebuilt by the session from its own durable log.

## Internals

Module layout under `src/`:

- `lib.rs` — crate root; re-exports the public surface; `#![forbid(unsafe_code)]`.
- `loop.rs` (mounted as `mod core_loop`) — `AgentCoreLoop`, the driver that pulls events out of the mailbox and feeds them to the state machine until it quiesces.
- `state.rs` — `AgentState` enum and its `step` function; the heart of the FSM.
- `event.rs` — `AgentInput` (public) and `AgentEvent` + `TurnOrigin` (private); the public/internal event split.
- `mailbox.rs` — `Mailbox` and `UserInputEntry`; priority queue feeding the FSM.
- `action.rs` — `AgentAction` outbox variants.
- `record.rs` — `TranscriptRecord`, `TurnOutcome`, `CustomMessage`.
- `message.rs` — `AssistantMessage`, `AssistantItem`, `ToolCall`, `ToolResultMessage`, `ToolResultStatus`.
- `ids.rs` — `TurnId` and `ToolCallId` newtypes.

### State machine

`AgentState` (see `state.rs`) has four variants:

- `Idle` — no active turn. The default; also the state the loop returns to after a graceful finish or an interrupt.
- `RunningModel { turn_id }` — a model request is outstanding for `turn_id`; awaiting `ModelCompleted`.
- `RunningTools { turn_id, tool_calls, completed_results, next_result_index }` — one or more tool calls are outstanding. Results may arrive out of order; `completed_results` buffers them by index and `next_result_index` advances through them contiguously so `ToolResult` records are emitted in the order the assistant listed the calls.
- `ReadyToContinue { turn_id }` — an internal transition point reached after every tool in the batch has completed. The mailbox immediately pumps a synthetic `ContinueModel` event, sending the FSM back to `RunningModel`.

```
              StartTurn (at Idle)
                  |
                  v
   +---------> Idle <---------------------+
   |             |                        |
   |             | StartTurn              |
   |             v                        |
   |        RunningModel                  |
   |             |                        |
   |             | ModelCompleted         |
   |             |   (no tool calls)      |
   |             |   => TurnFinished      |
   |             |      {Graceful}        |
   |             +------------------------+
   |             |
   |             | ModelCompleted
   |             |   (with tool calls)
   |             v
   |        RunningTools
   |             |
   |             | every tool completed
   |             v
   |        ReadyToContinue
   |             |
   |             | ContinueModel (synthetic)
   |             v
   |        RunningModel ... (loop)
   |
   | Interrupt (from RunningModel / RunningTools / ReadyToContinue)
   |   => TurnFinished{Interrupted}
   |      + CancelTurn action (skipped from ReadyToContinue)
   +-----------------------------------------------------------+
```

Staleness rules enforced inside `state.rs`: `ModelCompleted` / `ToolCompleted` whose `turn_id` does not match the current state are silently dropped. A `ToolCompleted` whose `tool_call_id` + `tool_name` does not appear in the active batch, or whose index has already been emitted, is also dropped. An interrupted turn's late completions are therefore safely ignored.

### Mailbox + input priority

`Mailbox` (see `mailbox.rs`) holds three queues plus one flag:

- `notifications: VecDeque<AgentEvent>` — volatile completions (`ModelCompleted`, `ToolCompleted`) pushed to the **front** so they preempt queued user work for the currently running turn.
- `steer: VecDeque<UserInputEntry>` — high-priority user inputs to start the next turn.
- `follow_up: VecDeque<UserInputEntry>` — normal-priority user inputs.
- `interrupt_requested: bool` — sticky flag set by `AgentInput::Interrupt`.

Each user input entry carries its sender tags:

```rust
struct UserInputEntry {
    from:    Option<String>,
    kind:    Option<String>,
    content: String,
}
```

Priority order inside `Mailbox::next_event`:

```
1. Interrupt  (only fires if state is RunningModel / RunningTools /
               ReadyToContinue; otherwise the flag is cleared silently)
2. Notification  (front of the VecDeque — ModelCompleted / ToolCompleted)
3. ContinueModel  (synthetic, only when state == ReadyToContinue)
4. Steer         (only consumed when state == Idle)
5. FollowUp      (only consumed when state == Idle)
```

When the mailbox pops a user input entry at `Idle`, it pairs `from` with `kind` into a `TurnOrigin` (present iff both are `Some`). The state machine uses `TurnOrigin` to decide how to open the turn: no origin means a plain `TranscriptRecord::UserMessage(content)`; an origin means `TranscriptRecord::Custom(CustomMessage { kind, content, metadata: { "from": from } })`. This is how agent-routed injections (e.g. a parent directive or a child report) land in the transcript as tagged entries rather than as anonymous user messages. The core does not interpret specific kind strings — those conventions live in `agent-orchestrator` and `agent-session`.

### Actions

`AgentAction` (see `action.rs`) enumerates every request the loop can make of the outside world:

- `RequestModel { turn_id }` — the caller should dispatch a model call for this turn and return its completion via `AgentInput::ModelCompleted`.
- `RequestTool { turn_id, tool_call }` — the caller should execute `tool_call` and return the result via `AgentInput::ToolCompleted`. For parallel batches, one `RequestTool` is emitted per call, in source order.
- `CancelTurn { turn_id }` — the caller should abort all in-flight model and tool work for `turn_id`. The orchestrator is expected to fan this out to every running tool handle.

All three are pure requests. `agent-core` never performs the underlying I/O and never observes whether it succeeded — completions come back through `enqueue_input`.

### Transcript records

`TranscriptRecord` variants (see `record.rs`):

- `TurnStarted { turn_id }` — emitted at the start of every turn.
- `UserMessage(String)` — the content of a human (or untagged) input that opened the turn.
- `AssistantMessage(AssistantMessage)` — the model's output for the turn, containing an ordered `Vec<AssistantItem>` of `Text` and `ToolCall` items.
- `ToolCallStarted { turn_id, tool_call }` — emitted per tool call when the assistant's message is processed, alongside a `RequestTool` action.
- `ToolResult(ToolResultMessage)` — a completed tool result, emitted in assistant-declared order (late results are buffered by `completed_results` until the preceding ones arrive).
- `TurnFinished { turn_id, outcome }` — closes the turn with `TurnOutcome::Graceful`, `Interrupted`, or `Crashed`.
- `Custom(CustomMessage)` — the open extension point. `agent-core` **produces** this variant only in one case: when a tagged `Steer` / `FollowUp` starts a turn at `Idle`, the opening entry is a `Custom` instead of a `UserMessage`. Downstream layers (compaction summaries and branch summaries in `agent-session`, future multi-agent spawn briefs / child reports) append their own `Custom` entries with their own kinds. The core defines the variant and the shape; it knows nothing about specific kind strings.

`TranscriptRecord::turn_id()` returns the turn id for variants that carry one (`TurnStarted`, `ToolCallStarted`, `TurnFinished`) and `None` otherwise.

## What this crate does NOT do

- **No durable storage.** The core has no log, no database, no on-disk journal. `agent-session` owns the durable DAG-of-entries context.
- **No async runtime, no I/O.** No `tokio`, no `async fn`, no threads. Callers drive it synchronously; `agent-session`'s runner module wraps it for async use.
- **No cost / usage / token accounting.** The core emits no usage numbers and does not inspect completions for cost. Metering lives above.
- **No tool execution.** `RequestTool` is a request; the caller runs the tool and feeds back a `ToolResultMessage`.
- **No model provider abstraction.** The core does not know what a model is or how to call one; it just accepts `ModelCompleted { assistant }` and moves on.
- **No multi-agent awareness or routing.** Inter-session routing, session registries, spawn/report semantics, and worklog triggers all live in `agent-orchestrator` / `agent-session`. The core's only concession to multi-agent existence is that `AgentInput::Steer` / `FollowUp` carry optional `from` / `kind` tags, which it propagates into `Custom` records opaquely.
- **No knowledge of specific `Custom` kinds.** Kind strings like `compaction_summary` or `branch_summary` are defined in the session layer, not here.
- **No ID allocation beyond `TurnId`.** `ToolCallId` values arrive from outside via the `ToolCall` structs the model produced; the core does not mint them.
