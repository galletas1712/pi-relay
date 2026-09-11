# One-time deployment migrations

These files are deployment artifacts, not automatic startup migrations.

## Claude model-route catalog refresh

`refresh-claude-model-routes.sql` moves exact Claude routes that can affect
current, executable, or replayed state:

- `claude-opus-4-8` → `claude-opus-5`
- `claude-fable-5` → `claude-fable-5-1`

It updates current session defaults, still-deliverable
`queued_inputs.provider_config` rows (`queued`/`consuming`), unfinished
`actions.provider_config` rows (`pending`/`blocked`/`running`), and every
`events.payload.provider` reference in one locked transaction while preserving
every other JSON field. Events are historical facts, but their provider
configuration can repaint frontend state on replay and therefore must move
with the current route. Consumed/cancelled queue rows and terminal action
provider snapshots are preserved as historical route facts. The migration
does not rewrite `transcript_entries.provider_replay`, action results,
compaction results, or other historical response payloads.

Each session referenced by an updated current/executable route or replay-visible
provider event gets one `session_revision`/`updated_at` bump. `queue_revision`
is unchanged because route snapshots do not alter queue membership or ordering.
A rerun finds no source routes in the migrated scopes and is a safe no-op.

Use this cutover order:

1. Stop the daemon and every other runtime/database writer. Keep every writer
   stopped through the migration. Do not remove containers, volumes, databases,
   data directories, or workspace roots.
2. With all writers stopped, take the authoritative restorable `pg_dump` of
   `pi_relay` and verify the archive before proceeding. For a custom-format
   archive, the minimum archive-integrity check is:

   ```sh
   backup="pi_relay-before-model-route-refresh.dump"
   pg_dump --format=custom --file="$backup" "$DATABASE_URL"
   pg_restore --list "$backup" >/dev/null
   ```

   `pg_restore --list` verifies archive readability, not a complete restore.
   Also complete the deployment's established restore verification against a
   new isolated empty scratch database—never the live `pi_relay` database.
   Do not proceed unless verification succeeds and the backup is retained
   outside any container or volume affected by deployment operations.
3. Check host-owned configuration manually. Update any retired model in the
   host daemon TOML and host-owned subagent-role `SKILL.md` frontmatter; these
   files are outside this repository and this migration does not modify them.
4. Record exact preflight counts for references that the migration must move.
   This query intentionally includes current sessions, active queue rows,
   unfinished actions, and all replay-visible provider configuration events,
   but excludes terminal queue/action snapshots. A non-`claude` kind paired
   with one of these models in these migrated scopes is malformed and must be
   remediated before proceeding:

   ```sql
   select location, kind, model, count(*) as rows, count(distinct session_id) as sessions
   from (
       select 'sessions.provider_config (current)' as location,
              id as session_id,
              provider_config->>'kind' as kind,
              provider_config->>'model' as model
       from sessions
       where provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
       union all
       select 'queued_inputs.provider_config (active)',
              session_id,
              provider_config->>'kind',
              provider_config->>'model'
       from queued_inputs
       where status in ('queued', 'consuming')
         and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
       union all
       select 'actions.provider_config (unfinished)',
              session_id,
              provider_config->>'kind',
              provider_config->>'model'
       from actions
       where status in ('pending', 'blocked', 'running')
         and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
       union all
       select 'events.payload.provider (replay-visible)',
              session_id,
              payload#>>'{provider,kind}',
              payload#>>'{provider,model}'
       from events
       where payload#>>'{provider,model}' in ('claude-opus-4-8', 'claude-fable-5')
   ) retired_routes
   group by location, kind, model
   order by location, kind, model;
   ```

   Separately inventory the terminal route snapshots that will remain
   unchanged:

   ```sql
   select location, status, kind, model, count(*) as rows
   from (
       select 'queued_inputs.provider_config (terminal)' as location,
              status,
              provider_config->>'kind' as kind,
              provider_config->>'model' as model
       from queued_inputs
       where status not in ('queued', 'consuming')
         and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
       union all
       select 'actions.provider_config (terminal)',
              status,
              provider_config->>'kind',
              provider_config->>'model'
       from actions
       where status not in ('pending', 'blocked', 'running')
         and provider_config->>'model' in ('claude-opus-4-8', 'claude-fable-5')
   ) historical_routes
   group by location, status, kind, model
   order by location, status, kind, model;
   ```

5. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/refresh-claude-model-routes.sql
   ```

   The transaction locks the four relevant tables, collects affected session
   ids before rewriting, updates only exact Claude routes in the migrated
   scopes, bumps each affected session once, and raises if any exact source
   model remains in a current session, active queue row, unfinished action, or
   provider event payload. That postcondition also makes malformed
   non-`claude` source-model references in those scopes fail closed.

6. Run the first step 4 query as the zero-reference check. It must return zero
   rows. The separate terminal-snapshot inventory is expected to remain
   unchanged.
7. Deploy the new binaries and web assets, then start the daemon.
8. Open representative migrated sessions. Complete one normal turn and one
   compaction, and confirm the expected current model is used.

After deployment is verified, delete this SQL file and its runbook section in a
follow-up commit.

## Single-delegation-wakeup upgrade

The new code never enqueues, republishes, or cancels a partial (per-child)
parent wakeup. A parent whose queue still holds a `queued`/`consuming` partial
at cutover would replay a stale `running` snapshot as its next turn, so the
cutover cancels those rows. The owning delegations are still `running` and still
owe their terminal wakeup, so no parent is stranded.

Order matters. Running the migration while the old binary is live lets the old
daemon re-enqueue a partial; running it after the new binary starts leaves a
window in which a stale partial is replayed into a parent transcript.

1. Stop the daemon and every other runtime/database writer. Keep every writer
   stopped through the migration. Do not remove containers, volumes, databases,
   data directories, or workspace roots.
2. With all writers stopped, take an authoritative restorable `pg_dump` of
   `pi_relay` and verify archive readability plus an isolated scratch restore
   before proceeding (never restore over live `pi_relay`; for a custom-format
   archive, start with `pg_restore --list <archive> >/dev/null`). Retain it
   outside any container or volume affected by deployment operations.
3. Record what will be cancelled (read-only; expected to be a handful of rows,
   often zero):

   ```sql
   select session_id,
          status,
          count(*) as rows,
          min(created_at) as oldest
   from queued_inputs
   where priority = 'steer'
     and status in ('queued', 'consuming')
     and content->>'type' = 'daemon_tool_observation'
     and client_input_id ~ '^delegation-steer:[^:]+:[^:]+:[^:]+$'
   group by 1, 2
   order by 1, 2;
   ```

4. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/single-delegation-wakeup.sql
   ```

   The transaction cancels every still-deliverable partial wakeup, bumps the
   affected parents' revisions so the first reconnect refetches their queue, and
   raises if any partial remains. A rerun matches no rows.

5. Re-run the step 3 query; it must return zero rows.
6. Deploy the new binaries.
7. Start the daemon and verify that a fan-out wakes its parent exactly once, at
   terminal status.

After deployment is verified, delete these one-time artifacts in a follow-up
commit.
