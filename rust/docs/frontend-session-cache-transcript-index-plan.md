# Frontend Session Cache and Transcript Index Plan

## Why this exists

The session sync redesign makes Postgres authoritative for queue/session/transcript
state and exposes revision counters. The remaining weakness is the web data layer:
it keeps several partially overlapping selected-session views (`session` active
branch, `session` full tree, and `historyTree` keyed by `lastEventId`) and uses
`history.tree` full bodies for the branch picker. That makes large sessions slow
and gives the frontend multiple places where topology, active leaf, queue, and
transcript bodies can disagree.

This plan moves directly toward the long-term shape while staying simple:
small modular RPCs, one normalized selected-session cache in the web app, compact
transcript topology separate from full entry bodies, and revisions as convergence
signals. The plan is intentionally not a monolithic `session.sync` endpoint and
intentionally does not add generic event patches.

## Goals

- Keep Postgres the durable source of truth and the Rust daemon the authoritative
  interpreter of transcript/queue semantics.
- Give the frontend one selected-session cache that owns head state, queue,
  active-branch bodies, compact tree topology, and sparse fetched bodies.
- Avoid `history.tree` full-body loads in normal UI paths, especially the history
  picker.
- Make branch switching one user-visible round trip: the switch response should
  contain the new head/revisions and active-branch bodies needed to render.
- Keep websocket events thin: events announce what changed and include canonical
  small projections where already available, but the cache reconciler decides
  what to fetch.
- Preserve simple reviewable APIs with clear separation of concerns.
- Implement queued follow-up edit/delete/reorder in the frontend on top of the
  queue mutation RPCs from the previous PR.

## Non-goals

- No IndexedDB or persistent browser transcript cache in this stack.
- No monolithic GraphQL-like `session.sync { want, known }` RPC.
- No generic server-built `SessionPatch` in every event.
- No daemon startup migration for old sessions; the one existing database was
  upgraded manually before merge.
- No removal of backcompat/debug endpoints such as `history.tree` or
  `session.get(include_entries=true, entries_scope="full_tree")`.

## API shape

### Existing `session.get`

`session.get` remains the cold-open/head endpoint.

For normal selected-session rendering the client calls:

```json
{
  "session_id": "session_1",
  "include_entries": true,
  "entries_scope": "active_branch"
}
```

The response stays a `SessionSnapshot`, but every transcript entry now includes
`sequence`. This is additive and lets the browser merge active-branch bodies into
its normalized body cache without guessing append order.

The snapshot already carries:

- `session_revision`
- `queue_revision`
- `transcript_revision`
- `last_event_id`
- canonical `queued_inputs`

### New `transcript.index`

Purpose: fetch compact transcript topology, not full bodies.

Request:

```json
{
  "session_id": "session_1",
  "after_sequence": 0,
  "limit": 1000
}
```

Response:

```json
{
  "session_id": "session_1",
  "active_leaf_id": "entry_9",
  "session_revision": 12,
  "transcript_revision": 7,
  "after_sequence": 0,
  "max_sequence": 42,
  "complete": true,
  "nodes": [
    {
      "id": "entry_1",
      "parent_id": null,
      "sequence": 1,
      "timestamp_ms": 123,
      "item_type": "user_message",
      "turn_id": null,
      "outcome": null,
      "can_switch_to": false,
      "edit_target_leaf_id": null,
      "display_hint": "hello"
    }
  ]
}
```

Notes:

- The endpoint is read-only repeatable-read so the rows and `max_sequence` are a
  consistent slice.
- `sequence` is a pagination cursor, not the freshness token. The freshness token
  is `transcript_revision`; `sequence` only says where to resume when the index
  is still for the same transcript revision.
- The initial implementation can reset and reload the compact index when
  `transcript_revision` changes unexpectedly. That is still cheap because nodes
  are small. Append events can extend it optimistically when they include the new
  node.
- `display_hint` is explicitly best-effort UI text. Correctness must not depend
  on it.
- Backend-computed booleans/targets (`can_switch_to`, `edit_target_leaf_id`) are
  authoritative because turn-boundary semantics belong server-side.

### New `transcript.entries`

Purpose: fetch sparse full bodies by ID for UI flows that need content but do
not need the whole tree.

Request:

```json
{
  "session_id": "session_1",
  "entry_ids": ["entry_1", "entry_7"]
}
```

Response:

```json
{
  "session_id": "session_1",
  "session_revision": 12,
  "transcript_revision": 7,
  "entries": [ ... full transcript entries with sequence ... ]
}
```

Normal uses:

- Restoring the text of a selected historical user message into the composer.
- Opportunistic body hydration for details UI later.

This endpoint takes explicit IDs. It does **not** take a huge `known_body_ids`
array.

### Enhanced `history.switch`

Request adds an optional response-shaping flag:

```json
{
  "session_id": "session_1",
  "leaf_id": "entry_9",
  "expected_active_leaf_id": "entry_42",
  "return_active_branch": true
}
```

Response:

```json
{
  "session_id": "session_1",
  "active_leaf_id": "entry_9",
  "activity": "idle",
  "session_revision": 13,
  "queue_revision": 5,
  "transcript_revision": 7,
  "last_event_id": 88,
  "active_branch_entries": [ ... entries with sequence ... ]
}
```

Rationale:

- The branch is exactly what the UI must render immediately after a switch.
- Returning it avoids the current post-switch `session.get` hot-path refetch.
- This is simpler than negotiating missing body IDs and is still much smaller
  than fetching the full tree.

### Queue mutation RPCs

The prior PR adds:

- `input.update_queued`
- `input.cancel_queued`
- `input.reorder_queued_follow_ups`

The frontend should use the returned canonical `queue` projection to replace the
cached queue. Steering messages are immutable, always above follow-ups, and not
part of reorder requests. Follow-up reorder sends the full ordered follow-up ID
list; no sparse/gapped order numbers are exposed to the client.

## Event model

Events remain thin, revision-bearing hints.

- Queue events include the canonical queue projection from the prior PR. The
  frontend replaces queue state if the event's `queue_revision` is newer.
- `transcript.appended` is enriched additively with:
  - `entry.sequence`
  - `session_revision`, `queue_revision`, `transcript_revision`
  - `active_leaf_id`
- `history.switched` is enriched additively with the same revisions and
  `active_leaf_id`/`activity`.

No generic `patch` field is introduced.

## Frontend selected-session cache

The web app should have one selected-session cache instead of multiple selected
session authorities. TanStack Query can continue to own global/server lists
(projects, session summaries, tools), but selected-session details should be a
single normalized cache object.

Shape:

```ts
interface SelectedSessionCache {
  sessionId: string;
  snapshot: SessionSnapshot | null;        // head, revisions, queue, metadata
  activeBranchEntryIds: string[];          // render order
  entriesById: Map<string, TranscriptEntry>;
  treeNodesById: Map<string, TranscriptTreeNode>;
  treeChildrenByParentId: Map<string | null, string[]>;
  treeOrder: string[];                     // sequence order for picker derivation
  treeTranscriptRevision: number | null;
  treeMaxSequence: number;
  treeComplete: boolean;
}
```

Reducer/capability operations:

- `loadSelectedSession(sessionId)`: hot path for opening a session. Fetch
  `session.get(active_branch)` and normalize. Render as soon as this returns.
- `refreshSelectedSession(sessionId)`: same endpoint, used for fallback
  reconciliation.
- `ensureTreeIndex(sessionId)`: fetch `transcript.index` pages only when the
  picker/export-like topology UI needs topology. If the local compact index is
  complete for the current `transcript_revision`, this is a no-op.
- `ensureEntryBodies(sessionId, ids)`: fetch missing full bodies by explicit ID.
- `applyQueueProjection(sessionId, queue)`: replace queue/head revisions from
  canonical queue projection.
- `applySwitchResult(result)`: replace active branch bodies/head from
  `history.switch(return_active_branch=true)`.
- `applyTranscriptAppended(event)`: if the event has a full entry with sequence
  and extends the current active branch, append it locally; otherwise schedule a
  selected refresh.

Important invariant: Components read from this cache (or selectors derived from
it), not separately from `session(active_branch)`, `session(full_tree)`, and
`historyTree(lastEventId)`.

## UX hot paths and latency

- Cold selected-session open: one RPC (`session.get` with active branch bodies),
  then render. Compact full-tree topology is not fetched in the hot path.
- History picker open: one or more small `transcript.index` page RPCs. For common
  small sessions this is one RPC; for large sessions it streams pages of compact
  nodes. No provider replay/full message bodies are transferred for the picker.
- Branch switch: one RPC (`history.switch(return_active_branch=true)`) and render
  from the returned branch. If restoring a historical user message, first fetch
  that one body via `transcript.entries` only if the body is missing locally.
- Queue edit/delete/reorder: one mutation RPC; replace queue from canonical
  response.
- Background/event reconciliation: events patch when sufficient, otherwise a
  debounced `session.get(active_branch)` refresh.

We do **not** need missing full-tree topology in the hot path for normal chat
rendering. Operations that require topology (`/switch`) call `ensureTreeIndex`
first and display a picker loading state while that capability is fetched.

## Implementation sequence in this PR stack

1. Plan/doc commit.
2. Backend API commit:
   - Add transcript record/node types with `sequence`.
   - Return entries with sequence from selected RPC views.
   - Add `transcript.index` and `transcript.entries` RPCs.
   - Enhance `history.switch` response.
   - Enrich transcript/history events with revisions.
   - Update RPC docs/tests.
3. Frontend cache/API commit:
   - Update TypeScript types and `agentApi`.
   - Add selected-session cache reducer/selectors.
   - Route selected chat rendering through the normalized cache.
   - Move history picker from `history.tree` full bodies to `transcript.index`.
   - Use `transcript.entries` only for missing restore bodies.
   - Use enhanced `history.switch` response instead of post-switch full refetch.
4. Queue UX commit:
   - Add edit/delete/reorder controls for queued follow-ups.
   - Send full follow-up ID ordering.
   - Replace queue from mutation responses/events.
5. Verification/fixup commit if needed.

## Pitfalls to watch and record

- If adding `sequence` to `StoredTranscriptEntry` is too invasive, use an
  RPC/store-specific `TranscriptEntryRecord` and keep session-core types stable.
- The compact index should not compute complex frontend presentation such as
  final picker titles. Keep only small stable facts plus best-effort hints.
- Any event lacking enough data must schedule a refresh, not guess.
- If a stale event arrives with older revision counters, ignore it except for the
  event high-water cursor.
- If current frontend code has unrelated TypeScript errors after rebasing onto
  latest main, fix them only when necessary and note that here.

## Mid-implementation notes

- Branch base was rebased onto latest `origin/main` before this plan. Main added
  provider timeouts, web transcript tool-card polish, Bash tool behavior, system
  prompt updates, Rust Claude model update, Mermaid rendering, and Claude Opus
  4.8 picker support. None changes this architecture; the web files do have
  recent markdown/rendering changes, so frontend edits must preserve those.
- Backend implementation used an RPC/store-specific `TranscriptEntryRecord`
  instead of adding `sequence` to `agent-session::StoredTranscriptEntry`. This
  kept the session-core storage vocabulary stable and avoided touching all
  stored-session tests/constructors. RPC-facing entry records serialize with the
  same fields plus `sequence`.
- `transcript.index.edit_target_leaf_id` is currently nullable rather than an
  incorrect parent shortcut. The frontend can derive the previous boundary from
  compact topology for user-message editing, and `history.switch` still validates
  the final target server-side. This preserves correctness without making the
  compact index do an ancestor walk per row.
- Transcript append events now query the just-inserted row inside the same
  transaction to include `sequence` and `tree_node`. Revision counters are read
  after the commit path bumps them. Compaction/recovery paths were adjusted so
  transcript events are emitted after transcript revisions are bumped.
- The first frontend commit is an additive foundation: TypeScript API/types,
  selected-session cache helpers, and a compact history picker component. App
  wiring and queue mutation controls remain separate commits so each step stays
  reviewable.
- App wiring now removes the selected-session `useQuery` cache authority. The
  app keeps projects/session lists/tools/system prompt in TanStack Query, but
  selected-session rendering, queue state, active branch bodies, compact
  topology, and sparse fetched bodies live in `SelectedSessionCache`.
- `/export` intentionally fetches only `session.get(active_branch)` now. The UI
  exports "the current branch", so full-tree bodies were unnecessary hot-path
  data and were another selected-session cache authority.
- The selected cache opportunistically absorbs canonical queue projections from
  queue mutation responses/events and absorbs `transcript.appended` entries when
  they append to the displayed branch. Side-channel transcript events
  (`turn.started`, `turn.finished`, `assistant.message`) advance the event
  high-water locally when their `entry_id` is already known, avoiding redundant
  selected refreshes after the matching `transcript.appended`.
- Queue edit/delete/reorder controls were added for queued follow-ups only.
  Steering rows remain immutable in the pane: they stay above follow-ups and
  expose only the disabled/non-editable state.
- Review hardening found implicit reducer/caller couplings: a changed-revision
  delta `transcript.index` page, or an overlapping/non-contiguous page, could
  truncate the local compact tree if some future caller skipped the paging-loop
  restart guard. The reducer now tracks the loaded compact-tree prefix and
  refuses non-zero `after_sequence` pages unless they match the loaded
  `transcript_revision` and start exactly at that prefix; callers must restart
  from `after_sequence=0`.
- Added focused selected-session reducer tests for normalization, queue
  revision replacement, tree-index paging/restart behavior, append events, and
  switch-result application.
- Added agent-store Postgres tests for compact index pagination/sparse entry
  sequence exposure, switch responses with revisions and active branch bodies,
  and the bump-before-emit invariant for `transcript.appended`
  `transcript_revision`/`sequence` payloads.
- Extracted a very small `useSelectedSessionStore` host for the selected cache.
  It keeps React state and the synchronous async-flow ref in one place
  (`replace`/`reset`/`update`) so App no longer hand-mirrors
  `setSelectedCache(...)` with `selectedCacheRef.current = ...` at each callsite.
- Post-merge UX pitfall: the first selected-cache wiring still treated every
  known event as a selected-session refresh hint. Busy turns emit many
  action/model/tool/transcript side-channel events, so the header could flicker
  between `refreshing` and settled even though the normalized cache had enough
  data to stay current. The frontend now keeps `session.get(active_branch)` as
  the fallback only for selected-session events whose canonical projection is
  not otherwise available (`session.idle`, recovery/config/history/compaction
  transitions, plus unknown events). Queue projections, append events, and
  activity hints are merged locally, and overlapping selected refreshes are
  coalesced per session.
- Post-merge suspended-tab pitfall: `last_event_id` is only a replay cursor for
  retained websocket event rows. The daemon may clear those rows when a session
  becomes idle, so a later `session.get` can report a smaller `last_event_id`
  than a tab observed before sleeping. The frontend must not interpret that as
  stale canonical state. Selected freshness is driven by revisions plus explicit
  reconciliation: after the page returns to the foreground
  (`visibilitychange`/`focus`/bfcache `pageshow`) the
  app invalidates the session list and fetches the selected active branch once,
  throttled to avoid duplicate browser lifecycle events.
- Compact topology from events is deliberately conservative. If the compact
  tree is already complete, a backend-computed `tree_node` from
  `transcript.appended` may extend it. If the tree is incomplete or stale, the
  frontend leaves recovery to `transcript.index`; this keeps `/switch` correct
  without deriving Rust display/turn-boundary logic in TypeScript.
- The web UI no longer renders local transcript-pending bubbles (`SENDING...`
  or `SYNCING...`). The composer button spinner is the only local indication
  that a submit RPC is in flight. Transcript rows and queued-input rows render
  only from canonical daemon events/RPC projections, which avoids duplicate
  shadow messages after reconnect/foreground reconciliation.
