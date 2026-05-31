# Session Performance Regression Repair Plan

## Context

After the selected-session cache and compact transcript index landed, the long-term architecture is better: one normalized selected-session cache, compact tree topology separate from full bodies, sparse `history.switch`, thin websocket events, and revision-based reconciliation.

However, a few implementation details regressed perceived performance and smoothness:

- `/switch` can appear empty for seconds on long sessions.
- Cold active-branch fetches for long sessions can be slower than before.
- Sending to an idle long session can block on full-session reads.
- `history.switch` can block on full-session reads before doing a sparse switch.
- The transcript view can briefly clear/jitter when topology metadata advances ahead of loaded active-branch bodies.

This PR repairs those regressions without changing the overall architecture.

## Goals

1. Keep the codebase simple and modular.
2. Keep APIs focused:
   - cheap active-leaf query;
   - cheap target-boundary validation;
   - compact tree index;
   - sparse body fetches.
3. Remove full-transcript reads from send/switch validation hot paths.
4. Render `/switch` progressively while the compact index is paging.
5. Keep the visible transcript projection stable unless matching bodies are loaded.
6. Add enough perf logging to identify any remaining bottleneck without making logging mandatory.

## Non-goals

- No monolithic `session.sync`.
- No fat websocket patches.
- No IndexedDB persistence.
- No speculative background fetch for `/switch`.
- No reintroduction of noisy `SENDING...` / `SYNCING...` rows.

## Backend plan

### 1. Cheap active-leaf validation

Expose `PostgresAgentStore::active_leaf_id(session_id)` publicly and use it from `ensure_expected_active_leaf`.

The previous implementation loaded the whole stored session just to compare one id. That made idle follow-up submission scale with transcript size.

### 2. Cheap turn-boundary validation for `history.switch`

Add `PostgresAgentStore::transcript_leaf_is_turn_boundary(session_id, leaf_id)`.

`history.switch` should validate:

1. source mutation is idle;
2. `expected_active_leaf_id` still matches via the cheap active-leaf query;
3. the target leaf is a switchable boundary via the single-row boundary query;
4. the store-level `switch_active_leaf` transaction still performs membership/revision/branch-id validation.

This keeps the daemon authoritative without building a full `TranscriptStore` for every switch.

### 3. Restore the fast full active-branch query

The sparse-switch work changed full active-branch fetches into:

1. recursive branch id query;
2. `id = any($ids)` body query.

That shape is useful for sparse body fetches, but it can be slower for a full active-branch load. Restore full active-branch loads to a single recursive body query, while keeping the id-list path for sparse switch validation.

### 4. Perf logs

When `PI_RELAY_PERF` is set, log timings for:

- `session.get`;
- `session.sync_active_branch`;
- `transcript.index`;
- `history.switch`;
- `input.follow_up`.

The logs should be lightweight and only active when explicitly enabled.

## Frontend plan

### 1. `/switch` progressive rendering

Keep `/switch` foreground/deterministic, but do not wait for every compact-index page before displaying rows.

Implementation:

- Fetch `transcript.index` with a larger page size.
- Apply each page to the selected-session cache.
- Push the current compact-node list into the dialog after each page.
- Show an inline loading row while more pages are loading, rather than replacing all rows with a spinner.

### 2. Keep topology metadata from clearing the transcript

`transcript.index` is topology state. It should not advance the visible active branch to an active leaf whose bodies are not loaded.

Implementation:

- `applyTreeIndex` may update tree metadata/revisions, but it must not mutate the visible snapshot's `active_leaf_id`.
- `ChatPane` should render the active leaf from the loaded active-branch entries when present, falling back to the snapshot only when no bodies are loaded.
- The selected-session cache should keep a separate `treeActiveLeafId` for picker/topology consumers.

This makes the transcript prefer "show the stable loaded branch" over "clear because metadata points at an unloaded leaf."

### 3. Reducer/UI tests

Add/update tests for:

- tree-index application does not mutate visible `active_leaf_id`;
- the transcript active leaf can be derived from loaded entries when snapshot metadata is ahead.

## Verification

Run:

- `npm run test --workspace packages/web`
- `npm run build --workspace packages/web`
- `cargo check -p agent-store --manifest-path rust/Cargo.toml`
- `cargo check -p agent-daemon --manifest-path rust/Cargo.toml`

If a Postgres test database is available, also run the relevant `agent-store` tests with `PI_RELAY_TEST_DATABASE_URL`.

Verified on this branch:

- `npm run test --workspace packages/web` passed (91 tests).
- `npm run build --workspace packages/web` passed (Vite chunk-size warnings only).
- `cargo check -p agent-store --manifest-path rust/Cargo.toml` passed.
- `cargo check -p agent-daemon --manifest-path rust/Cargo.toml` passed.

## Implementation notes / pitfalls

- The full active-branch query still needs display-parent semantics for compaction summaries: when walking backwards from a compaction root, the parent edge is the compaction `source_leaf_id` if present.
- `history.switch` boundary validation is intentionally daemon-level, not store-level. The store remains a low-level transactional switch primitive and keeps membership/revision/branch-id checks.
- `transcript.index` already ships truncated display hints; this PR keeps that property and avoids relying on full body text for picker display.
- While implementing, `activeLeafIdFromEntries` was added in the chat pane as a tiny UI guard: loaded active-branch bodies are treated as the visible projection tail. Snapshot metadata can still be ahead during reconciliation, but it no longer forces `MessageList` to derive a branch from a missing body.
- `/switch` progressive rendering intentionally leaves rows clickable only when their compact branch can be derived. `switchToTarget` now rejects a target whose branch is not present yet with a clear "history index is still loading" error rather than sending an empty branch-id fence.
- The progressive picker change stayed inside the existing picker component instead of exporting internal row-render helpers just for tests. That keeps the component boundary simpler; the cache/active-leaf invariants are covered by pure tests.
- Because `applyTreeIndex` no longer mutates the visible snapshot leaf, it records the index response's `active_leaf_id` separately as `treeActiveLeafId`. This preserves correct picker "current path" highlighting without letting topology metadata clear the transcript body projection.
