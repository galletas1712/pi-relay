# Selected Session Performance and Smoothness Fix Plan

## Problem statement

After the selected-session cache landed, a few hot paths still behave like they are uncached:

- Selecting a long session that was already visited discards its frontend tree/body cache and performs a fresh `session.get(include_entries=true, active_branch)` pull.
- `/switch` reuses the compact tree only while the session stays selected; switching away and back loses that tree and makes the picker slow again.
- `history.switch(return_active_branch=true)` and accepted idle follow-ups can return the full active branch, so switching or sending to a long session scales with branch length.
- The `Working…` timer is anchored through a freshly-created object every transcript render, so frequent transcript updates reset the interval and can keep it stuck at `0s`.
- Some recovery paths still replace the whole active branch, which can create transcript jitter during high-frequency tool/model updates.

## Design goals

1. Keep the codebase simple and modular: small RPCs, no monolithic sync endpoint, no fat events, no IndexedDB.
2. Preserve one frontend authority for selected-session state, but retain that authority per visited session rather than as a single slot.
3. Make hot paths proportional to the actual missing data:
   - session revisit: render cached bodies/topology immediately;
   - `/switch` open: use cached compact topology when revision-compatible, otherwise fetch compact index foreground;
   - `history.switch`: return the new branch ID list plus only missing bodies;
   - idle follow-up acceptance: return only the suffix after the client's base leaf.
4. Keep consistency server-authoritative. The daemon/Postgres revisions decide whether an incremental operation is safe; if not, the frontend does a foreground compact-index refresh or targeted active-branch sync.
5. Keep transcript rendering stable by preserving object identity for unchanged entries and avoiding full active-branch replacement when a suffix or sparse body set is enough.

## Implementation plan

### 1. Per-session selected cache map

`useSelectedSessionStore` will keep an in-memory `Map<sessionId, SelectedSessionCache>` plus a current cache pointer. The public shape stays intentionally small:

- `cache` and `cacheRef` expose the current session cache.
- `replace(next)` writes `next` into the map (for non-null `sessionId`) and makes it current.
- `reset(sessionId)` switches the current pointer to a cached entry if present, otherwise to `emptySelectedSessionCache(sessionId)`; it does **not** evict other sessions.
- `drop(sessionId)` evicts a deleted session.

This preserves existing component assumptions while making revisits and `/switch` use cached tree/body state immediately.

### 2. Incremental active-branch reconciliation

Use the existing `session.sync_active_branch` RPC for selected-session recovery instead of `session.get(include_entries=true)` whenever we already have a base leaf:

- The client sends `base_leaf_id = cache.activeBranchEntryIds.at(-1) ?? null`.
- If the server says `unchanged`, merge only overview metadata.
- If it says `extended`, append the returned suffix entries and merge overview metadata.
- If it says `branch_changed`, fall back to a full active-branch `session.get` because the current branch no longer extends the cached one.

This makes foreground/focus recovery and event fallback cheap for the common case of a session continuing from the visible leaf.

### 3. Accepted idle follow-up returns suffix sync, not full branch

For `input.follow_up` accepted directly into an idle session:

- Add optional `base_leaf_id` to the request.
- The daemon persists the accepted input as today, then returns `active_branch_sync` using `session.sync_active_branch` semantics and a fresh overview snapshot.
- The web client applies the suffix response. It keeps the old `active_branch` handling as a compatibility fallback but no longer asks the hot path to return a full branch.

This keeps sending to a long session proportional to the new user message/turn-start suffix.

### 4. Fast, safe `/switch`

Opening `/switch` remains foreground/deterministic rather than speculative background work:

- If the per-session compact tree is `treeComplete` and its `treeTranscriptRevision` matches the current snapshot revision, render immediately.
- Otherwise fetch `transcript.index` compact pages in the foreground. This fetch contains topology/display hints only, not full transcript bodies.

Switching to a target should also avoid a full branch body pull:

- The frontend computes the target branch IDs from the compact tree and sends:
  - `expected_transcript_revision`: the revision used to derive the target;
  - `active_branch_entry_ids`: the target branch IDs it expects;
  - `missing_body_ids`: the subset of that branch not present in `entriesById`.
- The daemon checks the transcript revision before switching. If the revision changed, it returns `history_changed`; the frontend refreshes the compact index in the foreground and asks the user to retry.
- On success, the daemon returns the actual `active_branch_entry_ids` and only the requested missing body records that are on the new branch.
- The frontend merges those sparse bodies and installs the returned branch ID list. If any IDs are still missing, it foreground-fetches those IDs with `transcript.entries` before rendering the switched branch.

The legacy `return_active_branch` boolean remains supported for older callers, but the web UI uses the sparse branch protocol.

### 5. Stable `Working…` clock

Change the timer from a per-render `WorkingClockAnchor` object to durable primitive props:

- `runningTurnStartMs(entries)` still finds the server-persisted turn start.
- `WorkingIndicator` receives `startMs` and `serverTimeMs`.
- Internally it creates an anchor ref only when `startMs` changes, so appending tool/model entries does not reset the elapsed timer.

### 6. Jitter control

- Apply suffix sync and sparse switch results through the selected-session reducer rather than replacing the whole active branch.
- Keep `session.get(include_entries=true)` only for cold load or explicit fallback when the branch changed unexpectedly.
- Preserve structural sharing in `entriesById`; unchanged transcript rows keep their object identities.

### 7. Verification

- Add/update reducer tests for active-branch suffix sync and sparse switch results.
- Update timer tests for the stable primitive clock API.
- Run web tests/build and Rust checks for changed crates.

## Pitfalls and decisions while implementing

- We intentionally do **not** add background fetching for `/switch`. If the cached compact tree is not revision-compatible, the dialog shows a foreground loading state and fetches compact topology.
- We intentionally keep `session.get` as the fallback for `branch_changed`; this is simpler and safer than trying to infer a branch diff when the cached base leaf is no longer an ancestor.
- The in-memory per-session cache is session-tab lifetime only. Persistence can come later if needed, but it is not required to fix the reported hot paths.
