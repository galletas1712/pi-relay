# agent-store

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-store` is the only durable backend. `PostgresAgentStore` wraps an SQLx
`PgPool` and owns normalized Postgres persistence for sessions, transcript
entries, the queued-inputs ledger, actions, events, projects, and daemon
config. SQL is written by hand — Postgres is the clearest language for the
JSONB-heavy ledger and the recursive transcript/recovery queries. There is no
repository trait; the daemon calls the concrete store directly. Why this crate
is authoritative and trait-free lives in [design decisions](../design-decisions.md)
("Postgres Store Is The Storage Crate", "Postgres Is Authoritative"); this doc
covers the mechanics.

## Responsibilities

- Persist and query sessions, projects, transcript forest, queued inputs,
  actions, and the observable event stream.
- Serialize one session's short state transitions with a per-session row lock,
  without locking unrelated sessions or holding locks across provider/tool I/O.
- Track per-session revision counters and emit canonical queue projections so
  daemon/frontend/Postgres views converge by replacement, not inferred patches.
- Fence queue consumption and active-leaf appends so a stale in-memory daemon
  cursor cannot mutate history it no longer owns.
- Provide the recovery invariants the daemon relies on after a crash.
- Serve cheap metadata / active-leaf / turn-boundary queries and bounded
  turn-card pages so selected-session load does not scale with transcript size.

## Schema

Tables (created idempotently by `migrate`; see `postgres/schema.rs`):

```
projects(id, name, workspaces jsonb, metadata jsonb, ...)
sessions(id, project_id?, outer_cwd, workspaces jsonb, active_leaf_id?,
         system_prompt, provider_config jsonb, metadata jsonb,
         session_revision, queue_revision, transcript_revision)
daemon_config(key, value jsonb)
transcript_entries(session_id, id, parent_id?, timestamp_ms, item jsonb,
                   provider_replay jsonb, turn_id?, sequence bigserial)
                   primary key (session_id, id)
queued_inputs(id, session_id, priority, content jsonb, origin jsonb?,
              status, follow_up_position?, client_input_id?, created/updated_at)
actions(id, session_id, turn_id?, action_id, attempt_id, kind, status,
        payload jsonb, result jsonb?, created/updated_at)
events(id bigserial, session_id, type, payload jsonb, created_at)
```

- `transcript_entries` is an append-only forest. `parent_id` points within the
  same session; `sequence` is a global insertion-order serial used for ordering
  and pagination. Inserts are `on conflict (session_id, id) do nothing`, so
  replays are idempotent.
- `queued_inputs` idempotency is keyed by a partial unique index on
  `(session_id, client_input_id)`.
- `provider_replay` is a sidecar column holding raw provider continuation data.
  It is never serialized to RPC/web responses (see Notes).

The persistence-facing vocabularies (`ProviderKind`, `InputPriority`,
`QueuedInputStatus`, `ActionKind`, `ActionStatus`, `SessionActivity`,
`EventType`, ...) are typed Rust enums in `lib.rs` that serialize to the same
Postgres/websocket strings; invalid database values fail at decode time.

## Per-session locking and revisions

Every short transition that mutates session-owned state takes the session row
lock first via `lock_session_tx`:

```sql
select id from sessions where id = $1 for update
```

This is a single-row lock for one `session_id`. Other sessions proceed
normally, and the lock is released at commit — long-running provider, tool, and
compaction LLM work runs outside the lock and reconverges via the `attempt_id`
fence on the relevant action row.

`bump_revisions_tx` advances the three counters in one update:

```
session_revision    += 1   on every visible transition
queue_revision      += 1   when the active queue changed
transcript_revision += 1   when transcript rows changed
```

An active-leaf-only switch bumps `session_revision` (and emits a fresh
projection) without touching `transcript_revision`, because no transcript rows
changed. Queue-visible transitions carry the canonical post-transition queue
snapshot plus all three revisions in their event payload
(`queue_event_payload`). Clients replace cached queue state when an event has a
newer `queue_revision`; an event without the projection forces a refetch rather
than an inferred patch.

## Canonical queue projection

`queue_state_tx` builds the one ordered active-queue projection used by
`session.get`, every queue mutation RPC response, and queue event payloads.
Active rows are `status in ('queued','consuming')`, ordered by
`QUEUED_INPUT_DISPATCH_ORDER`:

1. `steer` before `follow_up`;
2. steers by `origin.promoted_at` (else `created_at`);
3. follow-ups by `follow_up_position` (nulls last);
4. then `created_at`, then `id`.

Steers sort at the top and are not reorderable. The projection also derives
`activity`: `Running` if any action is unfinished, else `Queued` if any active
queued input exists, else `Idle`.

## Queue consumption (peek + version-fenced commit)

New queued input never takes a consuming lease. The daemon peeks the canonical
next row (`take_next_queued_input` / `..._steer_input`, which also reads
`xmin::text` as `row_version`), feeds it into the live session, then the
transcript-append transaction (`persist_outputs`) marks that same row
`consumed`. The consume update only matches when:

```
status='queued' AND xmin = peeked row_version
              AND id = (canonical next queued row right now)
   OR  status='consuming' AND origin.claim_id = <claim>
```

If steering was inserted/promoted above it, or the follow-up was
edited/cancelled/reordered before commit, the row version or canonical-next
check fails, the transaction errors, and the daemon's stale cursor reloads from
Postgres. The `consuming` branch only exists for legacy rows written by older
daemons; `reset_abandoned_consuming_inputs` resets any leftover `consuming`
rows to `queued` on first touch after restart, closing the
consumed-but-not-transcripted gap without leaving a lease for new rows.

```
peek ──▶ live reduce ──▶ persist_outputs tx
                              ├─ append transcript_entries
                              ├─ queued_inputs: queued ─▶ consumed  (xmin + canonical-next fence)
                              ├─ insert action rows
                              ├─ bump revisions
                              └─ insert events  (incl. input.consumed + projection)
```

`persist_outputs` also enforces the active-leaf fence: if the first appended
entry's `parent_id` no longer equals the session's current `active_leaf_id`,
the commit fails instead of appending to an inactive branch.

## Queued follow-up mutations

Only rows matching `priority='follow_up' AND status='queued'` are mutable.
`update_queued_input`, `cancel_queued_input`, and `reorder_queued_follow_ups`
each lock the session, optionally check `expected_queue_revision`, mutate, and
return the canonical queue. Outcomes are normal results, not errors:

- Stale `expected_queue_revision` → `reason="queue_changed"`, no mutation.
- Row no longer an editable follow-up → `reason="not_editable"`.
- Edit to identical content → no-op, no revision bump, no event.
- Reorder must supply the exact current follow-up id set; a mismatch is
  `queue_changed`, and re-supplying the current order is a no-op.

Cancel and any consuming-reset renumber the remaining follow-ups densely via
`renumber_follow_ups_tx` (rewriting `follow_up_position = 0..n-1`); the wire
never carries gapped positions. Steers stay immutable on top: the follow-up
mutation paths do not match them, and `cancel_queued_input` on a steer returns
`not_editable`.

`promote_queued_input` flips a follow-up to `steer`, clears its
`follow_up_position`, stamps `origin.promoted_at`, renumbers remaining
follow-ups, and emits `input.promoted` in the same transaction. When the update
matches zero rows it selects the row's real status and returns
`{ promoted: false, status, queue }` (a no-op carrying the canonical snapshot)
rather than a hard error; a genuinely missing id returns `QueueMutationError`.
This is what lets a stale UI steer click on an already-consumed row resolve as
an info no-op instead of an error.

Mutating methods emit `input.queued` / `input.promoted` / `input.updated` /
`input.cancelled` / `input.reordered`, each carrying the canonical projection.

## Actions and recovery

Action rows are durable model/tool/compaction work records. Model and tool rows
start `pending`; compaction rows start `running`. Model action payloads store
only `context_leaf_id` (a pointer to the immutable transcript leaf), not the
full model context — recovery rebuilds context by walking
`transcript_entries` from that leaf (`model_context_for_leaf`).

`action_is_unfinished` = `status in ('pending','blocked','running')`. These
rows are execution leases owned by the live daemon process. At startup,
`mark_all_unfinished_actions_stale` flips abandoned rows to `stale` (and bumps
the owning sessions' `session_revision`) before clients are accepted. The sole
exception is an attempt-fenced post-compaction dispatch intent in `pending` or
`running`, described below. Its payload contains the narrower durable
owner/generation/expiration lease used for restart reclaim.
Ordinary completion is fenced by
`(id, attempt_id, status in ('pending','running'))`; marked post-compaction
completion additionally requires its exact unexpired owner/generation/context
lease. A late completion from a stale attempt or lease therefore matches zero
rows and cannot mutate history.

Recovery invariants the daemon relies on:

- An open transcript tail is valid while an unfinished action explains it. A
  clean boundary with an unfinished action is legitimate live work — this is
  what lets queued follow-ups wait behind provider-backed compaction without
  triggering transcript repair.
- If a `stale` action left the active transcript in an open turn, first touch
  rehydrates and appends a crashed turn boundary.
- `cancel_unfinished_session_work` marks the unfinished rows `interrupted` and
  emits `session.work_cancelled`; session-wide cancellation does not create a
  model/tool action row. It is idempotent — a second call matches no rows and
  returns no event.

## Compaction

Compaction is a typed transcript root, not a transcript replacement (see
[design decisions](../design-decisions.md)). The store side:

- `create_compaction_action` (manual, boundary scope): requires the active leaf
  to be a turn boundary and not already a compaction summary.
- `block_model_action_for_compaction` (auto): transitions a `pending`/`running`
  model action to `blocked` and inserts a sibling `running` compaction action in
  the same transaction. Because this path always owns a blocked model action,
  its scope is `MidTurn` (carrying the blocked action's row/attempt ids and the
  turn's persisted `turn_started_at_ms`) even when a resumed action is anchored
  directly on a `CompactionSummary` boundary. Manual compaction remains
  `Boundary`.
- `complete_compaction_action`: re-checks the action is still unfinished and
  validates the source leaf. If the source leaf changed *and* a `MidTurn`
  blocked model action is no longer blocked, it marks the compaction `stale`.
  Otherwise it installs a new `CompactionSummary` root (plus any continuation
  suffix), repoints `active_leaf_id`, and for `MidTurn` scope flips the blocked
  model action back to `pending` re-anchored on the compacted leaf. Its payload
  receives a typed `post_compaction_dispatch` marker containing the exact
  action row, attempt, and compacted context leaf. The same transaction marks
  compaction complete and persists `compaction.auto_state`, including the
  consecutive-recompaction bound, before returning the resumed action for
  daemon dispatch.
- `fail_compaction_action` updates auto-failure/suppression metadata,
  terminally errors the related unfinished model action for `MidTurn`, terminally
  errors the compaction action, and records both error events in one
  transaction. Manual `Boundary` failure has no model action to update.

The marker is a narrow payload-backed dispatch outbox/lease for the
commit-to-spawn/register crash window. Global and per-session abandoned-action
sweeps preserve only `pending` or `running` model rows carrying it; ordinary
pending/running/blocked work is still marked stale. Claim takes the session and
action row lock, verifies the exact row/attempt/context leaf and active leaf,
then retains the marker while writing:

- a new random `owner_id`;
- `generation = 1` for pending work or the previous generation plus one for an
  expired running lease;
- a database-clock `expires_at_ms` 30 seconds in the future.

An unexpired running lease is not claimable. The runner renews its exact
owner/generation every 10 seconds. Completion, error, reactive re-blocking,
interruption, and task-failure staling are owner/generation fenced; terminal
writes remove the marker atomically. Startup validates/reclaims pending or
expired work. The daemon keeps a watchdog armed for the process lifetime,
sleeps to the next database-derived expiry, wakes on heartbeat/runner loss, and
backs off/retries transient inspection or recovery failures. Renewal loss stops
the heartbeat but leaves the runner awaited so its own already-committed
terminal or reactive-compaction handoff can finish; replacement registration
aborts a genuinely stale owner after expiry. Deterministic marker corruption is
returned as a typed claim error and changed to `error` with a `model.error`
event under an exact status/marker/owner-generation fence. SQL/pool/query/commit
and context/runtime-load errors retain the marker for retry rather than
terminally classifying infrastructure failure as corrupt durable data.

The guarantee is at-least-once dispatch. If a process dies after a provider
accepted the request but before Postgres records the terminal result, lease
expiry can issue a duplicate provider call. The current provider adapters do
not send provider idempotency keys, so exactly-once calls cannot be claimed.
The row/attempt/owner fences do prevent stale owners from committing a second
durable result. Remove this narrow payload protocol only when a general durable
dispatch outbox/lease provides the same atomic intent, ownership heartbeat,
startup reclaim, and terminal-clear behavior.

Auto-compaction failure/success bookkeeping lives in session metadata under
`compaction.auto_state`. Compaction terminal transitions update it in the same
transaction as their action rows; ordinary successful model completion can
independently call `reset_auto_compaction_failures`. Consecutive failures past
`max_consecutive_failures` set `suppressed`.

## Transcript reads and turn cards

The transcript forest is read several ways; UI-facing reads use
`TranscriptEntryBodyMode::Ui` (selects `'[]'::jsonb` for `provider_replay`),
while model continuation / debug reads use `Full`.

- `model_context_for_leaf` / `branch_entries_to_leaf` walk parent edges to a
  leaf for model dispatch and compaction.
- `active_branch` / `sync_active_branch` walk from `active_leaf_id` upward,
  following the compaction `source_leaf_id` edge across summary roots.
  `sync_active_branch` reports `Unchanged` / `Extended` / `BranchChanged`
  against a client `base_leaf_id` and returns only the delta.
- `transcript_tree_index` returns paginated compact topology nodes
  (`item_type`, `turn_id`, `outcome`, `can_switch_to`, truncated `display_hint`)
  by `sequence`, without bodies, for the switch picker.
- `transcript_turns` returns a bounded newest-first page of collapsed turn
  cards for the active branch. The recursive query in `turn_cards.rs` starts
  from the active leaf (or a `before_entry_id` cursor) and walks backward only
  until it has the requested number of turn starts, so the response is bounded
  by page size plus the entries inside those turns. Each card carries the full
  user-message entries and the full terminal assistant-message entry only;
  intermediate tool steps are omitted until expansion. Compaction cards preserve
  `last_turn_id` and `turn_started_at_ms` so a mid-turn compaction keeps the
  turn label and Working timer anchored to the original start.
- `transcript_turn_detail` reads exactly one card's path using the card id,
  leaf id, and sequence bounds from the page, instead of recomputing all cards.

Turn-card status maps `TurnFinished` outcome to `Completed`/`Open`, compaction
summaries to `Compacted`, and `can_resume` is set for `Interrupted`/`Crashed`
turns.

## Cheap hot-path queries

To keep send/switch validation independent of transcript size:

- `active_leaf_id(session_id)` — single-row read used to validate
  `expected_active_leaf_id` on idle follow-up submission.
- `transcript_leaf_is_turn_boundary` / `active_leaf_is_turn_boundary` —
  single-row boundary check for `history.switch` (a null leaf counts as a
  boundary). `switch_active_leaf` remains the low-level transactional primitive
  that still does membership / revision / branch-id validation.
- `session_snapshot` and the transcript-page/index reads run in a
  `repeatable read read only` transaction so the session row, actions, queue,
  activity, and event high-water mark come from one consistent snapshot.

`latest_model_token_usage_estimate` walks the active path to the nearest
completed/errored model action with usage, returning the server token count plus
the suffix entries appended after that anchor leaf, so the daemon can estimate
context size without re-counting the whole branch.

## Notes

- `provider_replay` is never on the wire. `TranscriptEntryRecord` marks the
  field `#[serde(skip_serializing)]`, and UI body reads select an empty replay
  array. Raw replay stays server-side for provider continuation only.
- Thinking blocks are not stored here; they are discarded at the provider parse
  layer, so `AssistantItem` persisted in `item` is only `Text` / `ToolCall`.
- `list_sessions` hides `metadata.hidden='true'` sessions, sorts archived last,
  orders each group by latest user-message transcript timestamp, and suppresses
  empty web-created sessions (no transcript, no live queue, no actions) so
  abandoned drafts do not clutter the list.
- `delete_session` cascades to transcript entries, queued inputs, actions, and
  events via `on delete cascade`.
- Sessions snapshot project workspace source metadata at creation; project
  sessions get private workspace directories under `outer_cwd`.

## Related

- [agent-session](./agent-session.md) — owns `StoredSession` /
  `StoredTranscriptEntry` snapshot shapes and the live FSM this store rehydrates.
- [agent-daemon](./agent-daemon.md) — the only caller; maps store results to RPC.
- [websocket-rpc](../websocket-rpc.md) — the RPC method contract and event wire
  format these projections feed.
- [web UI](../../../packages/web/docs/web-ui.md) — consumes the canonical queue and
  turn-card projections.
