# agent-session

Durable session semantics around the `agent-core` FSM kernel.

## Responsibility

`agent-session` turns the pure `AgentCoreLoop` into an editable session value.
It owns:

- `AgentCoreLoop` — deterministic turn/tool state.
- `TranscriptStore` — append-only forest of `TranscriptStorageNode`s with one
  active root-to-leaf path.
- `OutstandingActions` — private bookkeeping for model/tool requests sent to
  the runtime and completion events waiting for transcript acceptance.
- Current context token count — optional runtime-provided token count for the
  active model-visible context.
- Action and event outboxes — ephemeral live outputs for the daemon/runtime.

`session.drive()` is the only supported way to advance the FSM. It runs the
core to quiescence and drains freshly produced `TranscriptItem`s into the
`TranscriptStore`, which is the sole owner of live model-visible history.

This crate does not call providers, run tools, persist SQL, decide compaction
policy, or expose websocket RPC. Compaction is now a store/daemon operation:
the daemon summarizes a `ModelContext`, and `agent-store` atomically appends a
typed compacted root.

## Public Interface

`AgentSession` exposes `drive`, `enqueue_input`, `enqueue_session_input`,
`drain_actions`, `drain_events`, `drain_pending_inputs`, `model_context`, and
`transcript_store`.

`SessionAction` contains only:

- `RequestModel`
- `RequestTool`
- `CancelSessionWork`

`RequestModel` carries a materialized `ModelContext` for immediate live
dispatch plus the `context_leaf_id` that names the canonical transcript
checkpoint. The durable Postgres action row stores only that leaf reference and
context-token metadata; recovery can rebuild the full context from
`StoredTranscriptEntry` values, including each entry's provider replay sidecar.

`SessionInput` contains session-level model completion forms. Plain core inputs
use `AgentSession::enqueue_input`.

`SessionEvent` is live-only activity: transcript append and action
requested/completed/failed. Durable compaction and history-switch events are
emitted by `agent-store`.

`StoredSession` and `StoredTranscriptEntry` are session snapshot types. They
belong here because they describe how to rehydrate session semantics; they are
not a storage-backend trait.

## Transcript Forest

Each `TranscriptStorageNode` has a UUID string id, an optional parent id,
timestamp, and one `TranscriptItem`. Entries form a forest. The active session
view is exactly one root-to-leaf path, and `ModelContext` is derived by walking
that path.

`TranscriptItem::CompactionSummary` is a valid boundary root. It stores
lineage to the summarized source session/leaf plus the last source turn id so
post-compaction turns continue numbering correctly. Its `parent_id` is `None`;
lineage is provenance, not model-visible ancestry.

## Recovery

Restoring from transcript items or a transcript store is intentionally
quiescent. Open transcript turns are closed as crashed, the core is rebuilt
idle at that boundary, and unfinished external action rows are handled by the
daemon/store recovery layer.

The websocket daemon applies strict lifecycle rules: source-mutating history
writes are idle-only.

## Module Map

| File | Contents |
| --- | --- |
| `src/lib.rs` | Module declarations and public session exports. |
| `src/action.rs` | `SessionAction` and completion matching. |
| `src/input.rs` | `SessionInput`, `SessionInputError`. |
| `src/event.rs` | Runtime-only `SessionEvent`, `SessionActionKind`. |
| `src/outstanding_actions.rs` | Private model/tool request tracking. |
| `src/session.rs` | `AgentSession`, drive/input/action lifecycle, restore rehydration. |
| `src/session_tests.rs` | Session lifecycle tests. |
| `src/model_context.rs` | `ModelContext` read-only view and open-turn closure. |
| `src/transcript_store.rs` | Transcript forest, indexes, materialization, boundary checks. |
