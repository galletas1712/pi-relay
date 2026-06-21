# Temporary stage-to-delegation migration runbook

This document is intentionally temporary. It supports the one-time hard
`stage` -> `delegation` durable-state rename for #168/#169 and should be
removed/abandoned with the migration branch after the operator has migrated any
pre-rename state.

## Vocabulary and compatibility policy

`delegation` is the former `stage`: a bounded parent/child unit of subagent
work. A delegation contains either:

- one **full** subagent, which writes in the parent workspace; or
- a **readonly_fanout** of one or more read-only subagents, each in its own
  disposable snapshot.

The rename is deliberately hard. The post-rename daemon exposes delegation table
names, JSON fields, websocket/RPC methods, and provider-facing tool names. This
migration updates old durable state before restart instead of adding permanent
compatibility aliases. Keeping aliases would leave two vocabularies in the
runtime, increase test surface, and make future cancellation/steering behavior
ambiguous.

Existing ID **values** are preserved. Old IDs such as `stage_abc` remain valid
opaque delegation IDs; only newly-created delegations use `delegation_*`. This
avoids a risky global primary-key/value rewrite and keeps file paths stable.
For the same reason `.pi-handoff/stage_*` directory names are preserved.

## Recommended operator sequence

1. Stop `pi-agentd`. Do not migrate while the daemon is writing to Postgres or
   handoff files.
2. Take a database backup, preferably:

   ```bash
   pg_dump "$PI_RELAY_DATABASE_URL" > pi-relay-before-stage-to-delegation.sql
   ```

3. Dry-run:

   ```bash
   rust/scripts/migrate-stage-to-delegation-state.sh --database-url "$PI_RELAY_DATABASE_URL"
   ```

4. Inspect the summary and any fail-closed conflicts.
5. Apply:

   ```bash
   rust/scripts/migrate-stage-to-delegation-state.sh --database-url "$PI_RELAY_DATABASE_URL" --apply
   ```

6. Restart the post-rename daemon.

The Rust entry point is:

```bash
cargo run -p agent-store --example migrate_stage_to_delegation_state -- [options]
```

## Dry-run, apply, idempotency, and conflict behavior

Without `--apply`, the tool opens a transaction, performs the same detection and
rewrite work, prints a summary, and rolls back. With `--apply`, it commits DB
changes and writes structured handoff manifests if `--handoff-root` is provided.

The migration is designed to be rerun. Already-renamed schema, already-rewritten
tool/RPC names, and already-populated `delegation_id` fields are left unchanged.
Mixed old/new state is accepted only when it can be proven equivalent or safely
merged.

Fail-closed examples:

- `sessions.stage_id` and `sessions.delegation_id` both exist with different
  non-null values.
- `stages` and `delegations` both contain conflicting rows for the same ID.
- A structured JSON object contains both an old key (for example `stage_id`) and
  a new key (`delegation_id`) with different non-null values.
- An old schema has a running `readonly_fanout` row with no child sessions and
  no `expected_subagents` column. The original intended fan-out count cannot be
  inferred safely, so the operator must inspect/repair or allow spawning to
  finish before applying.

If the new JSON key exists but is `null` and the old key has a non-null value,
the migration fills the new key from the old value and removes the old key.

## Database state migrated

Schema migration covers:

- `stages` table -> `delegations` table;
- `sessions.stage_id` -> `sessions.delegation_id`;
- old/new table merge checks for mixed-state retries;
- old constraint/index names such as `stages_pkey`,
  `stages_parent_session_id_fkey`, `sessions_stage_id_fkey`, and
  `stages_parent_created_idx`;
- `delegations.expected_subagents integer not null default 1`.

For old valid schemas that predate `expected_subagents`, the migration infers:

- `full` delegations: `expected_subagents = 1`;
- `readonly_fanout` delegations with existing child sessions:
  `expected_subagents = count(distinct child sessions linked by stage_id or
  delegation_id)`;
- terminal zero-child fan-outs: `expected_subagents = 1`, because no running
  barrier will wait on them and this preserves the one-or-more fan-out schema
  shape;
- running zero-child fan-outs: fail closed because the intended fan-out size is
  unknowable.

If a previous partial migration created `delegations.expected_subagents = 1` for
a readonly fan-out that already has more child sessions, this tool repairs the
count upward to the durable spawned child count.

## Persisted JSON/tool/RPC/session config state migrated

The migration performs contextual structured rewrites only:

- provider/model-facing old tool names and tool-call names:
  - `stage.full`, `stage.start_full`, `stage_start_full`, `stage_full`
    -> `delegate_writing_task`;
  - `stage.fanout`, `stage.start_ro_fan`,
    `stage.start_readonly_fanout`, `stage_start_readonly_fanout`,
    `stage_start_ro_fan`, `stage_fanout`
    -> `delegate_readonly_tasks`;
  - `stage.status`, `stage_status` -> `inspect_delegation`;
  - `stage.cancel`, `stage_cancel` -> `cancel_delegation`.
- websocket/RPC method/type/name fields when they are explicit RPC fields:
  `stage.*` -> `delegation.*`.
- known orchestration field keys:
  `stage_id` -> `delegation_id` and `stageId` -> `delegationId`.
- JSON string fields that are generated tool/RPC payloads, such as
  `args_json`, provider replay `raw_json`, function-call `arguments`, and JSON
  string `output` from old delegation tools. These are parsed as JSON and
  patched structurally when possible.
- generated parent completion steer IDs:
  `queued_inputs.client_input_id` values with `stage-steer:` become
  `delegation-steer:`.
- generated parent completion steer text in queue rows scoped by
  `stage-steer:`/`delegation-steer:` client IDs, for example the leading
  `Stage ... finished` wording.
- generated system prompts in `sessions.system_prompt`. The old
  `## Subagent delegation` prompt section is replaced with the current
  delegation section from `PI.md`, including #169 steering/cancellation
  semantics.

This ensures parked agents restarted after the rename do not retain live
instructions to call removed stage tools/APIs.

## State deliberately not migrated

The migration does **not** rewrite arbitrary prose or arbitrary JSON string
values. In particular, it preserves:

- user transcript text;
- tool output text;
- final messages;
- source-code snippets;
- arbitrary JSON object keys such as `{ "stage": "prod" }` and
  `{ "stages": [...] }`;
- handoff `final_message.md` and `transcript.md` markdown files.

The rationale is data preservation. Historical transcript/tool/final-message
content is user data and may legitimately contain natural-language words such
as "Stage lighting notes: keep stage and screen aligned." Broad text
replacement would corrupt that data. Only known generated state that the
post-rename runtime must consume is migrated.

If `--handoff-root` is provided, only `.pi-handoff/**/index.json` manifests are
parsed and rewritten structurally. The migration never renames
`.pi-handoff/stage_*` directories and never edits handoff markdown.

## Backups and rollback

On `--apply`, unless `--no-backups` is passed, the script creates table copies
inside the current schema with names like:

```text
sessions_stage_to_delegation_backup_YYYYMMDD_HHMMSS
stages_stage_to_delegation_backup_YYYYMMDD_HHMMSS
delegations_stage_to_delegation_backup_YYYYMMDD_HHMMSS
transcript_entries_stage_to_delegation_backup_YYYYMMDD_HHMMSS
actions_stage_to_delegation_backup_YYYYMMDD_HHMMSS
queued_inputs_stage_to_delegation_backup_YYYYMMDD_HHMMSS
events_stage_to_delegation_backup_YYYYMMDD_HHMMSS
```

These are convenience snapshots, not a substitute for `pg_dump`. Prefer a full
`pg_dump` before applying, especially because DDL runs in one transaction and
the backup tables live in the same database.

Rollback guidance:

1. Stop the daemon.
2. Restore the pre-migration `pg_dump` into a fresh or cleaned database.
3. If `--handoff-root` was used with `--apply`, restore filesystem handoff
   manifests from filesystem backup or source control if needed. The migration
   edits only `index.json`; it does not create a filesystem backup for handoff
   files.
4. Restart the pre-rename daemon, or rerun the migration and restart the
   post-rename daemon.

## Current #169 semantics to remember after restart

The migrated prompts point agents at the current delegation behavior:

- Prefer `steer_subagent` for correcting a running full subagent instead of
  cancelling and restarting.
- Cancellation is terminal.
- Cancellation produces transcript-only cancellation handoff paths; normal
  completed handoff artifacts are written only by the completion CAS winner.
- Cancellation does not roll back workspace edits or remote-state side effects.

See `PI.md` for the current model-facing rules and `agent-daemon` delegation
runner docs/comments for the durable barrier and repair behavior.
