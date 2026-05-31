# Turn-card transcript and switch-target plan

## Context

The selected-session cache and compact transcript index removed several full-tree
correctness hazards, but long sessions can still feel slow because normal UI
operations can fetch and render entry-level transcript data. In the current data
set, large active branches produce 22-49 MB uncompressed websocket responses,
roughly half of which is raw provider replay that the UI usually does not need.
The `/switch` dialog also currently uses compact index pages ordered from the
oldest entries forward, while users usually need the newest/recent targets first.

A new UX direction makes the performance fix simpler: the transcript view should
be turn-oriented. Historical completed turns are collapsed by default and show a
small summary card. Full entry-level detail is needed eagerly only for the
current/running turn and lazily when the user expands an older turn.

## Goals

1. Keep the durable model simple: `transcript_entries` remains the append-only
   source of truth and `sessions.active_leaf_id` remains the current branch.
2. Make selected-session load independent of full transcript size in the normal
   case.
3. Do not send raw `provider_replay` to normal UI transcript responses.
4. Show historical completed turns as collapsed cards by default.
5. Load full turn details lazily when a user expands a turn.
6. Keep the current/running turn detailed and live by default.
7. Make `/switch` populate from newest switch/edit targets without loading the
   full compact topology.
8. Preserve exact user-message restoration: picking a user message in `/switch`
   must restore the full original user content, never the truncated preview.
9. Avoid a monolithic `session.sync` endpoint. Prefer small composable RPCs.
10. Keep PRs reviewable by stacking them.

## Non-goals

- Do not move raw provider replay to a new table in the first implementation.
  It already lives in a separate `provider_replay` column; the first win is to
  stop selecting/sending it on UI endpoints.
- Do not remove entry-level export/debug/model-continuation code.
- Do not build an automatic old-session migration into daemon startup.
- Do not introduce IndexedDB.
- Do not add fat websocket patches.

## PR stack

### PR 1: Exact `/switch` user-message restore

Problem: compact tree nodes carry a truncated `display_hint`. The switch picker
uses that hint as a preview, which is fine for display, but restored composer
text must come from the full transcript entry body. A regression can occur if a
compact target carries `restoreText` from the preview or if the restore path
trusts the truncated compact hint.

Implementation:

- Ensure compact-node switch targets never carry `restoreText`.
- Make the restore helper always prefer a cached full entry body or fetch the
  full entry by id with `transcript.entries`.
- Add a frontend test proving compact node user-message targets expose only a
  truncated preview and no restore text, so restore must fetch/use the full
  body.

Verification:

- `npm run test --workspace packages/web`
- `npm run build --workspace packages/web`

### PR 2: UI transcript projection without raw provider replay

Problem: normal UI transcript endpoints select and serialize
`provider_replay`, even though raw replay is server/model-continuation data and
is about half of large-session payloads.

Implementation:

- Add a UI transcript entry projection on the Rust side that selects entry
  bodies without `provider_replay` for UI paths.
- Keep existing full entry records for model continuation/export/debug paths.
- Default frontend transcript body fetches to the UI projection.
- Add an explicit `include_provider_replay` option only where raw replay is
  intentionally needed.
- Preserve any small display metadata that the UI actually needs, either from
  existing visible `item` data or a small replay display projection if required.

Verification:

- Measure `session.get active_branch` payload before/after on a long session.
- `cargo check -p agent-store --manifest-path rust/Cargo.toml`
- `cargo check -p agent-daemon --manifest-path rust/Cargo.toml`
- `npm run test --workspace packages/web`
- `npm run build --workspace packages/web`

### PR 3: Turn-card selected-session view

Problem: even without raw replay, the UI should not need thousands of
intermediate entries for collapsed historical turns.

Backend APIs:

- `transcript.turns`
  - active branch only;
  - returns newest/tail turn cards or a bounded page;
  - contains compact summaries: user preview, final assistant preview, counts,
    timestamps, status, boundary ids, edit targets;
  - excludes raw provider replay.
- `transcript.turn_detail`
  - returns full UI-projected entries for one turn/card;
  - excludes raw provider replay by default;
  - current/running turn detail can be loaded eagerly.

Frontend:

- Add normalized selected-session turn cache:
  - `turnCardsById` / ordered turn ids;
  - `turnDetailsByTurnId`;
  - current turn detail eager;
  - historical turn details lazy on expand.
- Render collapsed historical turn cards by default.
- Render expanded/current turns with the existing detailed transcript row
  components where possible.
- Keep old entry-level selected cache temporarily as compatibility for export
  and while migrating operations.

Verification:

- Long-session selected load should fetch a small bounded turn-card payload,
  not the whole active branch.
- Expanding an old turn fetches only that turn detail.
- Current/running turn still streams smoothly.
- Existing transcript tests plus new turn-card tests.

### PR 4: Newest-first `/switch` targets

Problem: `/switch` currently pages compact topology from oldest to newest and
then derives targets client-side. Recent useful rows can arrive late.

Backend API:

- `history.targets(session_id, limit, before_sequence?)`
  - returns newest switch/edit targets first;
  - includes full `restore_entry_id` but only preview text for display;
  - includes `expected_active_leaf_id`, `transcript_revision`, and enough data
    for `history.switch` to validate;
  - no raw provider replay.

Frontend:

- `/switch` opens from cached target page if fresh.
- Otherwise fetches the first newest target page.
- Infinite/page more targets as needed.
- `history.switch` remains authoritative and validates expected active leaf,
  transcript revision, boundary/edit target validity, and active branch ids.
- Restore text still uses full entry fetch, never the target preview.

Verification:

- `/switch` first useful rows appear from the first page on long sessions.
- Selecting a user edit restores the full user text.
- Selecting turn/compaction targets still switches safely.

### PR 5: Bulk transport / denormalized read model only if needed

After PRs 2-4, normal UX should no longer fetch huge payloads. If remaining
benchmarks still show head-of-line blocking or compact-index cost:

- Move bulk transcript/detail/export fetches to HTTP with compression or a
  second websocket.
- Add denormalized compact columns/read model such as `item_type`,
  `display_hint`, `source_leaf_id`, `outcome`, and turn-card rows maintained on
  append.

## Design invariants

- Raw provider replay is needed for provider continuation, not normal transcript
  display.
- The backend remains authoritative for switchability and active branch state.
- Frontend caches are projections and can be discarded/refetched by revision.
- Revision fences remain the consistency mechanism for mutations.
- UI preview text may be truncated; mutation/restore text must come from full
  entry bodies.
- Completed historical turns are collapsed by default; expanding is an explicit
  request for detail.

## Implementation notes

- The current `provider_replay` column is already separate from `item`; selecting
  only UI columns should avoid most raw replay cost without schema changes.
- The turn-card projection can initially be computed from the active branch in
  Rust. If benchmarks show it is too slow, promote it to a denormalized read
  model later.
- For current/running turn detail, use the active branch suffix from the most
  recent open turn/compaction boundary so live updates remain smooth.
- `/switch` target previews are display-only. Never use them as composer restore
  content.
