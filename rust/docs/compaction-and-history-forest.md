# Compaction And History Forest Proposal

Status: implemented first pass, reviewed by subagent

## Problem

The current Rust session model already stores transcript entries as a parent
linked forest: each `TranscriptStorageNode` has one optional `parent_id`, and a
session has one `active_leaf_id`. Rewind and fork fit this model well, but
compaction currently does not. Compaction asks an external harness to return a
full replacement `ModelContext`, then `AgentSession` replaces the active path.

That replacement lifecycle is more complex than the semantics we want:

- the durable transcript should remain append-only
- old branches should remain available for tree view, fork, and rewind
- compaction should change the model-visible active context without deleting or
  rewriting the summarized history
- rewind and fork should feel like the same operation: pick a point in history
  and continue from there

## Proposed Primitive

Make the core history operation:

```text
pick a point in the transcript forest
optionally create a new branch/root from it
make that branch/root the active leaf
```

Then the user-facing operations are small variations:

- rewind: choose a safe point in the current session and set it active. Sending
  the next message creates a new child branch from that point.
- fork: choose any explicit transcript point and create a new branch, usually
  in a new session, with an editable restored user message when the selected
  point is a user message.
- compaction: choose the current active leaf as the source, summarize the model
  context reachable from it, create a new compacted root, and set that compacted
  root active.

This makes "rewind then send" and "fork then send" the same data operation. The
UI can present the difference as workflow, not as separate storage semantics.

## Current Code Shape

Relevant current files:

- `agent-session/src/transcript_store.rs`: describes entries as a forest and
  materializes context by walking from `active_leaf_id` to a root.
- `agent-session/src/session.rs`: owns live turn mechanics plus rewind/fork
  validation. It does not own compaction state.
- `agent-daemon/src/main.rs`: exposes `history.rewind`, `history.fork`, and
  provider-backed `compaction.request` separately at the websocket boundary.
- `agent-daemon/src/runtime.rs`: owns the session driver and compaction worker
  tasks. Manual compaction is a durable action barrier between turns.
- `agent-store/src/postgres/transcript.rs`: persists append-only transcript
  entries and the session `active_leaf_id`.
- `agent-store/src/postgres/compaction.rs`: creates and completes compacted-root
  actions atomically.

The key simplification is that compaction now uses the same transcript forest as
rewind and fork. It no longer asks the live session FSM to validate and install
an arbitrary replacement context.

## Proposed Data Model

Add a first-class compacted root entry. The simplest shape is a new transcript
item:

```rust
TranscriptItem::CompactionSummary {
    source_session_id: String,
    source_leaf_id: String,
    summary: String,
    tokens_before: Option<usize>,
    last_turn_id: TurnId,
}
```

The parent of this entry is `None`, because it is a new model-visible root. The
`source_session_id` and `source_leaf_id` fields are lineage pointers, not
model-context parents. That distinction matters:

- `parent_id` answers "what entries are visible before this one when building
  provider context?"
- `source_session_id` and `source_leaf_id` answer "what older branch did this
  summary come from?"

The transcript store therefore becomes a forest with two edge types:

- solid parent edges for model-visible context
- lineage edges for UI/history provenance

The active path after compaction is:

```text
CompactionSummary(source_session_id = current_session, source_leaf_id = old_active_leaf)
  -> future turn
  -> future turn
```

The old path remains unchanged:

```text
old root -> ... -> old_active_leaf
```

The UI can draw the compacted root as a new root with a dashed provenance edge
back to `source_leaf_id`.

`last_turn_id` is required because a fresh compacted root contains no
`TurnStarted` / `TurnFinished` item. Today `AgentSession` resumes the next turn
number from the materialized model context's last turn id. Without storing the
source branch's last turn id on the compaction summary, the next post-compaction
turn would restart at `TurnId(1)`. The summary item should therefore behave like
a transparent context item for provider rendering, but still contribute
`last_turn_id` for session resume.

Lineage must be cross-session. Existing fork behavior copies transcript entries
into a new session, so a forked compacted root may point back to a source entry
that does not exist in the child session. Keeping `source_session_id` makes that
provenance explicit. The UI can choose whether to follow cross-session lineage;
the provider must not.

## Provider Rendering

Providers render `CompactionSummary` as a user-role context message:

```text
The conversation history before this point was compacted into this summary:

<summary>
...
</summary>
```

OpenAI and Anthropic can each adapt this to their normal user-role context
format. The important invariant is that compaction summaries are model-visible,
but their lineage edge is not.

## Compaction Lifecycle

The daemon should own the compaction job, not `agent-session`'s live turn FSM.
The proposed lifecycle:

1. Client requests compaction for a session.
2. Daemon acquires the session driver and requires an idle source mutation.
3. Store creates a durable compaction action row that records the source active
   leaf, source session id, and action attempt.
4. Store loads the active model context and last turn id for that source leaf.
5. Daemon runs a dedicated compaction provider path with a compaction prompt and
   no tools. This prompt summarizes only the dynamic model context materialized
   from the selected transcript branch. The configured global system prompt is
   still rendered as the stable first message for normal model turns, but it is
   not part of the transcript slice that compaction summarizes.
6. Store atomically inserts a new `CompactionSummary` root and sets it active,
   but only if the session's active leaf still equals the source leaf and the
   action attempt is still current.
7. Store marks the action complete and emits transcript/history events with the
   new root id, source session id, and source leaf id.

The atomic completion check is essential, not optional cleanup. Current runtime
output persistence only writes entries surfaced through transcript append
events; a replacement path mutation is not enough to persist a compacted root.
The compacted root transaction must be an explicit Postgres operation.

If the user rewinds or forks while compaction is running in a later version that
permits background compaction, the completion must not steal active focus from
the new branch. It can either fail as stale or persist the summary root without
making it active. For the initial implementation, idle-only compaction avoids
most of that surface, but the durable action row and compare-and-set completion
still make daemon death and retry behavior explicit.

## Automatic Compaction

Automatic compaction should still block forward model progress. Removing
replacement-context compaction from `agent-session` does not mean model turns can
race past compaction.

The intended automatic flow is:

1. The session driver is about to dispatch a model request.
2. It checks the active context token state or provider/context-window policy.
3. If compaction is needed, it creates a durable compaction action instead of
   dispatching the model request.
4. The session remains active/running, and ordinary user messages may queue, but
   no later model request for that session is dispatched.
5. When compaction completes, the Postgres compare-and-set transaction installs
   the compacted root as the active leaf.
6. The session driver reloads from that active leaf and continues with the model
   request that was blocked behind compaction.

This is a blocking gate at a model-request boundary, not a mid-turn active-path
replacement. That distinction is important:

- Proactive auto-compaction before a provider call should block and then resume
  the blocked model request from the compacted root.
- Manual compaction between turns should also block queued sends until the
  compacted root is installed or the compaction fails.
- Context-overflow recovery after a provider error can still do a
  compact-and-retry flow, but it should be modeled explicitly as retrying a
  failed model request after installing a compacted root. It should not require
  an arbitrary replacement `ModelContext`.

So the model-barrier behavior to keep is "do not dispatch the next model call
until compaction is done." The behavior to retire is "ask a harness to return an
entire replacement transcript path while a live `AgentSession` validates that
replacement against an open-turn suffix."

Unfinished actions are process-owned execution, not durable semantic session
state. A clean turn boundary may legitimately have an unfinished compaction
action while the daemon is alive, so queued inputs must not treat that as a
transcript-repair case. On daemon startup, the store marks any unfinished action
rows stale because the provider/tool future from the previous process cannot
resume. If the transcript itself has an open turn, first touch still repairs the
tail by appending a crashed boundary.

## Fork And Rewind Semantics

Rewind and fork should share the picker/tree backing model:

- the picker chooses a transcript point, not an opaque command argument
- user-message selections mean "branch from before this message and restore it
  into the composer"
- rewind is restricted to points that can become a valid active source for a
  future turn
- fork can target any explicit transcript entry by closing/recovering an open
  turn suffix as interrupted in the copied branch

The main transcript view should render only the active root-to-leaf path. The
tree view should render inactive branches and compacted roots.

The current web rewind picker only scans the active branch, while fork can scan
all entries. To make pre-compaction history reachable after compaction, rewind
must move to the tree-backed picker as well. Otherwise the backend may support
rewinding to old branches while the UI hides those targets.

## Compaction Schemes

### Scheme 1: Prefix Summary Plus Verbatim Suffix

This is the pi-mono shape. It summarizes old history and keeps a recent suffix
verbatim in future model context.

It is compatible with the forest model, but there are two implementation
choices:

- copy the suffix entries under the new compacted root
- keep the suffix on the old branch and teach context materialization to splice
  `summary + old suffix`

Copying is simple and keeps `model_context()` a plain parent walk, but duplicates
entries. Splicing avoids duplication, but makes materialization special and
harder to reason about with fork/rewind.

### Scheme 2: Fresh Summary Context

This is closest to opencode's active-history behavior. The compaction job may
include recent messages in its summarization prompt, but the future
model-visible context starts from the generated summary.

This fits the proposed forest model best:

```text
old branch remains intact
new compacted root contains summary
future turns append below the compacted root
```

The downside is that no recent suffix remains verbatim unless the summary
captures it. For a personal coding agent, that is probably acceptable if the
prompt asks for concrete files, decisions, constraints, and unresolved work.

### Scheme 3: Hybrid Summary Plus Copied Suffix

This keeps the simplicity of a new root while preserving recent exact context:

```text
CompactionSummary(source_session_id = current_session, source_leaf_id = old leaf)
  -> copied recent suffix entry
  -> copied recent suffix entry
  -> future turn
```

This is compatible with the forest and preserves provider context, but it
duplicates transcript entries and requires the UI to mark copied entries or
hide duplication in the tree. It is a possible later upgrade if fresh-summary
compaction loses too much useful detail.

## Recommendation

Start with scheme 2: fresh summary context.

Reasons:

- it matches the existing parent-walk materialization model
- it avoids replacement contexts
- it keeps compaction append-only
- it preserves fork/rewind from the pre-compaction subtree
- it avoids special splicing logic in `TranscriptStore::model_context`
- it is easy to show in a tree UI as a new root with provenance

If fresh-summary compaction feels too lossy in practice, add scheme 3 later by
copying a small recent suffix under the compacted root. Do not start with
spliced materialization unless duplication proves unacceptable.

## Implemented Plan

1. Add a typed compaction transcript item.
   - Prefer `TranscriptItem::CompactionSummary` over stringly `Injected` kind.
   - Include `source_session_id`, `source_leaf_id`, `summary`,
     `tokens_before`, and `last_turn_id`.
   - Thread the new variant through structural validation, turn-boundary checks,
     last-turn-id discovery, stored snapshot serde, provider adapters, and web
     transcript types.
   - Treat it as model-visible context for provider rendering and transparent
     for turn-boundary checks.

2. Render compaction summaries in providers.
   - OpenAI: user-role message with text summary wrapper.
   - Anthropic: user-role text content with the same wrapper.

3. Move compaction out of the live session FSM.
   - Remove or demote `CompactionState`.
   - Remove `SessionInput::CompactionCompleted`.
   - Remove replacement-context completion as the primary compaction path.
   - Explicitly retire the current model-barrier compaction behavior that can
     hold an in-flight model request and validate a replacement open-turn suffix.
     The daemon RPC is idle-only today; if automatic in-turn compaction returns
     later, it should be designed as a separate feature.

4. Add store transaction for compacted roots.
   - Insert the summary entry with `parent_id = null`.
   - Set `sessions.active_leaf_id` to the new entry.
   - Require `active_leaf_id == source_leaf_id` at completion time and the
     action attempt to still be current.
   - Mark the compaction action complete or stale in the same transaction.
   - Emit transcript append and history-compacted events with
     `new_root_id`, `source_session_id`, `source_leaf_id`, and `tokens_before`.

5. Make daemon compaction provider-backed.
   - Load active context.
   - Build compaction prompt.
   - Run provider with no tools.
   - Complete the store transaction.
   - Use a dedicated `run_compaction` path instead of `run_model`, since normal
     model turns include the global prompt, dynamic prompt context, and tools.
   - Preserve the existing auth retry behavior for Codex/OpenAI.

6. Update RPC and UI tree views.
   - Treat compaction as another branch/root event.
   - Draw active path separately from full forest.
   - Show lineage from compacted root to source leaf.
   - Make rewind and fork use the same tree-backed target model, with rewind
     applying a narrower validity filter.
   - Decide whether the main transcript renders the compaction summary as a
     compact row or hides it while still including it in provider context.

7. Keep docs in sync.
   - Update architecture, websocket RPC, design decisions, and any UI command
     docs when this proposal is implemented.

## Invariants

- Transcript entries are never deleted by rewind, fork, or compaction.
- `parent_id` is only model-visible context ancestry.
- Compaction provenance is not a parent edge.
- Compaction provenance includes a source session id because forks can copy a
  compacted root into another session.
- A compacted root carries the last source turn id so future turns continue
  turn numbering after compaction.
- The active transcript shown in the main conversation is the active root-to-leaf
  path only.
- Tree view is the place to inspect inactive branches and compacted roots.
- Compaction completion must not silently move `active_leaf_id` if the source
  leaf changed.
- Compaction completion must be durable and atomic with the action row,
  transcript entry, active leaf update, and emitted events.

## Open Questions

- Should compaction summaries be visible in the main transcript as a compact
  row, or only in the tree and provider context?
- Should automatic compaction eventually run during a turn, or only between
  turns?
- If background compaction is allowed later, should stale completions be
  discarded or retained as inactive compacted roots?
- How much recent tail should be summarized into the prompt for scheme 2?
- Do we need a separate typed lineage table, or is `source_leaf_id` inside the
  compaction transcript item enough for now?

## Reviewer Notes

A subagent review agreed that the forest/new-root idea is sound, but identified
the following risks that this proposal now accounts for:

- fresh summary roots need turn-id continuity
- lineage must either be cross-session or rewritten on fork
- compaction completion must be an explicit Postgres compare-and-set
  transaction, not normal runtime output collection
- removing `CompactionState` intentionally retires model-barrier compaction
- the new transcript variant must be threaded through validation, recovery,
  provider rendering, RPC events, and web types
- the rewind picker must become tree-backed if old pre-compaction branches are
  meant to remain reachable from the UI
