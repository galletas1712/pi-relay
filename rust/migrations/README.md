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

## Bash-only call-description cleanup

`bash-only-call-descriptions.sql` is a separate one-time cleanup for sessions
written by PR #330. It removes the relay-owned `call_description` field from
non-Bash canonical tool-call arguments in transcript entries, queued daemon
observations, tool actions, and persisted event payloads. It also removes the
old OpenAI `apply_patch` description header and non-Bash descriptions from
provider-replay records. Canonical Bash arguments, MCP arguments, unknown tool
arguments, and operational results are preserved.

Run it only after taking a `pg_dump` and stopping the daemon:

```sh
psql "$DATABASE_URL" -v ON_ERROR_STOP=1 \
  -f rust/migrations/bash-only-call-descriptions.sql
```

The script takes table locks, performs all updates in one transaction, and is
safe to rerun. For each session with an actual changed row, it bumps
`session_revision` once for each changed category. Transcript/provider-replay
changes also bump `transcript_revision`; queued-input content changes also
bump `queue_revision`; action/event payload changes and system-prompt changes
only bump `session_revision`. Action and event changes share one category, so a
session with both is bumped once for that category. The revision flags are
collected from `UPDATE ... RETURNING` rows, so unchanged rows and unrelated
sessions are not bumped, and rerunning the script does not bump anything again.
Do not remove containers, volumes, databases, or workspace roots. Deploy the
Bash-only code after the script succeeds, verify the affected sessions, and
then delete this one-time artifact in a follow-up commit.
