# One-time upgrade migrations

These files are deployment artifacts, not automatic startup migrations. Delete
each one in a follow-up commit once its deployment is verified.

## `single-delegation-wakeup.sql`

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

## `drop-daemon-observation-call-id.sql`

`DaemonToolObservation` no longer carries `tool_call_id`. **This one is
optional and unordered**: the Rust type does not set
`#[serde(deny_unknown_fields)]`, so historical rows that still carry the key
deserialize and render unchanged with or without it. Run it whenever
convenient — before or after the deploy — purely so the durable jsonb stops
carrying a key nothing reads.

Both statements are idempotent and re-runnable; a second run rewrites the same
rows to the same value. Nothing indexes or constrains the key, and
`events.payload` never embeds the observation body, so no event rewrite is
needed.

```sh
psql "$DATABASE_URL" -f rust/migrations/drop-daemon-observation-call-id.sql
```

Verify (must return zero after the run):

```sql
select count(*) from transcript_entries
 where item->>'type' = 'daemon_tool_observation'
   and item ? 'tool_call_id';
select count(*) from queued_inputs
 where content->>'type' = 'daemon_tool_observation'
   and content->'content' ? 'tool_call_id';
```
