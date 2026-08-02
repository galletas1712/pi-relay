# One-time database upgrades

These files are deployment artifacts, not automatic startup migrations.

## Private workspace ownership

`2026-03-20-private-workspace-ownership.sql` is the one-time bridge from
main-era session rows to the current private-workspace lifecycle.

Safe rollout:

1. Stop `pi-agentd` and any process that can create or delete sessions.
2. Take and verify a PostgreSQL backup.
3. Run the script's preflight audits. Repair every parentless read-only row,
   invalid root/full/read-only shape, duplicate private workspace identity, and
   full child that does not exactly share its parent.
4. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/2026-03-20-private-workspace-ownership.sql
   ```

   The script is additive/backfill-only and rerunnable. Any conflicting
   ownership mapping aborts the transaction rather than overwriting identity.
5. Save the post-check output, then deploy/start the new daemon.

The migration never lists runtime directories and never marks an unknown
workspace safe to delete. Do not start the new daemon until every root,
independent history fork, and read-only session has exactly one mapping and
every full subagent has none.

## Single delegation wakeup

The new code never enqueues, republishes, or cancels a partial (per-child)
parent wakeup. A parent whose queue still holds a `queued`/`consuming` partial
at cutover would replay a stale `running` snapshot as its next turn, so the
cutover cancels those rows. The owning delegations are still `running` and still
owe their terminal wakeup, so no parent is stranded.

Order matters. Running the migration while the old binary is live lets the old
daemon re-enqueue a partial; running it after the new binary starts leaves a
window in which a stale partial is replayed into a parent transcript.

1. Take a `pg_dump` of `pi_relay`.
2. Stop the daemon. Do not remove containers, volumes, databases, or workspace
   roots.
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
