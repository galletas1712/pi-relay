# One-time concurrent-delegation upgrade

These files are deployment artifacts, not automatic startup migrations.

1. Take a `pg_dump` of `pi_relay`.
2. Snapshot or back up the configured runtime `workspace_root`.
3. Record running delegations and child sessions with read-only queries.
4. Stop the old control plane and runtime. Do not remove containers, volumes,
   databases, or workspace roots.
5. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/concurrent-delegations-preflight.sql
   ```

   This command raises and exits nonzero on duplicate full writers, any full
   delegation that does not expect exactly one child, any full delegation that
   does not link exactly one child except a failed launch with no materialized
   child, partially materialized running legacy delegations, or children whose
   canonical role/task metadata cannot be reconstructed. Otherwise it succeeds
   without writes. On failure, stop and inspect; never delete or cancel rows
   automatically.
6. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/concurrent-delegations.sql
   ```

   The transaction assigns deterministic legacy child indices, reconstructs
   canonical launch specifications, gives failed full launches without a child
   a terminal-only historical placeholder, installs the full-delegation count
   constraint, and rebuilds/validates required indexes. A rerun safely
   reinstalls the named constraint and rebuilds named invalid index artifacts.

7. Run:

   ```sh
   psql "$DATABASE_URL" -f rust/migrations/concurrent-delegations-prompts.sql
   ```

   `sessions.system_prompt` is rendered once at `session.start` and stored
   forever, while tool descriptions and JSON schemas are rebuilt from the
   registry on every model request. This transaction removes the
   pre-concurrency single-delegation instructions from every top-level prompt
   that still carries them, so no stored prompt contradicts the
   concurrent-capable tool schemas. It does not regenerate whole prompts:
   older surrounding prose stays as rendered. Subagent prompts are left alone,
   because subagents have no delegation tools. It raises if any top-level
   prompt still carries the old text. A rerun matches no rows. Rewritten
   sessions get a fresh `updated_at`, so the first connect after this step
   re-warms every one of them in the background.

8. Deploy matched control/runtime binaries and republish the host-owned
   workflow packages described in `rust/docs/workflow-package-update.md`.
9. Start the normal daemon.
10. Verify old sessions, boot recovery, all active delegation cards, writer
    exclusivity, and a read-only launch beside an old running writer.
11. Retain backups until old and newly concurrent delegations finish.

After deployment is verified, delete these one-time artifacts in a follow-up
change. Never use `docker compose down -v`, prune the Postgres volume, reset the
runtime workspace root, or point tests at production.

## Known limitations

The startup index check in `agent-store/src/postgres/schema.rs::migrate`
verifies index **validity** (`indisvalid`/`indisready`), not **definition**, and
covers only the three uniqueness-critical indexes
(`delegations_parent_launch_key_uq`, `sessions_delegation_spawn_index_uq`,
`delegations_parent_running_full_uq`). `concurrent-delegations.sql` rebuilds and
validates all five it touches, adding `delegations_parent_running_idx` and
`delegations_running_created_idx`. A valid-but-wrong index with a required name
passes startup; only the one-time `drop index if exists` + `create` in
`concurrent-delegations.sql` repairs it.

The eight-slot read-only cap is enforced by the parent-session row lock plus the
Rust admission check. Unlike full-writer exclusivity, it has no unique-index
backstop, because an aggregate `sum(expected_subagents)` bound is not
expressible as a partial unique index.

An old running read-only fan-out larger than the new capacity is preserved. It
continues normally and blocks new read-only admission until that delegation is
terminal; reservations are not released child by child.
