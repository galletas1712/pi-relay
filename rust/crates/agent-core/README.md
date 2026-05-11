# agent-core

Deterministic FSM kernel for one agent turn loop.

## Responsibility

`agent-core` owns the live control state for a single session:

- priority mailbox
- turn state machine
- action outbox
- transcript-item outbox
- `TurnId` and `ActionId` progression

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
content blocks. They may also carry opaque `from` and `kind` tags. If tags are
present, both must be present.

## Outputs

`TranscriptItem` variants:

- `TurnStarted`
- `UserMessage(UserMessage)`
- `AssistantMessage`
- `ToolCallStarted`
- `ToolResult`
- `TurnFinished`
- `Injected`

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
- Untagged user input becomes `TranscriptItem::UserMessage`.
- Tagged user input becomes `TranscriptItem::Injected` with the tag metadata.

The tagged path is generic injected context, not subagent orchestration.

## Relationship To Other Crates

- `agent-vocab` owns the shared message/transcript shapes.
- `agent-session` owns durable history, resume, rewind, fork, compaction, and
  session integration.
- Providers and tools live above the core.
