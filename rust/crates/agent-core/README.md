# agent-core

Deterministic FSM kernel for one agent turn loop.

## Responsibility

`agent-core` owns the live control state for a single session:

- priority mailbox
- turn state machine
- action outbox
- transcript-item outbox
- `TurnId` and `ActionId` progression over IDs defined by `agent-vocab`

It does not call models, run tools, persist history, know storage backends, or
know about hierarchical agents. Callers feed inputs in, call `drive`, then drain
the requested actions and transcript items.

## Inputs

`AgentInput` variants:

- `Interrupt`
- `Steer`
- `FollowUp`
- `ModelCompleted`
- `ModelFailed`
- `ToolCompleted`

`Steer` and `FollowUp` carry a `UserMessage`, which can contain text and image
content blocks. The core does not model caller-authored injected context,
subagent routing, compaction, or storage.

## Outputs

`TranscriptItem` variants:

- `TurnStarted`
- `UserMessage(UserMessage)`
- `AssistantMessage`
- `ToolCallStarted`
- `ToolResult`
- `TurnFinished`

`AgentAction` variants:

- `RequestModel`
- `RequestTool`
- `CancelTurn`

The core never performs those actions. It only requests them.

## Important Semantics

- Completion inputs must match the active `action_id` and `turn_id`, otherwise
  they are ignored as stale.
- Tool results can arrive out of order, but transcript `ToolResult` items are
  emitted in the assistant-declared tool-call order.
- Interrupting model/tool work closes the turn as interrupted and emits a
  cancellation request for external work.
- User input becomes `TranscriptItem::UserMessage`.

## Relationship To Other Crates

- `agent-vocab` owns the shared message/transcript shapes.
- `agent-session` owns durable history, resume, rewind, fork, and session
  integration. Compaction is installed durably by `agent-store`.
- Providers and tools live above the core.
