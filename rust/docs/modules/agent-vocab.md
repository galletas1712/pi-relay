# agent-vocab

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-vocab` owns the serializable data shapes the rest of the stack shares: message blocks, tool calls and results, transcript items, id newtypes, and provider config. It sits at the bottom of the crate graph so [agent-provider](./agent-provider.md), [agent-tools](./agent-tools.md), [agent-store](./agent-store.md), [agent-session](./agent-session.md), and [agent-daemon](./agent-daemon.md) can all talk about messages without depending on the [agent-core](./agent-core.md) FSM. It defines data only — no behavior, no I/O, no async.

## Responsibilities

- Define the wire/storage shapes for user input, assistant output, tool calls, tool results, and transcript log entries.
- Define the id newtypes used to key turns, actions, and tool calls.
- Define provider selection and tuning config (`ProviderKind`, `ReasoningEffort`, `ProviderConfig`).
- Define `ProviderReplayItem`, the opaque per-provider raw payload carried for replay/rehydration.
- Provide serde representations stable enough to persist in Postgres and to send across the [websocket-rpc](../websocket-rpc.md) boundary.

## Key types

### Ids (`ids.rs`)

- `TurnId(u64)` — local turn counter. `first()` is `1`; `next()` increments.
- `ActionId(u64)` — local action counter. `first()` is `1`; `take_next(&mut)` returns the current value and advances.
- `ToolCallId(String)` — opaque provider/tool id. Defaults to `"1"`; supports numeric (`from_u64`, `take_next`) and string construction. `Display`/`as_str` expose the inner string. The string form accommodates provider-issued ids that are not numeric.

`TurnId` and `ActionId` serialize as bare numbers; `ToolCallId` as a bare string.

### Messages (`message.rs`)

```
UserMessage { content: Vec<ContentBlock> }
ContentBlock = Text { text } | Image { image: ImageContent }   (tag = "type")
ImageContent { mime_type, source: ImageSource }
ImageSource  = Base64(String) | Url(String)                    (tag = "kind", content = "value")

AssistantMessage { items: Vec<AssistantItem> }
AssistantItem = Text(String) | ToolCall(ToolCall)

ToolDefinition   { name, description, input_schema: Value }
ToolCall         { id: ToolCallId, tool_name, args_json: String }
ToolResultMessage{ tool_call_id, tool_name, output, status: ToolResultStatus }
ToolResultStatus = Success | Error | Interrupted | Crashed
```

- `UserMessage` carries an ordered block list. `text(..)` builds a single-text message; `as_text()` returns the text only when the message is exactly one `Text` block.
- `ContentBlock` is internally tagged on `type` (`text` / `image`). Images are first-class input so providers can issue vision-capable requests.
- `AssistantItem` has only `Text` and `ToolCall`. There is no thinking variant — thinking blocks are discarded at the provider parse layer (`agent-provider`'s `anthropic.rs`), so they never reach vocab or storage. `AssistantMessage` exposes `tool_calls()` and `text()` helpers over its items.
- `AssistantItem` has a hand-written serde impl: `Text` serializes as `{ "type": "text", "text": .. }` and `ToolCall` as `{ "type": "tool_call", "id": .., "tool_name": .., "args_json": .. }`. Unknown keys are ignored on deserialize; a missing `args_json` defaults to `"{}"`.
- `ToolCall.args_json` is the raw JSON string of arguments; `args_value()` parses it to `serde_json::Value`. `ToolDefinition.input_schema` is a JSON-schema `Value`.
- `ToolResultMessage` has constructors `success`, `error`, `interrupted` (output `"interrupted"`), and `crashed` (output `"crashed before tool result was recorded"`).

### Transcript items (`transcript_item.rs`)

```
TranscriptItem (tag = "type") =
    TurnStarted      { turn_id }
    UserMessage(UserMessage)
    AssistantMessage(AssistantMessage)
    ToolCallStarted  { turn_id, tool_call: ToolCall }
    ToolResult(ToolResultMessage)
    TurnFinished     { turn_id, outcome: TurnOutcome }
    CompactionSummary(CompactionSummary)

TurnOutcome = Graceful | Interrupted | Crashed
```

- `TranscriptItem` is the append-only log entry persisted as a node in the [agent-session](./agent-session.md) transcript forest.
- `turn_id()` returns the owning turn for `TurnStarted`, `ToolCallStarted`, `TurnFinished`, and `CompactionSummary` (via `last_turn_id`); message/result variants return `None` because they are not turn-boundary markers.
- `CompactionSummary` records `source_session_id`, `source_leaf_id`, `summary`, optional `tokens_before`, `last_turn_id`, and optional `turn_started_at_ms`. It is appended as a new transcript root when a branch is compacted; the prior branch stays available for active-leaf switching.

### Provider config (`provider.rs`)

- `ProviderKind = OpenAi | Claude`. `FromStr` accepts `"openai"`, `"claude"`, and the legacy alias `"anthropic"`; it serializes back to `"openai"` / `"claude"`. `"codex"` is rejected — Codex is an auth transport, not a provider kind. See [design decisions](../design-decisions.md) for why OpenAI always routes through the ChatGPT/Codex subscription transport.
- `ReasoningEffort = None | Minimal | Low | Medium | High | XHigh | Max`, default `Medium`. Round-trips through lowercase strings (`"none"`, `"minimal"`, `"low"`, `"medium"`, `"high"`, `"xhigh"`, `"max"`).
- `ProviderConfig { kind, model, reasoning_effort, max_tokens?, prompt_cache? }`. `reasoning_effort` defaults to `Medium`; `max_tokens` and `prompt_cache` (a raw `Value`) are omitted when absent.
- `ProviderReplayItem { provider, raw_json, display? }` carries a provider's raw response payload as a JSON string plus an optional display hint. `ReplayDisplay { kind: LocalTool | HostedTool, pretty_name, input_summary? }` describes how to render hosted/local tool replay entries. Helpers: `new`/`new_with_display`, `raw_value()`, and `raw_type()` (reads the `type` field of the raw payload).

## How it works

```
provider <-> agent-vocab <-> store / session / daemon
   |              |
   |              +-- UserMessage / AssistantMessage / ToolCall / ToolResult
   |              +-- TranscriptItem (persisted forest node)
   |              +-- ProviderConfig / ProviderReplayItem
   |
   +-- parses raw model output, drops thinking, emits AssistantItem::{Text,ToolCall}
```

Every crate above vocab speaks these shapes instead of provider SDK types. Providers translate their wire formats to and from vocab (dropping thinking content on the way in); tools produce `ToolResultMessage`; the store persists `TranscriptItem` nodes, including typed `DaemonToolObservation` items for daemon-authored synthetic tool observations; the daemon serializes the same shapes over [websocket-rpc](../websocket-rpc.md).

The registered builtin tools (defined in [agent-tools](./agent-tools.md), not here) are `edit`, `bash`, `grep`, `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`). There are no `read`/`write` tools. `agent-vocab` only defines the `ToolDefinition`/`ToolCall`/`ToolResultMessage` shapes those tools are expressed in.

## Notes

- No `ThinkingRedacted` (or any thinking) `AssistantItem` variant exists. Thinking is dropped before it reaches vocab; do not add a variant to "preserve" it.
- `AssistantItem` serde is hand-written, not derived — keep its field layout in sync with the `Serialize`/`Deserialize` impls if the variant shape changes, and keep the round-trip test in `message.rs` passing.
- `ToolCallId` is a string by design; never assume it parses as an integer. `take_next`/`from_u64` are conveniences for runtime-issued ids only.
- `ProviderConfig` derives `Serialize`/`Deserialize` but not `PartialEq`/`Eq` (it holds free-form `Value` fields); compare components explicitly if needed.
- Adding a new `TranscriptItem` variant means touching every match over it (storage row mapping, transcript projection, RPC codec) — there is no catch-all arm.
- Related plans: [tool-surface](../plans/tool-surface.md) and [transcript-ui](../plans/transcript-ui.md) build on these shapes; the [web UI](../../../packages/web/docs/web-ui.md) consumes their JSON forms.
