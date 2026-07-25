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

7. Deploy matched control/runtime binaries and republish the host-owned
   workflow packages described in `rust/docs/workflow-package-update.md`.
8. Start the normal daemon.
9. Verify old sessions, boot recovery, all active delegation cards, writer
   exclusivity, and a read-only launch beside an old running writer.
10. Retain backups until old and newly concurrent delegations finish.

After deployment is verified, delete these one-time artifacts in a follow-up
change. Never use `docker compose down -v`, prune the Postgres volume, reset the
runtime workspace root, or point tests at production.

Existing sessions retain their persisted prompts and remain valid. Those
conservative prompts do not gain concurrent launch behavior. Start a new
session after deployment and workflow publication to receive the new `PI.md`
contract and use concurrent orchestration.

An old running read-only fan-out larger than the new capacity is preserved. It
continues normally and blocks new read-only admission until that delegation is
terminal; reservations are not released child by child.
