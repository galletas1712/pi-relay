# Web session switching and metadata-operation performance plan

## Summary

> Implementation note: much of the early client-side work described here has
> been implemented with TanStack Query rather than the bespoke cache sketched in
> Phases 1-4. The remaining high-impact items are now server-side cheap metadata
> paths, active-branch fetching for normal display, incremental transcript
> events, and rendering virtualization/lazy rendering.

The web UI currently treats many small interactions as full session synchronization events. Selecting a session, renaming it, archiving it, and many websocket events can trigger `session.get` with `include_entries: true`, which loads the complete transcript tree from Postgres, serializes it through the daemon, sends it over the websocket, parses it in the browser, rebuilds transcript display structures, and renders every visible transcript node.

This causes two user-visible problems:

1. **Session switch lag:** the sidebar selection changes immediately, but the transcript view keeps showing the previous session until the new full snapshot arrives and renders.
2. **Slow simple operations:** rename/archive are metadata-level operations, but selected-session flows can perform one or more full transcript refreshes before/after the mutation. Server-side archive/configure can also load the full stored transcript just to validate metadata changes.

The goal is to make cheap interactions cheap, make session switching feel immediate, and reserve full transcript loads for operations that actually need the full tree.

## Goals

- Eliminate stale transcript display after selecting a different session.
- Make switching to recently viewed sessions effectively instant.
- Avoid full transcript reloads for metadata-only operations such as rename/archive.
- Avoid duplicate refreshes caused by direct action handlers plus websocket event handlers.
- Reduce normal session-switch payload size by loading only the active branch where possible.
- Incrementally update the selected transcript from events instead of repeatedly full-refreshing while a session runs.
- Keep large transcript rendering responsive through virtualization and lazy rendering.
- Add instrumentation so we can identify whether time is spent in database queries, daemon serialization, websocket transfer, browser JSON parse, derived transcript computation, or React rendering.
- Keep the implementation simple, consistent, and modular: centralize selected-session state derivation, cache updates, event patching, and entry-scope handling instead of spreading special cases across components.

## Non-goals

- Do not remove full history-tree support. `/fork`, `/switch`, export, and history visualization still need full transcript tree data.
- Do not change the durable transcript model in this plan.
- Do not prematurely optimize every transcript component before measuring. Start with clear over-refresh problems.
- Do not introduce a new global state framework or multiple websocket connections unless the smaller fetch/event changes still leave measured responsiveness problems.

---

## Current behavior and bottlenecks

### Session selection

Relevant client flow:

- `Sidebar` calls `onSelectSession(sessionId)`.
- `App.selectSession(sessionId)` updates only `selectedRef.current` and `selectedId`.
- Existing `snapshot` and `entries` remain in state.
- A `useEffect` on `selectedId` calls `refreshSelected(selectedId)`.
- `refreshSelected` calls `api.getSession(sessionId, { includeEntries: true })`.
- Only after that returns does the UI call `setSnapshot(nextSnapshot)` and `setEntries(nextSnapshot.entries ?? [])`.

Consequence: the app can show the previous session transcript after the sidebar has selected another session.

### Stale selected-session state beyond the transcript

The same selected-vs-loaded mismatch can affect more than the visible message list. After `selectedId` changes, current code still derives several controls from `snapshot` and `entries` even if they belong to the previous session:

- header/provider/model values;
- `modelLocked` and model-control disabled state;
- stop button state;
- queued-input composer pane;
- `expectedActiveLeafId` for follow-up sends;
- archived-session resume/unarchive logic;
- inspector contents.

Any implementation must treat `snapshot` and `entries` as usable only when they are known to belong to the selected session. A central derived value should be used across the app:

```ts
const loadedSnapshot =
  snapshot?.session_id === selectedId ? snapshot : null;
const loadedEntries = loadedSnapshot ? entries : [];
```

The header may still use the selected session-list summary while `loadedSnapshot` is null, but action preconditions should not use a stale snapshot.

### Full `session.get(include_entries: true)` cost

Client request:

```ts
session.get({ session_id, include_entries: true })
```

Daemon work:

- acquire/recover session driver;
- load session snapshot;
- load pending actions;
- load last event id;
- check active queued inputs;
- load queued inputs;
- if `include_entries`, load full history tree.

Postgres transcript query:

```sql
select id, parent_id, timestamp_ms, item, provider_replay
from transcript_entries
where session_id=$1
order by sequence
```

Browser work after the response:

- parse a potentially large websocket JSON message;
- set React state for snapshot and entries;
- compute active branch with `branchEntriesFor`;
- build tool indexes with `indexToolEntries`;
- build turn views with `buildTurnViews`;
- derive transcript display nodes with `deriveTranscriptDisplayNodes`;
- parse provider replay JSON and tool args JSON;
- build tool/edit previews;
- render all display nodes;
- render markdown for assistant text;
- restore scroll position.

### Rename flow

Current selected-session rename can do:

1. `session.rename` RPC;
2. `loadSessions()`;
3. `refreshSelected(renameSessionId)` if selected;
4. event-driven `loadSessions()` from `session.configured`;
5. event-driven `scheduleSelectedRefresh()` because every selected-session event currently schedules a full refresh.

The rename mutation itself is a simple metadata update, but the UI performs transcript-level synchronization around it.

### Archive/unarchive flow

Current selected-session archive can do:

1. pre-refresh selected session with `refreshSelected(sessionId)`;
2. `session.configure` RPC;
3. `loadSessions()`;
4. post-refresh selected session with `refreshSelected(sessionId)`;
5. event-driven `loadSessions()`;
6. event-driven scheduled selected refresh.

Server-side, `session.configure` currently treats metadata changes as source mutations:

```rust
if model_changed || metadata_changed {
    driver.ensure_idle_for_source_mutation().await?;
}
```

`ensure_idle_for_source_mutation()` calls `recover_if_needed()`, which can load the full stored session/transcript. This is unnecessary for a metadata-only archive flag change.

### Request/event duplication and websocket serialization

The websocket server handles incoming requests and outgoing subscribed events in a single loop. A large request/response, especially a full `session.get(include_entries: true)`, can delay subsequent interactions on that socket. On the client side, action handlers and event handlers often both refresh the same data.

---

## Design principles

1. **Separate selection state from loaded transcript state.** The selected session ID can change before transcript data is ready. The UI should represent this explicitly.
2. **Patch metadata locally.** Rename/archive/configure should update session list and selected snapshot metadata without loading transcript entries.
3. **Fetch the smallest useful data.** Normal session display needs the active branch, not the full tree. History operations can request the full tree lazily.
4. **Use events incrementally.** Transcript append events should append entries; metadata events should patch metadata; activity events should patch activity.
5. **Make expensive rendering proportional to viewport.** Large transcripts should not render every row on every switch.
6. **Measure before and after.** Each phase should include instrumentation or acceptance metrics.
7. **Do not derive UI/action state from stale snapshots.** A snapshot is selected-session state only when `snapshot.session_id === selectedId`.
8. **Avoid replacing whole mutable blobs from stale client state.** Metadata patches are safer than full metadata replacement once clients stop pre-refreshing.
9. **Prefer one small module per concern.** Keep state transitions in helpers instead of ad hoc `setSnapshot`/`setEntries`/`setSessions` chains in action handlers.
10. **Prefer one consistent patch path.** RPC responses, websocket events, and optimistic updates should call the same cache/session-list/snapshot patch helpers.
11. **Prefer protocol fields over client inference.** If the UI needs `has_transcript_entries`, entry scope, or complete metadata, return it explicitly instead of inferring it from partial entries.
12. **Keep compatibility fallbacks narrow and visible.** Older/missing event payloads should mark cache stale or do a targeted refresh, not reintroduce broad refresh-on-every-event behavior.

### Modularity targets

Implement the client changes around a few focused helpers/modules:

- `selectedSessionState`: derives `loadedSnapshot`, `loadedEntries`, `transcriptLoading`, `activeProvider`, model lock/disabled state, queued inputs, and safe header summary from `selectedId`, `snapshot`, `entries`, and the session list.
- `sessionCache`: owns cache reads/writes/patches, entry scope, stale flags, LRU/size limits, and event-id bookkeeping.
- `sessionPatches`: applies metadata/provider/activity/active-leaf/queued-input/transcript-entry patches to session list, selected snapshot, and cache through one path.
- `eventReducer`: maps websocket event frames to typed patch operations or explicit fallback refresh reasons.
- `sessionFetch`: wraps `session.get` calls, request generations, stale response guards, perf logging, and cache writes.

The goal is that components render derived state and handlers dispatch small operations; they should not need to know cache internals or event payload compatibility details.

---

## Phase 0: Instrumentation and baseline

Add low-risk timing and size measurements before making larger changes.

### Client instrumentation

Add a small performance helper in `packages/web/src/perf.ts` or inline behind a dev flag.

Capture:

- session select time;
- `session.get` request start/end;
- websocket message byte size and JSON parse time in `rpc.ts`;
- response entry count;
- approximate response size;
- transcript derivation timings;
- first render/load state transitions;
- rename/archive timings and whether full refresh happened.

Suggested marks:

```ts
performance.mark("session-select");
performance.mark("session-get-start");
performance.mark("session-get-end");
performance.mark("rpc-message-received");
performance.mark("rpc-json-parse-end");
performance.mark("transcript-derive-start");
performance.mark("transcript-derive-end");
performance.mark("transcript-render-committed");
```

Useful logged fields:

```ts
{
  sessionId,
  entries: nextSnapshot.entries?.length ?? 0,
  approxBytes: JSON.stringify(nextSnapshot).length,
  rpcMs,
  deriveMs,
  cacheHit,
  source: "select" | "event" | "rename" | "archive"
}
```

Avoid excessive logging in production. Gate behind `import.meta.env.DEV` or a localStorage flag such as `piRelayPerf=1`.

### Daemon instrumentation

Add timings around `session.get`:

- driver acquire;
- `recover_if_needed`;
- `session_snapshot`;
- `history_tree`;
- response serialization size if practical.

Add timings around `session.configure` and `session.rename`:

- driver acquire;
- idle validation;
- metadata update query;
- event insert/publish;
- `clear_event_buffer_if_idle`.

### Acceptance criteria

- We can answer whether slow cases are dominated by server DB/recovery, websocket payload/serialization, browser JSON parse, transcript derivation, or render.
- Logs show when rename/archive accidentally trigger full transcript refresh.

---

## Phase 1: Immediate UX fix for session switching

### Problem

The transcript view can display old entries after `selectedId` changes.

### Plan

Track which session the current `entries` belong to and render an explicit loading state when selected session and loaded entries do not match.

Also make all selected-session UI use a non-stale snapshot abstraction:

```ts
const loadedSnapshot =
  snapshot?.session_id === selectedId ? snapshot : null;
const loadedEntries = loadedSnapshot ? entries : [];
const transcriptLoading = !!selectedId && !loadedSnapshot;
```

Use `loadedSnapshot` for action preconditions, stop button state, queued-input pane, inspector content, expected active leaf, model lock state, and model-control disabled state. Use the selected session-list row only for safe summary display while the snapshot is loading.

Current `ChatPane` already passes:

```tsx
entriesSessionId={snapshot?.session_id ?? null}
```

`MessageList` computes:

```ts
const entriesBelongToSelectedSession = !hasSession || !sessionId || entriesSessionId === sessionId;
```

But this guard is only used for scroll logic. Extend it to UI rendering.

### Minimal fix: clear data immediately on select

Lowest-risk fix if implemented before the cache:

```tsx
onSelectSession={(sessionId) => {
  selectSession(sessionId);
  setSnapshot(null);
  setEntries([]);
  closeSidebarIfOverlay();
}}
```

Pros:

- very simple;
- stale transcript disappears immediately.

Cons:

- switching back always shows blank/loading even if data was just loaded;
- unnecessary visual churn.

### Preferred implementation: explicit loading state plus cache

Once the cache exists, avoid clearing cached data unnecessarily. Add props to `MessageList`:

```ts
loadingSession: boolean;
```

Compute in `ChatPane` or `App`:

```ts
const transcriptLoading = !!selectedId && snapshot?.session_id !== selectedId;
```

When loading, render a centered state such as:

```tsx
<div className="message-scroll">
  <div className="empty-state">
    <Loader2 className="spin" />
    <span>Loading session...</span>
  </div>
</div>
```

Also ensure the header can still show the selected session title from the session list while the snapshot is loading.

### Avoid stale derivation cost

Do not merely hide stale entries after deriving transcript display structures from them. In `MessageList`, if entries do not belong to the selected session, derive from an empty list:

```ts
const entriesBelongToSelectedSession =
  !hasSession || !sessionId || entriesSessionId === sessionId;

const effectiveEntries = entriesBelongToSelectedSession ? entries : [];

const visibleEntries = useMemo(
  () => (hasSession ? branchEntriesFor(effectiveEntries, activeLeafId) : effectiveEntries),
  [activeLeafId, effectiveEntries, hasSession]
);
```

Then render the loading state. This avoids doing the full `branchEntriesFor` / tool indexing / turn view / display-node derivation path for the previous session while the new selected session is loading.

### Acceptance criteria

- After clicking a different session, the previous transcript is never shown as if it belongs to the newly selected session.
- After clicking a different session, stale previous-session entries are not used for expensive transcript derivation.
- The sidebar selected row updates immediately.
- The chat header updates immediately from session-list summary if snapshot is not loaded yet.
- Stop/model/composer/inspector state does not temporarily reflect the previous selected session.

---

## Phase 2: In-memory session snapshot cache

### Problem

Switching back to a recently viewed session performs a full reload and render path.

### Plan

Cache loaded snapshots and entries by session ID in memory. On selection:

1. update `selectedId` immediately;
2. if cached, show cached snapshot/entries immediately;
3. still refresh in the background if the cache is stale or events indicate missing updates;
4. if not cached, show loading state and fetch.

### Client state shape

Keep cache entries explicit and avoid storing entries only as an optional field inside `SessionSnapshot`. This keeps metadata/snapshot patches independent from transcript-entry patches and makes entry scope visible.

```ts
type EntryScope = "active_branch" | "full_tree";

type CachedSnapshot = Omit<SessionSnapshot, "entries">;

type CachedSession = {
  snapshot: CachedSnapshot;
  entries: TranscriptEntry[];
  entryScope: EntryScope;
  cachedAt: number;
  lastEventId: number;
  stale: boolean;
};

const sessionCacheRef = useRef(new Map<string, CachedSession>());
```

A full-tree cache can satisfy active-branch display. An active-branch cache cannot satisfy history picker/export if alternate branches are needed.

Cache on successful refresh:

```ts
const { entries = [], ...snapshotWithoutEntries } = nextSnapshot;
sessionCacheRef.current.set(sessionId, {
  snapshot: snapshotWithoutEntries,
  entries,
  entryScope,
  cachedAt: Date.now(),
  lastEventId: nextSnapshot.last_event_id,
  stale: false
});
```

On select:

```ts
const cached = sessionCacheRef.current.get(sessionId);
if (cached && !cached.stale) {
  setSnapshot(cached.snapshot);
  setEntries(cached.entries);
} else {
  setSnapshot(null);
  setEntries([]);
}
```

Use helpers rather than direct mutations:

```ts
getCachedSession(sessionId)
writeCachedSession(sessionId, snapshot, entries, entryScope)
patchCachedSession(sessionId, patcher)
markCachedSessionStale(sessionId, reason)
deleteCachedSession(sessionId)
```

The same patch helpers should update:

1. `sessions`;
2. selected `snapshot` if it belongs to the patched session;
3. `sessionCacheRef`.

This keeps RPC-result patching, optimistic patching, and event patching consistent.

### Cache invalidation/update

- On `transcript.appended`, append/update cached entries for that session if possible.
- On `session.configured`, patch cached metadata/provider if included in event payload.
- On `history.rewound`, patch `active_leaf_id` if included; otherwise mark cache stale.
- On `history.compacted`, mark full transcript cache stale unless event includes enough entry data.
- On delete, remove cache entry.
- On project switch, cache may remain, but selected snapshot/entries should be cleared and in-flight list/session requests from the prior project should be ignored.
- On websocket close/reconnect, mark caches stale unless replay from `lastEventId` succeeds. Idle sessions clear event rows, so replay is best-effort rather than a complete synchronization guarantee.
- Cap cache by approximate size/entry count as well as count. A simple first cap is 10-20 sessions plus a maximum total cached entry count/byte estimate.

### Acceptance criteria

- Switching back to a cached session shows transcript immediately.
- Background refresh does not visibly revert to a loading state unless cache is known invalid.
- Deleted sessions are removed from cache.
- Cache entry scope is respected: history/export never accidentally use an active-branch-only cache as if it were a full tree.

---

## Phase 3: Make rename/archive metadata-only on the client

### Problem

Rename/archive perform full selected transcript refreshes even though they only need metadata/session-list updates.

### Plan

Patch local state optimistically or immediately after RPC success. Do not call `refreshSelected` for metadata-only operations.

Use one shared patch helper for all metadata/provider/config UI changes. Rename, archive/unarchive, provider configuration, event handling, and optimistic updates should all call the same helper so snapshot/list/cache behavior stays consistent.

### Rename changes

Current behavior:

```ts
await api.renameSession(renameSessionId, title);
await Promise.all([
  loadSessions(),
  renameSessionId === selectedRef.current ? refreshSelected(renameSessionId) : Promise.resolve(null)
]);
```

Replace with:

1. call `api.renameSession`;
2. patch `sessions`, selected `snapshot`, and cache through the shared patch helper;
3. close dialog;
4. optionally schedule a debounced list reconciliation, but no full selected refresh.

Helper:

```ts
function patchSessionMetadata(
  sessionId: string,
  patch: Record<string, unknown>,
  removeKeys: string[] = []
) {
  setSessions((current) => current.map((session) => {
    if (session.session_id !== sessionId) return session;
    const metadata = { ...session.metadata, ...patch };
    for (const key of removeKeys) delete metadata[key];
    return { ...session, metadata };
  }));

  setSnapshot((current) => {
    if (!current || current.session_id !== sessionId) return current;
    const metadata = { ...current.metadata, ...patch };
    for (const key of removeKeys) delete metadata[key];
    return { ...current, metadata };
  });

  patchSessionCache(sessionId, (cached) => {
    const metadata = { ...cached.snapshot.metadata, ...patch };
    for (const key of removeKeys) delete metadata[key];
    return { ...cached.snapshot, metadata };
  });
}
```

Rename patch:

```ts
patchSessionMetadata(renameSessionId, { title });
```

### Archive/unarchive changes

Current selected archive does pre- and post-`refreshSelected`.

Replace with:

1. use `session.activity` or `loadedSnapshot.activity` to guard obvious busy sessions;
2. call a metadata patch RPC when available, or `api.configureSession` with merged metadata as a compatibility bridge;
3. patch local metadata after success;
4. no pre-refresh and no post-refresh full transcript.

If we want optimistic UI, patch before the RPC and revert on error. Safer first implementation: patch after RPC success.

Archive patch:

```ts
if (archived) patchSessionMetadata(sessionId, { archived: true });
else patchSessionMetadata(sessionId, {}, ["archived"]);
```

### Avoid metadata clobbering

`session.configure` currently replaces the full metadata object. Removing the pre-refresh makes it possible for archive/unarchive to overwrite concurrent metadata changes if the client builds the new metadata from a stale session-list row.

Preferred protocol shape:

```json
{
  "session_id": "...",
  "metadata_patch": { "archived": true },
  "metadata_remove": []
}
```

Unarchive:

```json
{
  "session_id": "...",
  "metadata_patch": {},
  "metadata_remove": ["archived"]
}
```

Keep full metadata replacement only as a compatibility path or for settings screens that intentionally edit the whole metadata object.

### Provider/configure changes

Changing provider-adjacent settings, such as reasoning effort, currently calls `configureProvider` and then performs `loadSessions()` plus `refreshSelected(sessionId)`. Treat this as a metadata/config patch too:

1. call `api.configureSession({ sessionId, provider })`;
2. patch provider in sessions, selected snapshot, and cache from the response/event;
3. do not full-refresh transcript.

Provider kind/model changes still require strict server validation and should use `has_transcript_entries` rather than active-branch entry count for the lock check.

### Error behavior

If the daemon rejects because the session is busy, show the existing error notice and leave local state unchanged.

### Acceptance criteria

- Rename selected session triggers no `session.get(include_entries: true)`.
- Archive/unarchive selected session triggers no `session.get(include_entries: true)`.
- Reasoning-effort/provider-adjacent configure changes trigger no selected full transcript refresh.
- Session list and header update after the RPC succeeds.
- Errors leave UI state consistent.
- Archive/unarchive does not overwrite unrelated metadata keys modified concurrently.

---

## Phase 4: Fix event handling and refresh coalescing

### Problem

The selected-session event handler schedules a full refresh for every event, including metadata events. Separately, direct handlers call `loadSessions`, causing duplicate work.

Current pattern:

```ts
if (event.session_id === selectedRef.current) {
  scheduleSelectedRefresh(event.session_id);
}
if (isSessionListRefreshEvent(event.event)) {
  void loadSessions();
}
```

### Plan

Replace broad refresh behavior with event-specific patching. Keep this modular: an `eventReducer` should convert each `EventFrame` into one or more patch operations or an explicit fallback reason. The application should then apply those patch operations through the same helper path used by RPC responses.

Example patch operation shape:

```ts
type SessionPatchOperation =
  | { type: "metadata"; sessionId: string; patch: Record<string, unknown>; remove?: string[] }
  | { type: "provider"; sessionId: string; provider: ProviderConfig }
  | { type: "activity"; sessionId: string; activity: Activity }
  | { type: "active_leaf"; sessionId: string; activeLeafId: string | null }
  | { type: "queued_inputs"; sessionId: string; patch: QueuedInputPatch }
  | { type: "transcript_entry"; sessionId: string; entry: TranscriptEntry; activeLeafId?: string | null }
  | { type: "mark_stale"; sessionId: string; reason: string }
  | { type: "refresh_list"; reason: string };
```

### Event categories

#### Metadata/session summary events

Events:

- `session.configured`
- `session.created`
- `history.forked`

Actions:

- patch metadata/provider/activity if payload contains enough data;
- otherwise do a debounced `loadSessions`; do not full-refresh transcript.
- update both `sessionsRef` and state through the same list patch helper so later event filtering uses the latest project/session metadata.
- `session.created` and `history.forked` usually do not have enough data to insert a full session-list row; treat them as list-refresh events unless the payload is expanded to include a full summary.

#### Activity events

Events:

- `input.queued`
- `input.consumed`
- `input.promoted`
- `input.accepted`
- `action.requested`
- `model.requested`
- `tool.requested`
- `tool.started`
- `session.idle`
- `model.completed`
- `model.error`
- `tool.completed`
- `tool.error`
- `compaction.completed`
- `compaction.error`
- `session.work_cancelled`
- `session.recovered`

Actions:

- patch session activity in list;
- patch pending/queued state if event payload has enough data;
- debounced `loadSessions` only for reconciliation;
- selected transcript refresh only when event implies missing transcript entries and no incremental entry payload is available.

Activity rules should be conservative:

- `input.queued` => `queued`;
- `input.consumed`, `input.accepted`, `action.requested`, `model.requested`, `tool.requested`, `tool.started`, `compaction.requested` => `running`;
- `session.idle` => `idle`;
- `model.completed`, `model.error`, `tool.completed`, `tool.error`, `compaction.completed`, and `compaction.error` should not by themselves set a session to idle, because another action may already have been created in the same turn. Wait for `session.idle` or reconcile with a lightweight snapshot/list refresh.

Completion/error events should include `action_row_id` where possible. Current `model.completed`/`tool.completed` payloads can be keyed only by `action_id`, while UI pending actions are keyed by `action_row_id`; without row IDs the client either needs an action map from earlier events or a lightweight snapshot refresh.

Queue events should either include a complete `QueuedInput` record (`input_id`, `priority`, `status`, `content`, `client_input_id`, `created_at`, `promoted_at`) or be treated as hints that schedule a lightweight selected snapshot/list reconciliation.

#### Transcript events

Events already exist:

- `transcript.appended`
- `turn.started`
- `turn.finished`
- `assistant.message`

`transcript.appended` currently includes `entry_id` and `item`, but it does **not** include enough information to append a full `TranscriptEntry` because it lacks at least `parent_id`, `timestamp_ms`, and `provider_replay`.

Plan:

- extend `transcript.appended` payload to include full entry data; or
- add a new event `transcript.entry_appended` with full entry data; or
- add a `history.entries_since` endpoint to fetch only missing entries after the last known entry/sequence.

Preferred: extend/add event with full entry because the store already has the inserted entry at persistence time.

Required payload:

```json
{
  "entry": {
    "id": "...",
    "parent_id": "...",
    "timestamp_ms": 123,
    "item": { "type": "..." },
    "provider_replay": []
  },
  "active_leaf_id": "..."
}
```

Client action:

```ts
appendEntryToSession(sessionId, entry, activeLeafId)
```

Update:

- selected `entries` if session is selected;
- selected `snapshot.active_leaf_id` if provided;
- cache entry for that session;
- `lastEventIds`.

If append events are emitted in batches, avoid applying an `active_leaf_id` that points to an entry the client has not received yet. Either:

- set active leaf to each appended `entry.id` while events arrive, then apply final active leaf after it exists locally;
- include a batch-final event that updates active leaf after all entries;
- or have the client defer `active_leaf_id` application until the referenced entry exists.

### Debounce list refresh

Replace immediate `loadSessions()` calls from event handler with a coalesced scheduler:

```ts
const sessionListRefreshTimer = useRef<number | null>(null);

const scheduleSessionListRefresh = useCallback((delayMs = 250) => {
  if (sessionListRefreshTimer.current !== null) return;
  sessionListRefreshTimer.current = window.setTimeout(() => {
    sessionListRefreshTimer.current = null;
    void loadSessions().catch(() => undefined);
  }, delayMs);
}, [loadSessions]);
```

### Acceptance criteria

- `session.configured` does not trigger selected full transcript refresh.
- Bursts of events trigger at most one list refresh per debounce interval.
- Running sessions update transcript by appended entries, not repeated full snapshots, once full entry events exist.
- Full selected refresh remains available as a fallback for unknown or cache-invalidating events.

---

## Phase 5: Daemon metadata-only operation optimization

### Problem

`session.configure` can load/recover the full stored transcript for metadata-only changes. Model-lock validation also loads the full stored session just to check whether entries exist.

### Plan

Split validation paths:

- model/source-changing operations require strict checks;
- metadata-only operations use cheap idle checks;
- model-lock check uses an existence query, not full transcript load.
- full metadata replacement should be avoided for small UI mutations; add a patch-style metadata update path so archive/unarchive cannot clobber concurrent metadata changes.

### Add cheap store helpers

In `agent-store`:

```rust
pub async fn has_transcript_entries(&self, session_id: &str) -> Result<bool> {
    sqlx::query_scalar(
        "select exists(select 1 from transcript_entries where session_id=$1)"
    )
    .bind(session_id)
    .fetch_one(&self.pool)
    .await
}
```

Add a cheap activity/idle check that does not recover/load transcript:

```rust
pub async fn is_session_idle_cheap(&self, session_id: &str) -> Result<bool> {
    Ok(!self.has_unfinished_actions(session_id).await?
       && !self.has_queued_inputs(session_id).await?)
}
```

In daemon runtime, add a method that checks:

- active runtime map does not contain session; or active runtime is idle if that can be represented cheaply;
- no unfinished actions;
- no active queued inputs;
- no transcript recovery.

For metadata-only operations, this avoids `recover_if_needed()`.

Also add cheap summary helpers that avoid transcript loads:

```rust
pub async fn active_leaf_is_turn_boundary(&self, session_id: &str) -> Result<bool> {
    // true for null active leaf, turn_finished, or compaction_summary
}
```

This supports a cheap recovery precheck for reads/subscriptions.

### Change `session_configure`

Current logic:

```rust
if model_changed || metadata_changed {
    driver.ensure_idle_for_source_mutation().await?;
}
if model_changed {
    let stored = state.repo.load_stored_session(&session_id).await?;
    if !stored.entries.is_empty() { ... }
}
```

Target logic:

```rust
if model_changed {
    driver.ensure_idle_for_source_mutation().await?;
    if state.repo.has_transcript_entries(&session_id).await? {
        return Err(provider_locked);
    }
} else if metadata_changed {
    driver.ensure_idle_for_metadata_mutation().await?;
}
```

Depending on desired semantics, archive/unarchive may require idle but should not require transcript recovery. Rename may not require idle at all unless we intentionally want all metadata edits to be idle-only. Current UI allows rename regardless of activity; keep that behavior unless product requirements say otherwise.

### Add metadata patch RPC/store path

Preferred daemon/store operation:

```rust
pub async fn patch_session_metadata(
    &self,
    session_id: &str,
    patch: Value,
    remove: &[String],
) -> Result<Vec<EventFrame>>
```

SQL can apply a JSONB merge plus removals in one update. The resulting event/response should include the full resulting metadata for simple clients, plus the patch for compatibility/debugging if useful.

Use this path for rename and archive/unarchive. Keep full `session.configure` for provider changes and intentional whole-metadata replacement.

### Change event payloads for configure/rename

`session.rename` currently emits `session.configured` with only title:

```json
{ "title": "..." }
```

`session.configure` emits only provider:

```json
{ "provider": ... }
```

Change to include enough state for clients to patch without refetch:

```json
{
  "session_id": "...",
  "provider": { ... },
  "metadata": { ... },
  "activity": "idle"
}
```

Or use specific patch payloads:

```json
{
  "metadata_patch": { "title": "..." },
  "metadata_remove": ["archived"],
  "provider": null,
  "activity": "idle"
}
```

For reads/events, full resulting metadata is simpler and less error-prone. For writes, small UI metadata mutations should use patch payloads at the request boundary and include full resulting metadata in the response/event. That keeps writes safe and reads simple.

### Change RPC responses

Return enough data from `session.rename` and `session.configure` to patch UI:

`session.rename` response:

```json
{
  "session_id": "...",
  "title": "...",
  "metadata": { ... },
  "activity": "idle"
}
```

`session.configure` response:

```json
{
  "session_id": "...",
  "provider": { ... },
  "metadata": { ... },
  "activity": "idle"
}
```

### Acceptance criteria

- Metadata-only configure does not call `load_stored_session` or `history_tree`.
- Model-change validation does not load all entries just to check whether entries exist.
- Rename/configure responses and events are sufficient for client-side patching.
- Archive/unarchive use metadata patch semantics or another server-side merge path and do not overwrite unrelated metadata keys.

---

## Phase 6: Fetch less data for normal session display

### Problem

Normal session display currently fetches the full transcript tree even though `MessageList` renders only the active branch:

```ts
branchEntriesFor(entries, activeLeafId)
```

Even with an active-branch query, `session.get` currently calls `recover_if_needed()` first. `recover_if_needed()` can load the full stored transcript to check whether the session needs tail repair. That would erase much of the benefit of active-branch fetches unless recovery is also made cheap.

### Plan

Add an active-branch-only fetch path. Keep full tree fetch for history operations.

### API options

#### Option A: extend `session.get`

```json
{
  "session_id": "...",
  "include_entries": true,
  "entries_scope": "active_branch"
}
```

Scopes:

- `none` or omitted: no entries;
- `active_branch`: only entries reachable from active leaf;
- `full_tree`: current behavior.

#### Option B: add endpoint

```ts
session.getActiveBranch(sessionId)
```

or

```ts
history.active_branch(sessionId)
```

Option A keeps session snapshot and entries in one response.

### Store implementation

Possible implementation strategies:

1. Load full tree and compute branch server-side. Easy but does not reduce DB work; only reduces websocket/browser cost.
2. Recursive CTE from active leaf to root. Better.

Example recursive query:

```sql
with recursive branch as (
  select t.*
  from transcript_entries t
  join sessions s on s.id = t.session_id and s.active_leaf_id = t.id
  where t.session_id = $1

  union all

  select parent.*
  from transcript_entries parent
  join branch child
    on parent.session_id = child.session_id
   and parent.id = child.parent_id
)
select id, parent_id, timestamp_ms, item, provider_replay
from branch
order by sequence;
```

Handle `active_leaf_id is null` by returning empty entries.

### Recovery/read optimization

Before calling a full recovery path on read-only operations, do a cheap precheck:

1. if an active runtime exists, no stored recovery is needed;
2. reset abandoned consuming inputs if necessary;
3. query only the active leaf item and unfinished-action/queue state;
4. if the active leaf is `null`, `turn_finished`, or `compaction_summary`, no full transcript recovery is needed;
5. only load the full stored session or active branch for recovery when an open tail actually needs repair.

Also avoid calling full recovery from broad event subscriptions. The web client subscribes to many visible sessions, so `events.subscribe` should not full-load transcripts merely to attach to event streams.

### Client usage

- Session selection and normal refresh use `active_branch`.
- `/fork`, `/switch`, export, history picker use `full_tree`, loaded lazily at dialog open.
- Cache should record whether entries are `active_branch` or `full_tree`.
- Model locking should not infer from active-branch entries. Include an explicit `has_transcript_entries` or `transcript_entry_count` in snapshots/list summaries and use that for provider kind/model locking.

Cache shape:

```ts
type EntryScope = "active_branch" | "full_tree";

type CachedSession = {
  snapshot: Omit<SessionSnapshot, "entries">;
  entries: TranscriptEntry[];
  entryScope: EntryScope;
  cachedAt: number;
  lastEventId: number;
  stale: boolean;
};
```

A full-tree cache can satisfy active-branch display. An active-branch cache cannot satisfy history picker/export if alternate branches are needed.

### Acceptance criteria

- Normal session switch payload excludes inactive branches.
- History picker/export still show complete data after explicitly requesting full tree.
- Large forked sessions switch faster even before virtualization.
- Active-branch `session.get` does not internally full-load the transcript unless recovery is actually required.
- `events.subscribe` for a session list does not trigger transcript recovery/full transcript load for every subscribed session.
- Rewound-to-root sessions with existing transcript entries still show provider/model controls as locked via `has_transcript_entries`.

---

## Phase 7: Incremental transcript updates for running sessions

### Problem

While a selected session is running, event handling currently schedules full selected refreshes. This repeatedly reloads all entries.

### Plan

Use transcript append events to append entries locally and update active leaf. Full refresh becomes fallback/reconciliation only.

### Required server change

Emit full appended `TranscriptEntry`, not just `entry_id` and `item`.

Current event insertion only has `entry_id` and `item` in `insert_transcript_item_events_tx`. Update persistence flow to pass the full entry or look up the necessary fields when emitting.

Preferred event payload:

```json
{
  "entry": {
    "id": "...",
    "parent_id": "...",
    "timestamp_ms": 123,
    "item": { "type": "assistant_message", "items": [] },
    "provider_replay": []
  },
  "active_leaf_id": "..."
}
```

### Client append logic

Add helper:

```ts
function upsertTranscriptEntry(entries: TranscriptEntry[], entry: TranscriptEntry): TranscriptEntry[] {
  if (entries.some((candidate) => candidate.id === entry.id)) return entries;
  return [...entries, entry];
}
```

Events currently stream in durable sequence order for each websocket subscription, so the common path can append. If a future `entries_since` endpoint or batch replay can return out-of-order entries, keep ordering policy inside this helper rather than scattering sorts across callers.

Patch selected state:

```ts
setEntries((current) =>
  currentEntriesSessionId === sessionId ? upsertTranscriptEntry(current, entry) : current
);
setSnapshot((current) => current?.session_id === sessionId
  ? { ...current, active_leaf_id: activeLeafId ?? entry.id }
  : current
);
```

Patch cache similarly.

In practice, selected state and cache should be patched by the same `applySessionPatch({ type: "transcript_entry", ... })` helper rather than separate call sites.

### Fallbacks

Still perform full refresh when:

- client detects missing parent for appended entry;
- client receives an active leaf that is not present locally after processing available appended entries;
- event stream lag/reconnect requires replay beyond cache confidence;
- history rewound/compacted/forked changes branch structure;
- unknown event type affects transcript;
- append event lacks full entry due to older daemon version.

### Acceptance criteria

- Running selected sessions update transcript without full `session.get(include_entries: true)` after every model/tool event.
- Full refresh count during a normal turn drops to near zero, except fallback/reconciliation.

---

## Phase 8: Transcript rendering performance

### Problem

The UI renders every display node and parses/renders expensive content eagerly.

### Plan A: Virtualize the transcript

Use a variable-height virtualization library or custom scroller.

Recommended options:

- `react-virtuoso`: easiest variable-height chat-style list, good follow-output behavior.
- `@tanstack/react-virtual`: lower-level, more control.

Requirements:

- preserve per-session scroll positions;
- support sticky-to-bottom behavior while running;
- support variable-height markdown/tool rows;
- handle rows expanding/collapsing;
- keep accessibility reasonable.

Implementation outline:

- Convert `displayNodes` to virtualized items.
- Use node key as item key.
- Replace manual `scrollRef` bottom logic with virtualizer's follow-output APIs where available.
- Keep existing scroll position cache but adapt it to virtualizer offsets.

### Plan B: Lazy-render expensive content

Tool groups:

- collapsed groups should not parse/render full tool output bodies;
- defer `JSON.stringify(input, null, 2)` until expanded;
- defer diff row generation until expanded or use memoized preview summaries;
- cap preview line counts.

Assistant markdown:

- memoize markdown blocks by `{entry.id, text}`;
- consider collapsed/preview mode for very long assistant text if necessary;
- avoid unnecessary re-creation of plugin arrays if profiling shows impact.

Provider replay:

- cache parsed provider replay by entry ID and replay reference/hash;
- avoid calling `JSON.parse` repeatedly for unchanged entries.

Tool args:

- cache parsed args by tool call ID and args string;
- avoid building full edit previews unless the row is expanded or summary requires it.

### Plan C: History picker derivation

Full-tree history operations can still be expensive after normal session display switches to active-branch loading. `historyForkOptions` and `historySwitchOptions` repeatedly compute branch paths and turn views. Keep this separate from message-list optimization:

- precompute entry maps, parent paths, and turn views once per full-tree load;
- avoid calling `buildTurnViews` inside per-entry loops;
- virtualize the history picker if large full trees remain slow;
- keep the full-tree load and derived picker state scoped to the dialog rather than replacing the normal active-branch cache.

### Acceptance criteria

- Large sessions remain scrollable and responsive.
- Switching to a large cached session does not block the UI for multiple seconds.
- Collapsed tool groups are cheap to render.
- Full-tree history picker/export work remains isolated from normal session display state.

---

## Phase 9: Request scheduling and websocket responsiveness

### Problem

Large websocket responses can delay smaller actions on the same connection.

### Plan

Start with reducing large responses. If responsiveness still suffers, add request scheduling/cancellation semantics.

### Client-side stale request handling

`refreshSelected` already drops stale responses if `selectedRef.current !== sessionId`. Keep this behavior.

Enhance by tracking request generations. This should be part of the first cache/loading PR, not a late optional cleanup, because selecting A → B → A can otherwise allow the first A response to apply after a newer A request has started.

```ts
const selectedRequestGeneration = useRef(0);
```

On selection, increment generation. On response, ignore if generation changed.

This prevents stale responses from applying, though it does not cancel daemon work.

Also add generation guards for session-list loads:

```ts
const sessionListGeneration = useRef(0);
```

When the project changes or a new `loadSessions(projectId)` starts, capture the generation and project ID. Apply results only if both still match. This prevents an old project's delayed list response from replacing the current project's sessions.

Wrap this in a small `sessionFetch` helper so request generation, perf logging, cache writes, and stale-response checks are not duplicated across selection, reconnect, and background refresh paths.

### Abort/cancel support, optional

Longer term, add request cancellation or separate connections:

- one websocket for control/metadata requests;
- one websocket for large transcript fetches/events;
- or request IDs with cancellation messages.

This is lower priority if active-branch fetch and incremental events solve most load.

### Acceptance criteria

- Rapidly clicking sessions does not apply stale transcripts.
- Small metadata operations are not delayed behind repeated full transcript refreshes in normal usage.

---

## Phase 10: Tests

### Client unit tests

Add/update tests around `App` helpers where feasible:

- selected-session derivation returns `loadedSnapshot=null` and `loadedEntries=[]` when `selectedId` and `snapshot.session_id` differ;
- patch session metadata updates sessions, selected snapshot, and cache;
- rename does not call `refreshSelected`;
- archive does not call `refreshSelected`;
- provider/reasoning configure does not call selected full refresh;
- `session.configured` event patches metadata and does not schedule selected full refresh;
- transcript append event upserts entries and ignores duplicates;
- append event defers/refreshes when active leaf references a missing entry;
- cache invalidation on delete/project switch/history events.
- stale selected refresh generation is ignored, including the A → B → A race;
- stale session-list response from a previous project is ignored.
- event reducer maps events to typed patch operations/fallback reasons without directly mutating React state.

### Client component tests

For `MessageList`/`ChatPane`:

- selected ID differs from snapshot session ID => loading state, not stale transcript;
- selected ID differs from snapshot session ID => transcript derivation uses empty entries, not stale entries;
- no session => existing empty state;
- cached session => transcript renders immediately.

### Daemon/store tests

- metadata-only configure does not require transcript load/recovery path;
- event subscription does not require transcript load/recovery path;
- active-branch `session.get` does not full-load transcript when no recovery is needed;
- model change after transcript exists rejects via existence query;
- session snapshot/list includes `has_transcript_entries` or equivalent model-lock field;
- configure/rename responses include provider/metadata/activity;
- `session.configured` event includes patchable metadata/provider;
- metadata patch operation preserves unrelated metadata keys;
- active-branch query returns correct ordered branch;
- full-tree query remains unchanged;
- transcript append event includes full entry data.
- model/tool completion events include enough identifiers to reconcile pending actions or explicitly require lightweight fallback.

### Integration/e2e tests

- create large session, switch away/back: cached switch immediate, no stale transcript.
- rename selected large session: no full transcript request, title updates.
- archive selected large session: no full transcript request, archive flag updates.
- running session appends transcript entries without repeated full refresh.
- `/switch`, `/fork`, export still load full tree and work.

---

## Rollout plan

### Milestone 1: UX and over-refresh cleanup

- Add loading state for mismatched selected/snapshot session.
- Centralize selected-session derivation (`loadedSnapshot`, `loadedEntries`, `transcriptLoading`) and use it for actions/controls.
- Add basic in-memory cache.
- Remove selected full refresh from rename/archive handlers.
- Do not schedule full selected refresh for `session.configured`.
- Debounce `loadSessions` from events.
- Add request generation guards for selected refreshes and session-list loads.

Expected impact: stale transcript fixed; rename/archive much faster; cached switching fast.

### Milestone 2: Server metadata optimization

- Add cheap metadata idle validation.
- Add transcript-exists query for provider lock.
- Add cheap recovery precheck and avoid recovery on broad event subscriptions.
- Add safe metadata patch operation for rename/archive.
- Expand configure/rename responses and event payloads.
- Update client to patch from responses/events.

Expected impact: archive/configure of large sessions avoids daemon transcript loads.

### Milestone 3: Smaller session-display fetches

- Add active-branch entry scope to `session.get` or a new endpoint.
- Use active-branch scope for normal selection.
- Load full tree lazily for history/export.
- Update cache scope tracking.

Expected impact: large forked sessions switch faster and transfer less data.

### Milestone 4: Incremental live updates

- Emit full transcript-entry append events.
- Append entries client-side.
- Restrict full refresh to fallback/reconciliation.

Expected impact: active running sessions stop repeatedly reloading full transcripts.

### Milestone 5: Rendering scalability

- Add virtualization.
- Lazy-render expanded tool bodies/diffs.
- Memoize provider replay/tool arg parsing.

Expected impact: very large transcripts remain responsive after data arrives.

---

## Detailed client change checklist

### `packages/web/src/App.tsx`

- Add small modular helpers/hooks, colocated initially if preferred but with clear boundaries:
  - selected-session derivation;
  - session fetch/request generation;
  - session cache;
  - session patch application;
  - event-to-patch reducer.
- Add session cache ref and helpers:
  - `getCachedSession`;
  - `writeCachedSession`;
  - `patchCachedSession`;
  - `markCachedSessionStale`;
  - `deleteCachedSession`.
- Change session selection handler to load cached snapshot or show loading state.
- Add selected transcript loading computation based on non-stale `loadedSnapshot`.
- Update `refreshSelected` to write cache and tag entries session ID.
- Add request generation guards to `refreshSelected` and `loadSessions`.
- Change rename flow:
  - no `refreshSelected`;
  - patch local metadata after success.
- Change archive flow:
  - no pre-refresh;
  - no post-refresh;
  - use metadata patch semantics when available;
  - patch local metadata after success.
- Change provider/reasoning configure flow:
  - no selected full refresh;
  - patch provider/config state after success.
- Split event handling by event type:
  - metadata patch;
  - activity patch;
  - queued input patch;
  - transcript append patch;
  - fallback full refresh only for specific events.
- Add debounced session-list refresh.
- Use `loadedSnapshot` for stop/model/composer/inspector/action precondition state.

### `packages/web/src/chatPane.tsx`

- Accept/pass transcript loading state.
- Header should use `session` summary while `snapshot` is loading.

### `packages/web/src/transcript.tsx`

- Render loading state when selected session and entries session do not match.
- Avoid deriving active branch/tool indexes/turn views/display nodes from stale entries while loading.
- Later: integrate virtualized list.
- Later: lazy-render tool details and memoize expensive parsing.

### `packages/web/src/agentApi.ts`

- Extend `getSession` options with entry scope:

```ts
interface GetSessionOptions {
  includeEntries?: boolean;
  entriesScope?: "active_branch" | "full_tree";
}
```

- Update `renameSession`/`configureSession` result types when daemon returns metadata/provider.

### `packages/web/src/types.ts`

- Add richer event payload helper types if useful.
- Add `EntryScope` type if cache tracks scope.
- Add `has_transcript_entries` or `transcript_entry_count` to session snapshot/list types once the daemon provides it.

### Suggested client helper boundaries

Keep these helpers plain functions where possible so they are easy to unit test:

```ts
deriveSelectedSessionState(input): SelectedSessionState
applySessionPatch(state, operation): StatePatch
reduceEventToOperations(event): SessionPatchOperation[]
upsertTranscriptEntry(entries, entry): TranscriptEntry[]
cacheCanSatisfy(cacheEntry, requiredScope): boolean
```

React components should receive already-derived props and should not need to know whether data came from cache, RPC, or event patches.

---

## Detailed daemon/store change checklist

### `rust/crates/agent-store`

- Add `has_transcript_entries(session_id)`.
- Add `active_leaf_is_turn_boundary(session_id)` or equivalent cheap recovery precheck helper.
- Add active-branch transcript query.
- Add a history tree/entries API that can return active branch or full tree.
- Add metadata patch/merge update helper.
- Change transcript append event insertion to include full entry data, or add a new event type for full appended entries.
- Avoid exposing large model-context action payloads in web-facing pending action/event views; add summarized action views if needed.
- Ensure appropriate indexes exist. Current `(session_id, sequence)` supports ordered full loads. The primary key `(session_id, id)` supports recursive parent lookup. No new index should be needed initially.

### `rust/crates/agent-daemon/src/main.rs`

- Extend `session.get` params with entry scope.
- Use active-branch query when requested.
- Include `has_transcript_entries` or `transcript_entry_count` in `session.get` and possibly `session.list`.
- Do not make `events.subscribe` recover/full-load every subscribed session.
- Change `session_configure` validation:
  - model changes: strict source mutation validation + `has_transcript_entries`;
  - metadata-only: cheap idle validation or no idle validation depending on desired semantics.
- Add/use metadata patch RPC semantics for rename/archive where possible.
- Return metadata/provider/activity from configure/rename.
- Publish richer `session.configured` events.
- Include `action_row_id` in model/tool completion/error events where possible, or document that clients must reconcile pending actions via lightweight snapshot refresh.

### `rust/crates/agent-daemon/src/runtime.rs`

- Add cheap metadata validation method that does not call `recover_if_needed`.
- Add cheap recovery precheck so active-branch reads do not full-load transcripts unless an open tail actually needs repair.
- Keep strict `ensure_idle_for_source_mutation` for history/model/source-changing operations.

### `rust/docs/websocket-rpc.md`

Update websocket docs for:

- `session.get.entries_scope`;
- `session.get`/`session.list` transcript-existence field;
- metadata patch request/response semantics if added;
- new/changed `session.configured` payload;
- changed configure/rename responses;
- transcript append event full entry payload.
- action completion/error event identifiers used for pending-action reconciliation.

---

## Acceptance metrics

Track before/after for at least one small, medium, and large session.

### Session switching

- Time from click to sidebar selected row: immediate, under 50 ms.
- Time from click to transcript no longer showing stale previous session: immediate, under 50 ms.
- Time from click to controls no longer reflecting stale previous-session stop/model/queue/inspector state: immediate, under 50 ms.
- Cached session display: under 100 ms for typical sessions.
- Uncached active-branch display: target under 500 ms for medium sessions.

### Rename/archive

- No selected-session `session.get(include_entries: true)` during rename/archive.
- No selected-session `session.get(include_entries: true)` during reasoning-effort/provider-adjacent configure changes.
- Rename selected large session: target under 200 ms excluding network/DB variance.
- Archive selected large idle session: target under 300 ms excluding network/DB variance.

### Running session updates

- Full selected refreshes per normal model/tool turn: target zero after incremental append events, except fallback cases.
- Transcript update after append event: under 100 ms for typical sessions.

### Rendering

- Large transcript scroll remains responsive.
- Expanding a large tool output may take bounded time, but collapsed transcript should remain fast.

### Code structure

- Selected-session derivation, cache writes, event reduction, and patch application are covered by focused unit tests.
- Components do not contain event-payload compatibility logic or cache invalidation rules.
- Rename/archive/configure/event patch paths share the same patch helpers.

---

## Risks and mitigations

### Cache staleness

Risk: cached transcript or metadata diverges from durable state.

Mitigations:

- use event IDs and replay on reconnect;
- mark cache stale on history/compaction events if not fully patchable;
- background refresh after cache display;
- keep full refresh fallback.
- mark caches stale on websocket close/reconnect unless event replay succeeds from the cached `lastEventId`; idle sessions clear event rows, so replay cannot be treated as a complete guarantee.

### Active-branch fetch breaks history features

Risk: history picker/export only sees active branch.

Mitigation:

- track cache entry scope;
- explicitly request full tree for history/export;
- add tests for fork/switch/export.

### Event payload compatibility

Risk: web client and daemon versions mismatch.

Mitigation:

- client checks whether full entry exists in event payload;
- fallback to scheduled selected refresh if not;
- document protocol version changes.

### Virtualization scroll behavior

Risk: chat sticky-bottom and per-session scroll restoration regress.

Mitigation:

- defer virtualization until after fetch/refresh fixes;
- add focused tests/manual QA for bottom-stick behavior;
- use a mature library with variable-height support.

### Metadata mutation semantics

Risk: allowing metadata changes while running could surprise code that assumes config is stable.

Mitigation:

- decide metadata classes:
  - title can change anytime;
  - archived may require idle;
  - provider/model changes require strict idle + no transcript entries;
- encode this explicitly in daemon methods rather than treating all metadata the same.

### Complexity creep

Risk: fixing every edge case inline in `App.tsx` makes the UI harder to reason about than the current full-refresh approach.

Mitigations:

- use small helper modules/functions with unit tests;
- keep a single patch path for list/snapshot/cache;
- keep fallback refresh reasons explicit and logged;
- defer optional websocket splitting/cancellation until measurements show it is needed.

### Large action payloads

Risk: even after transcript fetches shrink, snapshots/events can still carry large model-context payloads through pending actions or action events.

Mitigations:

- expose summarized web-facing pending action/event payloads;
- keep full model context available only in store/internal daemon paths;
- add instrumentation for event/frame sizes, not just `session.get` sizes.

---

## Open decisions

1. Should rename be allowed while a session is running? The current UI effectively allows it. Keep unless product requirements change.
2. Should archive require idle? Current UI says yes. Keep, but enforce cheaply.
3. Should normal session display fetch active branch only, or should it fetch full tree for simpler client behavior? Recommendation: active branch only.
4. Should transcript append event replace existing `transcript.appended` payload or add a new event? Recommendation: add full `entry` to existing payload while keeping `entry_id`/`item` for compatibility.
5. Which virtualization library should we use? Recommendation: evaluate `react-virtuoso` first for chat-style variable-height rows.
6. Should metadata patches be a new RPC or an extension of `session.configure`? Recommendation: extend or add a patch-shaped path, but make archive/unarchive server-side merge operations rather than full metadata replacement.
7. Should `has_transcript_entries` live on `session.list`, `session.get`, or both? Recommendation: both, so header/model lock state can be correct before entries are loaded.
8. Should recovery happen on `session.get` only, or also on subscription? Recommendation: avoid recovery on broad subscriptions; perform cheap recovery precheck on selected-session reads and mutations.

---

## Suggested first PR

Keep the first PR intentionally small:

1. Add selected-session derivation (`loadedSnapshot`, `loadedEntries`, `transcriptLoading`) and use it for transcript, controls, composer queue, stop button, and inspector state.
2. Render transcript loading state when `selectedId !== snapshot?.session_id`, and avoid deriving display nodes from stale entries.
3. Add in-memory session cache with explicit `entries`, `entryScope`, `lastEventId`, and `stale` fields; display cached data on selection.
4. Add selected-refresh and session-list request generation guards.
5. Remove `refreshSelected` calls from rename/archive handlers.
6. Patch session metadata locally after rename/archive success through a shared list/snapshot/cache patch helper.
7. Stop `session.configured` from scheduling a selected full refresh.
8. Add lightweight perf logging around `refreshSelected` and websocket frame parse/size.

This first PR should provide visible improvement without changing the daemon protocol.
