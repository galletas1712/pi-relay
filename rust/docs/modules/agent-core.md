# agent-core

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-core` is the deterministic finite state machine for a single agent turn loop. It has no I/O, no async runtime, and no knowledge of providers, storage, or websockets. It accepts external inputs on a priority mailbox, advances a small state enum, and drains two outputs per step: durable [`TranscriptItem`](./agent-vocab.md)s (the model-visible record of the turn) and `AgentAction`s (side effects the caller must perform — model requests, tool requests, turn cancellation). The crate forbids `unsafe`, and `AgentState`/`Mailbox` are private; callers observe liveness only through the methods on `AgentCoreLoop`.

## Responsibilities

- Decide, given the current state and the next input, what transcript items to append and what side effects to request.
- Sequence model and tool work for one turn: request the model, fan out tool calls, collect results in source order, then resume the model.
- Allocate monotonic `ActionId`s for every model/tool request and `TurnId`s for every new turn.
- Reject stale or mismatched completions so a turn cannot be corrupted by late callbacks.
- Stay pure: no time, no randomness, no I/O. The same inputs always yield the same transcript items and actions.

It does **not** own durable history, execute models or tools, or persist anything. Those belong to [agent-session](./agent-session.md) and the daemon. See [design decisions](../design-decisions.md) for why the kernel is isolated this way.

## Key types

`AgentCoreLoop` — the public driver. Holds the private `Mailbox`, the private `AgentState`, the `last_turn_id` / `next_action_id` counters, and two output queues (`action_outbox`, `transcript_item_outbox`). Public surface:

- `new()` / `default()` — fresh idle core at turn 0.
- `resume_at(last_turn_id, next_action_id)` — fresh idle core continuing an existing session's id sequence.
- `resume_running_model(turn_id, action_id)` — resume with a model request already in flight (durable storage owns the persisted action row; the core only needs the correlation ids to accept the eventual completion).
- `resume_ready_to_continue(turn_id, next_action_id)` — resume after a tool batch finished but before the follow-up model request was dispatched.
- `enqueue_input(input)` — push an `AgentInput` onto the mailbox.
- `drive()` — process mailbox events until quiescent (see below).
- `drain_actions()` / `drain_transcript_items()` — take the accumulated outputs.
- `drain_pending_inputs()` — remove queued user inputs without advancing the FSM (for introspection/tests; notifications and the interrupt flag are untouched).
- `is_idle()`, `is_ready_to_continue()`, `has_pending_work()`, `last_turn_id()`, `next_action_id()` — liveness/inspection.

`AgentInput` (public) — external input delivered by the caller:

- `Interrupt` — stop active model/tool work.
- `Steer { content }` — high-priority user input; runs before queued follow-ups.
- `FollowUp { content }` — normal-priority user input for the next available turn.
- `ModelCompleted { action_id, turn_id, assistant }` — a model request returned an `AssistantMessage`.
- `ModelFailed { action_id, turn_id, error }` — a model request failed.
- `ToolCompleted { action_id, turn_id, result }` — a tool produced a `ToolResultMessage`.

`Steer` / `FollowUp` carry a `TurnInput(UserMessage)`. The `steer`/`follow_up` constructors take plain text; `steer_message`/`follow_up_message` take a structured `UserMessage` (including images). `ModelCompleted` and the failure/tool variants are volatile control signals correlated by `(turn_id, action_id)`.

`AgentAction` (public) — side effects requested of the caller:

- `RequestModel { action_id, turn_id }`
- `RequestTool { action_id, turn_id, tool_call }`
- `CancelTurn { turn_id }` — cancel all active model/tool work for the turn; the caller fans this out to every running tool handle for that `turn_id`.

`AgentState` (private) — the four control states:

```
Idle
RunningModel    { turn_id, action_id }
RunningTools    { turn_id, tools: Vec<RunningTool>, next_result_index }
ReadyToContinue { turn_id }
```

`RunningTool` holds the originating `ToolCall`, its `action_id`, and an optional buffered `result`. `AgentEvent` is the private internal event the mailbox hands to the state machine; it mirrors `AgentInput` but replaces queued user inputs with positioned `StartTurn`/`Steer` events and adds the synthetic `ContinueModel`.

## How it works

The mailbox is the only entry point. `enqueue_input` routes each `AgentInput` into one of four lanes:

```
interrupt_requested : bool            (set by Interrupt)
notifications       : VecDeque        (Model/Tool completions + failures, FIFO)
steer               : VecDeque        (high priority user input)
follow_up           : VecDeque        (normal priority user input)
```

Completions and failures are pushed to the back of the notification queue so they preempt queued user work while preserving arrival order relative to each other.

`drive()` repeatedly asks the mailbox for the `next_event` for the current state, applies it via `AgentState::step`, and appends any emitted items/actions to the outboxes. It stops when the mailbox yields nothing, or as soon as the core reaches `ReadyToContinue` — so the caller can decide whether to resume the model or run a compaction before the next request. Empty transitions (ignored stale inputs) are skipped without ending the loop.

### Mailbox priority

`next_event` drains strictly by priority:

1. **Interrupt** — only if a turn is active (`RunningModel` / `RunningTools` / `ReadyToContinue`). A pending interrupt while `Idle` is dropped.
2. **Notification** — the next queued model/tool completion or failure.
3. **State-dependent user input:**
   - `ReadyToContinue` consumes a **steer** if one is queued, otherwise emits the synthetic `ContinueModel`. Follow-ups are *not* consumed here — they wait until the turn fully closes and the core is `Idle`.
   - `Idle` consumes the next user input, steer before follow-up, as a `StartTurn` carrying the next `TurnId`.
   - `RunningModel` / `RunningTools` consume no user input (the turn is busy).

### Turn flow

```
Idle ──StartTurn──▶ RunningModel
                         │  ModelCompleted (no tool calls)──▶ TurnFinished(Graceful) ─▶ Idle
                         │  ModelFailed ───────────────────▶ TurnFinished(Crashed)  ─▶ Idle
                         │  ModelCompleted (with tool calls)
                         ▼
                   RunningTools  ◀─┐ ToolCompleted (buffered until all in source order)
                         │         │
                         ▼ all results recorded
                   ReadyToContinue ──Steer──▶ RunningModel (new user turn input, same turn)
                         │
                         └──ContinueModel──▶ RunningModel
```

Starting a turn (`Idle` + user input) emits `TurnStarted` and the `UserMessage`, then `RequestModel`, and moves to `RunningModel`.

On `ModelCompleted`, the core appends the `AssistantMessage`. If it carries no tool calls the turn finishes `Graceful` and returns to `Idle`. If it carries tool calls, the core appends a `ToolCallStarted` and emits a `RequestTool` for each (in source order, each with a fresh `action_id`), and moves to `RunningTools`. Multiple tool calls run in parallel.

In `RunningTools`, each `ToolCompleted` is matched to its `RunningTool` and buffered. `next_result_index` is a low-water mark: results are appended as `ToolResult` items strictly in the model's source order, regardless of the order completions actually arrive. Once every tool has reported, the state becomes `ReadyToContinue`. From there `ContinueModel` (or a consumed steer) requests the model again within the same turn.

### Stale / mismatched input rejection

Every accepted transition is guarded so out-of-band callbacks cannot corrupt a turn — a mismatch yields an empty transition (no items, no actions) and the state is unchanged:

- `ModelCompleted` / `ModelFailed` apply only in `RunningModel` and only when both `turn_id` and `action_id` match the in-flight request.
- `ToolCompleted` applies only in `RunningTools`, only for the active `turn_id`, only when the `tool_call_id` **and** `tool_name` identify a pending tool whose `action_id` matches, and only when that result has not already been recorded (index not below `next_result_index`, slot still empty).

So a completion that arrives after an interrupt has reset the core to `Idle` is silently discarded.

### Interrupt cleanup

`Interrupt` always drives the core back to `Idle` and closes the turn `Interrupted`:

- From `RunningModel`: emit `TurnFinished(Interrupted)` and `CancelTurn`.
- From `RunningTools`: for every tool at or past `next_result_index`, emit its buffered result if present or synthesize a `ToolResult` with `crashed` status, then `TurnFinished(Interrupted)` and `CancelTurn`. Tools whose results were already recorded are left intact.
- From `ReadyToContinue`: all external work is already done, so emit only `TurnFinished(Interrupted)` (no `CancelTurn`).

## Notes

- **Determinism / no I/O.** The kernel touches no clock, RNG, network, filesystem, async runtime, provider SDK, tool executor, or storage. All persistence and execution live above it.
- **The core does not own history.** Resume constructors take only the id watermarks; durable transcript history and the persisted action rows belong to [agent-session](./agent-session.md). `drive` emits transcript items for the caller to store, but never replays or buffers prior turns.
- **Thinking blocks never reach the core.** `AssistantItem` is `{ Text, ToolCall }` only; provider thinking/reasoning blocks are discarded at the provider parse layer, so the FSM only ever sees text and tool calls.
- **`ReadyToContinue` is a deliberate pause.** `drive` returns there so the caller can interpose (e.g. compaction) before the follow-up model request. A queued steer is folded into the same turn; a queued follow-up only starts a fresh turn once the current one closes.
- **Mailbox is volatile.** Queued inputs, notifications, and the interrupt flag are not durable; after a crash the session reconstructs an idle core from persisted transcript items and recovers open tails as crashed.
- **Intentionally not modeled.** Injected/system context, context sources, sub-agents, and routing metadata are out of scope for `agent-core`. It models exactly one turn loop's control flow and nothing about *what* goes into a model request — that assembly happens in [agent-session](./agent-session.md) and the prompt layer.
