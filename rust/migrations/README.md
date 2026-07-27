# One-time single-delegation-wakeup upgrade

These files are deployment artifacts, not automatic startup migrations.

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
   select q.session_id,
          q.status,
          count(*) as rows,
          min(q.created_at) as oldest
   from queued_inputs q
   where q.priority = 'steer'
     and q.status in ('queued', 'consuming')
     and q.content->>'type' = 'daemon_tool_observation'
     and q.client_input_id ~ '^delegation-steer:[^:]+:[^:]+:[^:]+$'
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
