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
3. Do not expose raw `provider_replay` on any UI/RPC transcript response.
4. Show historical completed turns as collapsed cards by default.
5. Load full turn details lazily only when a user expands a turn.
6. Keep the current/running turn represented by a live-updating summary card by
   default; tool calls and intermediate entries still load only on expansion.
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

Implementation status:

- Done in `fix/switch-restore-full-user-message`.
- The restore helper now treats `restore_entry_id` as authoritative and always
  loads the full user-message body from the selected-session body cache or from
  `transcript.entries`. Display-only `restoreText` is used only for legacy
  targets without an entry id.
- Added a compact-node switch-target test proving the compact preview is
  truncated and `restoreText` is absent.
- Follow-up clarification: `/switch` must not show a partial target list while
  the compact index is still paging. The compact dialog now shows only the
  loading state until `treeComplete` is true, unless a complete fresh tree is
  already cached.

### PR 2: UI transcript projection without raw provider replay

Problem: normal UI transcript endpoints select and serialize
`provider_replay`, even though raw replay is server/model-continuation data and
is about half of large-session payloads.

Implementation:

- Add a UI transcript entry projection on the Rust side that selects entry
  bodies without `provider_replay` for UI paths.
- Keep existing full entry records for model continuation/export/debug paths.
- Default frontend transcript body fetches to the UI projection.
- Do not add a UI/RPC raw-replay escape hatch. Raw provider replay stays
  server-side; the UI renders only semantic `TranscriptItem`s.

Verification:

- Measure `session.get active_branch` payload before/after on a long session.
- `cargo check -p agent-store --manifest-path rust/Cargo.toml`
- `cargo check -p agent-daemon --manifest-path rust/Cargo.toml`
- `npm run test --workspace packages/web`
- `npm run build --workspace packages/web`

Implementation status:

- Done in `perf/ui-transcript-no-provider-replay`.
- Store/UI RPC transcript body reads now take a `TranscriptEntryBodyMode`.
  Normal UI paths (`session.get`, `session.sync_active_branch`,
  `transcript.entries`, `history.switch` returned bodies, and
  `transcript.appended` event bodies) use `Ui`, which avoids reading/sending
  raw replay.
- Follow-up simplification: the explicit raw-replay RPC flag was removed. Full
  durable replay remains internal to model continuation/debug store reads, but
  the frontend wire type no longer contains a `provider_replay` field at all.
- Added a Postgres store test proving the UI projection omits replay while the
  full projection preserves it.

### PR 3: Turn-card selected-session view

Problem: even without raw replay, the UI should not need thousands of
intermediate entries for collapsed historical turns.

Backend APIs:

- `transcript.turns`
  - active branch only;
  - returns a bounded newest/tail page by default and older pages via
    `before_entry_id`;
  - returns collapsed turn cards with the full user message entries in that
    turn and the full final assistant message entry for that turn;
  - omits intermediate tool calls/results until expansion;
  - contains only the card metadata the UI needs: status, boundary ids, resume
    flag, start timestamp, optional compaction summary;
  - excludes raw provider replay from the wire entirely.
- `transcript.turn_detail`
  - returns full UI-projected entries for one turn/card;
  - takes the card id, leaf id, and sequence bounds from `transcript.turns` so
    the backend reads only that card path rather than materializing every card;
  - excludes raw provider replay from the wire entirely;
  - used only when the user expands a card.

Frontend:

- Add normalized selected-session turn cache:
  - `turnCardsById` / ordered turn ids;
  - `turnDetailsByTurnId`;
  - all details lazy on expand.
- Render collapsed turn cards by default, including the current/running turn.
- Render expanded turns with the existing detailed transcript row components
  where possible.
- Keep old entry-level selected cache temporarily as compatibility for export
  and while migrating operations.

Verification:

- Long-session selected load should fetch one bounded tail page of turn cards,
  not the whole active branch.
- Expanding an old turn fetches only that turn detail.
- Current/running turn still streams smoothly.
- Existing transcript tests plus new turn-card tests.

Implementation status:

- Done in `feature/turn-card-transcript-view`.
- Added composable `transcript.turns` and `transcript.turn_detail` RPCs.
  `transcript.turns` returns bounded active-branch turn-card pages; the first
  call fetches the newest/tail page and a `next_before_entry_id` cursor fetches
  older pages on demand. `transcript.turn_detail` returns detail for exactly
  one card by following that card's leaf/sequence bounds instead of recomputing
  all turn cards.
- Added selected-session turn-card cache projections (`turnCardsById`,
  `turnOrder`, `turnDetailsById`) alongside the existing body/topology caches.
- The chat pane renders turn cards when available and lazily fetches detail when
  the user clicks "Show details". Older historical cards are fetched only when
  the user clicks "Load older turns". No turn detail or older page is fetched
  in the selected-session hot path.
- Follow-up simplification: `transcript.turns` now carries full semantic
  user-message entries and the full final semantic assistant-message entry for
  every card. It no longer carries derived previews/counts; the collapsed chat
  view renders directly from those entries.
- Follow-up performance/correctness fix: `transcript.turns` selects full
  assistant-message JSON only for the terminal assistant message in each card,
  not for intermediate assistant/tool-call steps that are hidden until
  expansion. Compaction cards also preserve `last_turn_id` and
  `turn_started_at_ms` so a mid-turn compaction keeps the current turn label and
  Working timer anchored to the original turn start.
- Follow-up cache fix: when canonical `transcript.turns` advances a card,
  previously expanded detail is preserved only if the cached detail still
  reaches the card's latest active leaf. Otherwise the detail cache is dropped so
  the next expansion refetches `transcript.turn_detail` instead of showing a
  stale partial detail.
- The selected-session hot path now uses metadata-only `session.get` followed by
  `transcript.turns`; it does not fetch full active-branch bodies on session
  select, foreground refresh, accepted follow-up reconciliation, or
  `history.switch`.
- Transcript append events incrementally update the current card summary and
  any already-expanded turn detail. The canonical `transcript.turns` refresh is
  still used when the cache cannot prove an event extends the selected branch.
- Hot-path pitfall fixed during review: daemon progress should not block on
  extra per-entry transcript reads between model/tool/model work. The output
  persistence path now reuses the `insert into transcript_entries ... returning`
  record for websocket append payloads and reads revision/head state once after
  the revision bump, rather than querying each just-inserted row again while
  emitting events. The fallback lookup remains only for rare duplicate-entry,
  recovery, and compaction paths.
- Follow-up performance fix: `transcript.turns` no longer walks the whole
  active branch on selected-session load. The recursive query starts from the
  active leaf (or a server-returned older-page cursor) and walks backward only
  until it has the requested number of card starts. This keeps the hot-path
  response bounded by the requested tail-page size plus the entries inside
  those turns.
- Follow-up performance fix: `transcript.turn_detail` no longer derives every
  card before finding the requested one. The frontend sends the card id,
  leaf id, and sequence bounds from the card page; the backend validates and
  reads only that card's parent chain.
- Remaining pitfall: bounded paging is still computed from append-only
  `transcript_entries` at read time. If microbenchmarks still show card
  derivation as too slow for a single very large turn, promote cards to a
  denormalized read model in PR 5.

### Deferred: Newest-first `/switch` targets

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

### Deferred: Bulk transport / denormalized read model only if needed

After the turn-card hot path, normal UX should no longer fetch huge payloads. If remaining
benchmarks still show head-of-line blocking or compact-index cost:

- Move bulk transcript/detail/export fetches to HTTP with compression or a
  second websocket.
- Add denormalized compact columns/read model such as `item_type`,
  `display_hint`, `source_leaf_id`, `outcome`, and turn-card rows maintained on
  append.

## Design invariants

- Raw provider replay is needed for provider continuation, not normal transcript
  display. UI/RPC transcript payloads do not include it.
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
- Current/running turns stay collapsed by default; the card carries
  `start_timestamp_ms` so the "Working…" timer remains anchored without loading
  detail.
- `/switch` target previews are display-only. Never use them as composer restore
  content.
