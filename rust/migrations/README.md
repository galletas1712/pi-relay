# One-time deployment migrations

These are operator-run deployment artifacts, not automatic startup migrations.
Never assume daemon startup rewrites historical session data.

## Required before this image revision

- [`image-content-cutover.md`](image-content-cutover.md) — **run before
  deploying or starting this revision against existing sessions.** Take and
  verify a backup, stop the daemon/runtime and every other writer, preserve all
  PostgreSQL volumes and workspace directories, then complete the runbook
  through its fixed-point check before resuming writers.

## Other one-time cutovers

- [`single-delegation-wakeup.md`](single-delegation-wakeup.md) /
  [`single-delegation-wakeup.sql`](single-delegation-wakeup.sql) — cancels
  still-deliverable stale per-child delegation wakeups while preserving the
  terminal wakeup owed by each running delegation. Follow the linked runbook's
  backup, stopped-daemon, preflight-query, migration, zero-row verification,
  deploy, and fan-out verification sequence.

After a deployment and its historical-session checks are verified, remove the
completed one-time artifacts in a follow-up change.
