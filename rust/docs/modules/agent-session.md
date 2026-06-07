# agent-session

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-session` wraps the pure FSM in [agent-core](./agent-core.md) with durable history. `AgentSession` is the sole owner of durable transcript items: every item the core produces flows into a `TranscriptStore`, an append-only forest in which the session's active view is one root-to-leaf path. From that path the session materializes a `ModelContext` for provider requests and daemon-owned compaction. Each transcript node carries an opaque `provider_replay` sidecar so reasoning/tool continuation state survives across turns, branches, switches, and compaction without depending on any provider-retained server state.

## Responsibilities

- Own the core loop and the durable transcript forest; drain core output into the store on every `drive`.
- Materialize a contiguous `ModelContext` (visible items + provider-replay sidecar) from the active path for model requests and compaction.
- Carry session-level model completions that the pure core does not understand (the provider-replay sidecar, max-output-token failures).
- Track outstanding model/tool work so stale completions after an interrupt or history edit cannot mutate history.
- Restore/resume a session from a storage snapshot, repairing open turn tails.
- Branch-aware history operations: resume a crashed/interrupted turn, install a compaction root, switch the active leaf.

## Key types

`AgentSession` owns exactly:

```
core               AgentCoreLoop          // pure FSM
transcript_store   TranscriptStore        // durable forest, sole owner of items
outstanding_actions OutstandingActions    // ledger of in-flight model/tool work
action_outbox      VecDeque<SessionAction>
event_outbox       VecDeque<SessionEvent>
```

There is no context-token-count field on the session; token accounting lives with the daemon and compaction, not here.

`TranscriptStorageNode` is one forest node:

```rust
pub struct TranscriptStorageNode {
    pub id: String,
    pub parent_id: Option<String>,   // None == a root
    pub timestamp_ms: u64,
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}
```

`TranscriptStore` keeps entries indexed by id, parent, and current leaves, plus an `active_leaf_id`. A node is a *turn boundary* iff its item is `TurnFinished` or `CompactionSummary` (the empty store is also a boundary). History edits are only legal at a boundary.

`ModelContext` is a derived, ordered view over the active path:

```rust
pub struct ModelContextEntry {
    pub item: TranscriptItem,
    pub provider_replay: Vec<ProviderReplayItem>,
}
```

The store is canonical; `ModelContext` is rebuilt whenever the session needs to feed the core or a provider a contiguous history. It is also the type used to make a copied or recovered open turn structurally complete.

`SessionAction` is the work the session emits to the daemon: `RequestModel { action_id, turn_id, model_context, context_leaf_id }`, `RequestTool { action_id, turn_id, tool_call }`, and `CancelSessionWork` (a session-wide, idempotent, best-effort invalidation barrier). `SessionEvent` is ephemeral observer activity (`TranscriptItemAppended`, `ActionRequested`, `ActionCompleted`, `ActionFailed`) — not transcript entries. `SessionInput` carries the two completions the core cannot accept directly: `ModelCompleted` (with provider-replay) and `ModelMaxOutputTokens`. `StoredSession` / `StoredTranscriptEntry` are the serializable storage snapshot shapes (see [agent-store](./agent-store.md)).

## How it works

### Two lanes per entry

Each node carries two parallel lanes:

```
visible lane          provider-replay lane (opaque)
TranscriptItem        Vec<ProviderReplayItem>
  TurnStarted           reasoning / encrypted_content (OpenAI)
  UserMessage           thinking / redacted_thinking / signature (Anthropic)
  AssistantMessage      raw function_call / tool_use, tool outputs
  ToolCallStarted       ...kept byte-shape-stable for exact replay
  ToolResult
  TurnFinished
  CompactionSummary
```

The visible lane is what the UI renders (text, tool calls, tool results, turn outcomes, compaction roots, branches). The provider-replay lane is sent back to the provider so a stateless request can continue a reasoning/tool loop, and is otherwise invisible (thinking/reasoning blocks never become transcript rows — they are discarded at the provider parse layer and never enter the visible vocab). The two lanes are aligned by entry but are not the same abstraction; `ModelContext` carries both so neither is lost across any history operation. See [agent-provider](./agent-provider.md) for how each lane is serialized to OpenAI Responses or Anthropic Messages, and [design decisions](../design-decisions.md) for why replay lives outside UI rendering.

### Drive loop

```
enqueue_input / enqueue_session_input
        |
      drive()  ->  core.drive()
        |            |
        |        drain_transcript_items  -> append to store, emit TranscriptItemAppended
        |        drain_actions           -> queue RequestModel/RequestTool, track outstanding
        v
   drain_actions() / drain_events()  (daemon consumes)
```

`enqueue_input` is the only way to feed the core from outside; the core is never exposed, so context materialization in `drive` cannot be bypassed. A plain `ModelCompleted` is rejected there because it must carry the provider sidecar — it must arrive via `SessionInput::ModelCompleted`. `RequestModel` always snapshots the current `ModelContext` and the leaf it was built from (`context_leaf_id`).

`OutstandingActions` is a ledger of model/tool requests that have left the session but have not yet been re-accepted into the transcript. It rejects stale completions (no matching pending work, e.g. after an interrupt), and delays the `ActionCompleted`/`ActionFailed` event until the matching transcript item proves the core actually accepted the completion. `CancelSessionWork` itself never creates a model/tool action row.

### Restore and resume

`from_stored_session` / `from_model_context` rebuild a session from durable history. If the active branch ends mid-turn (no trailing boundary), the open tail is closed as **crashed** before the session is exposed: open tool calls are patched with crashed `ToolResult`s, then a `TurnFinished{Crashed}` is appended. The recovered session is idle and derives its resume point only from the persisted path, never from volatile in-flight state. If the open turn already has all tool results present (`open_turn_ready_to_continue`), the core resumes ready-to-continue instead of crashing the tail.

`resume_model_turn` retries a terminal crashed/interrupted turn from its original checkpoint leaf. The old terminal branch stays durable; new model output appends as a **sibling branch** under the checkpoint, so retry never duplicates the user's original message. `restore_compacted_runtime` re-arms a running model turn against a chosen leaf after a compaction install.

### Switch

Switch moves the active leaf to a prior turn boundary without deleting any rows. It is **transcript-only**: workspace files are not checkpointed or restored. Because the replay sidecar travels with each node, switching to a point inside a tool loop selects the ancestor replay records along with the visible items. The session primitive can invalidate active work on a local switch; the websocket contract is stricter and makes source-mutating writes idle-only (see [websocket-rpc](../websocket-rpc.md)).

### Compaction

```
active path (long history)              new active root
TurnStarted ... TurnFinished   --->     CompactionSummary  (parent_id = None)
   (old branch stays durable)              + continuation suffix...
```

`install_compaction_checkpoint` appends a typed `TranscriptItem::CompactionSummary` as a new **root** (`parent_id = null`) carrying the summary text, `tokens_before`, the last turn id, and the source session/leaf it summarizes. Any `continuation_suffix` (e.g. an open tool loop that must not be summarized away) is re-parented onto the new root so the active trajectory is preserved intact. The summary root is a turn boundary, so `last_turn_id` and downstream context resolve from it. The old branch remains available for same-session active-leaf switching and tree inspection. Compaction is not a session boundary.

## Notes

- The store is the single source of durable items; the core only buffers items for the current run until `drive` drains them. There is no `drain_transcript_items` on the session — items go straight into the store.
- A node is editable history only at a turn boundary (`TurnFinished` / `CompactionSummary` / empty). Attempting an edit while mid-turn with no interruptible core turn returns `HistoryOperationError::Busy`.
- Open tool calls are completed deterministically on close: missing `ToolCallStarted`/`ToolResult` items are synthesized (crashed results) so a recovered or copied turn is always provider-valid — no orphaned `tool_use`/`tool_result`, no missing result for a completed call.
- `to_stored_session` persists only the transcript forest and `active_leaf_id`. Runtime mailboxes, outstanding requests, and outboxes are intentionally excluded: resume semantics come from the persisted path, not volatile work.
- `ModelMaxOutputTokens` is handled session-side: it appends the partial assistant message (with its replay sidecar) plus a crashed `TurnFinished` and emits `ActionFailed`, leaving a recoverable, sibling-resumable tail.
- `provider_replay` is opaque to this crate — it is never interpreted here, only carried and re-aligned. Provider translation belongs in [agent-provider](./agent-provider.md), not in the session or core.
- Branch/switch/compaction continuity is verified end to end against real providers; the staged rollout that introduced the replay lane is folded into this crate and [agent-provider](./agent-provider.md), and the [web UI](../../../packages/web/docs/web-ui.md) renders only the visible lane.
