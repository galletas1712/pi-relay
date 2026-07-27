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

## `prompt-profile-gating.sql`

`sessions.system_prompt` is rendered once at `session.start` and stored forever.
Read-only subagent sessions started before capability gating carry a rendered
instruction to push a branch to the configured remote — a real side effect that
outlives their disposable workspace snapshot. New sessions never render it.

Only a *still-running* read-only subagent can act on that sentence, so the
rewrite is limited to `subagent_type = 'read_only'` rows whose delegation is
`running` or `cancelling`, and within those rows to the rendered `## Workspace`
block. **Matching zero rows is the expected, healthy outcome** — on a quiet
deployment there is nothing live to fix, and skipping this migration entirely is
defensible if the preflight count below is 0.

1. Take a `pg_dump` of `pi_relay`.
2. Preflight — count what would be rewritten. `block` inlines the migration's
   `pg_temp.workspace_block`, so the predicate is identical:

   ```sql
   select count(*) from sessions s
   cross join lateral (
     select case
       when position(reverse(E'\n## Workspace\n') in reverse(s.system_prompt)) = 0 then ''
       else substr(s.system_prompt,
                   length(s.system_prompt)
                     - position(reverse(E'\n## Workspace\n') in reverse(s.system_prompt))
                     - length(E'\n## Workspace\n') + 2)
     end as block
   ) w
   where s.subagent_type = 'read_only'
     and exists (select 1 from delegations d
                 where d.id = s.delegation_id and d.status in ('running','cancelling'))
     and position(E'\n## Tools\n' in w.block) > 0
     and position(
           'modify files in the Git workspace subdirectory directly. Before publishing changes, create a new descriptive branch and push that branch to the configured remote.'
           in w.block) > 0;
   ```

   If this is 0, stop here.
3. Stop the control plane, or run while no read-only subagent is active. The
   transaction takes an `access exclusive` lock on `sessions`; a running
   subagent already holds its prompt in memory, so the rewrite reaches it only
   on restart.
4. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/prompt-profile-gating.sql
   ```

   It reports how many prompts it rewrote, and `notice`s the ids of any live
   read-only prompt whose `## Workspace` block still carries the sentence
   instead of aborting — inspect those by hand. A rerun matches no rows. It does
   not regenerate whole prompts and does not add the new `## Subagent contract`
   or `./.pi-handoff/` text: those sessions run on code that makes no such
   guarantee.
5. Verify against the `pg_dump`: diff the `## Project Instructions` section of
   one affected prompt — it must be byte-identical, even if the project's
   `AGENTS.md` quotes the push sentence.
6. Deploy matched control/runtime binaries and start the daemon.
7. Spot-check `/system` on an old read-only subagent session.

Never use `docker compose down -v`, prune the Postgres volume, reset the
runtime workspace root, or point this at a test database you care about.
