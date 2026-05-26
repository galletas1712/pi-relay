# Session Sync Redesign Plan

## Goals

- Keep Postgres as the only durable source of truth for session state.
- Make daemon/frontend/Postgres views converge through canonical projections,
  not inferred client patches.
- Serialize short state transitions per session without locking unrelated
  sessions.
- Avoid user-input queue leases for new queue consumption paths: a queued input
  should remain `queued` until the same transaction that appends it to the
  transcript marks it `consumed`.
- Preserve action attempt fencing for long-running model/tool/compaction work.
- Keep old-session migration outside the daemon runtime. Fresh schema gets the
  new columns; existing databases are upgraded with the standalone script in
  this PR.

## Non-goals

- Do not modify the TypeScript packages.
- Do not build an automatic old-session/data migration into daemon startup.
- Do not hold database locks during provider requests, tool execution, or
  compaction LLM calls.

## Model changes

Fresh databases add these columns:

- `sessions.session_revision bigint not null default 0`
- `sessions.queue_revision bigint not null default 0`
- `sessions.transcript_revision bigint not null default 0`
- `queued_inputs.follow_up_position integer null`
- `queued_inputs.updated_at timestamptz not null default now()`
- action lease columns are left for a later worker-pool PR; the existing
  `attempt_id` fencing remains the completion guard.

Existing databases are migrated by `rust/scripts/migrate-session-sync-v1.sql`.

## Locking strategy

Every short state transition that mutates session-owned state should take the
session row lock first:

```sql
select id from sessions where id = $1 for update
```

This is a row lock for one session, not a database or table lock. It serializes
mutations for one `session_id`; other sessions continue normally.

The lock is held only while validating and committing state changes. Long-running
provider/tool work happens outside the lock and commits via the existing
`attempt_id` fence.

## Queue consumption strategy

New queued-input consumption should avoid `queued -> consuming -> consumed`.
Instead:

1. The daemon peeks the next dispatchable queued input.
2. It feeds that input into the in-memory session reducer.
3. The Postgres transition transaction locks the session row and atomically:
   - appends transcript entries,
   - updates `queued_inputs.status` from `queued` to `consumed`,
   - creates follow-on action rows,
   - updates revisions,
   - inserts events.

If the daemon crashes before the transaction, the input is still `queued`. If it
commits, the input is in the transcript and the next action is durable.

The old `consuming` recovery code remains for rows created by previous daemon
versions or in-flight deployments.

The commit path also fences the peeked row with its Postgres row version and
requires that it is still the canonical next queued input when the transcript
transaction commits. If steering is inserted/promoted above it, or if the
follow-up row is edited/cancelled/reordered before commit, the transaction
fails, evicts the daemon's stale in-memory cursor, and the next touch reloads
from Postgres.

## Canonical queue projection

One store helper builds the ordered active queue. It is used by:

- `session.get`
- queue mutation RPC responses
- queue-related event payloads

Ordering:

1. `priority='steer'`, ordered by steering/promote time.
2. `priority='follow_up'`, ordered by `follow_up_position`, then creation time,
   then id.

Steering stays at the top and is not reorderable.

## Revisions and events

Queue-visible transitions increment `queue_revision`.
Transcript data changes increment `transcript_revision`. Active-leaf-only view
changes increment `session_revision` but do not bump `transcript_revision`
because no transcript rows changed.
Any visible transition increments `session_revision`.

Queue-related events include the canonical post-transition queue snapshot and
revision fields:

```json
{
  "session_revision": 12,
  "queue_revision": 5,
  "transcript_revision": 7,
  "activity": "queued",
  "queued_inputs": []
}
```

Clients should replace cached queue state when an event has a newer
`queue_revision`. Older events or events without canonical queue projection
should cause a refetch rather than inferred patching.

## Snapshot consistency

`session.get` should read from a single consistent snapshot. The implementation
uses a repeatable-read, read-only transaction for session row, pending actions,
queue, activity, and event high-water reads.

## Active cursor fencing

Short transition commits check that newly appended transcript entries extend
the current `sessions.active_leaf_id`. If another request switches or recovers
the active branch before a stale daemon cursor commits, the commit fails instead
of appending rows to an inactive branch and claiming the active leaf.

## Branch base

This branch was rebased/fetched against `origin/main` at
`da7ed0d8d2fb35ab3448d59cecb4c730a4631d02` (`Return relative paths from Grep
tool (#83)`). The recent provider SSE/state replay changes keep long-running
provider I/O outside store transactions, which matches this plan. The Grep
relative-path change is tool-layer-only and does not change the session sync
architecture.

## Verification

- `cargo check --manifest-path rust/Cargo.toml`
- `cargo test --manifest-path rust/Cargo.toml`
- Postgres integration tests are included where practical and skip unless
  `PI_RELAY_TEST_DATABASE_URL` is set.
