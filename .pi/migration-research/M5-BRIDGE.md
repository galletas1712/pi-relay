# M5: bridge/supervisor

**Status: complete. B1–B8 all PASS** (clean end-to-end run, 2026-08-09;
traces: `.pi/m1-demo/traces/m5-b{1..8}.jsonl`, one run-away smoke trace
`m5-smoke.jsonl`; per-test journal mirrors in `packages/bridge/data/journal/`).

The bridge is the pi-relay product process: it supervises one
`pi --mode rpc` host per session (4 prime extensions loaded, GLM-5.2 via the
m1-demo SSE shim), serves a slim WS/JSON-RPC contract to clients, and keeps a
Postgres control plane so sessions, queued commands, and resumable event
streams survive host crashes and bridge restarts.

## Where it lives

`packages/bridge/` (new npm workspace package `@pi-relay/bridge`; deps hoisted
to the repo root `node_modules`, root `package-lock.json` updated — no changes
to `rust/`, `packages/web/`, or any existing package). Runtime on Node 24
native TS (type stripping), `tsc --noEmit` clean. Layout + full contract spec:
`packages/bridge/README.md`.

## Architecture

- **HostProcess** (`src/host.ts`): spawns
  `node <pi-cli> --mode rpc --tools ipython --session-dir data/sessions
  [--session-id <uuid> | --session <file>]`, JSONL stdio framing,
  ack-correlated rpcs with timeouts, stderr → `data/logs/<sessionId>.log`,
  graceful stop then SIGKILL. The bridge **generates the session UUID and pins
  it** via `--session-id`: bridge id == pi sessionId == prime-rlm kernel
  snapshot dir key, from birth (no discovery race, idempotency-friendly).
- **Supervisor** (`src/supervisor.ts`): per-session send/event promise-chain
  locks; maps pi rpc events → contract events (field truncation at 16 KiB);
  every contract event is spooled to PG (transactionally bumping
  `sessions.last_event_seq`, ring-trimmed at 2000/session) then broadcast.
  Crash path: `host_exited` → journal `maybe_lost` marking →
  `session.error(host_crashed)` → respawn (`pi --session <file>` resume,
  generation bumped, session-id verified) → journal drain (pending entries in
  seq order, serialized on the send chain).
- **PG control plane** (`migrations/0001_init.sql`): `sessions` (state CHECK,
  host_pid, host_generation, last_event_seq), `command_journal` (pending →
  acked/failed/maybe_lost, idempotency_key link), `event_spool`
  (PK(session_id,seq)), `idempotency_keys` (sha256 params + stored response),
  `schema_migrations`. **Test container `pi-relay-bridge-test-pg` on
  127.0.0.1:56432 only** — the live `pi-relay-postgres` (55432) and
  `infra-control-1` were never touched. Password lives only in
  `.pi/m1-demo/.bridge-pg-password` (mode 600, gitignored tree).
- **WSS server** (`src/server.ts`): upgrade-time Origin allowlist (exactly one
  Origin, exact match → 403 `forbidden_origin`) + `Authorization: Bearer`
  (401 `unauthorized`), 8 MiB frame cap (close 1009), JSON text frames only.
  Per-connection-per-session subscription watermarks make replay+live
  gap-free and duplicate-free (replay holds the per-session event lock).

## Contract v0 (slim)

Methods: `session.list/create/attach/detach`, `prompt.send`, `session.steer /
followUp / abort / getState`, `subagent.tree`, and typed `not_implemented`
stubs for `workspace.*` + `mcp.*`. Events: `session.state`, `message.delta`,
`tool.exec`, `subagent.lifecycle`, `session.error`, all with per-session
monotonic `seq`. Attach resume: no `fromSeq` → live-only at head;
`fromSeq:H` → replay `H+1..head` (flagged `replayed:true`) then live; trimmed
range → typed `event_gap {minAvailable, headSeq}`. Errors are typed codes
(`session_not_found`, `session_busy`, `host_unavailable`, `event_gap`,
`idempotency_conflict`, `bad_request`, `method_not_found`, `not_implemented`,
`internal`). `session.create`/`prompt.send` take `idempotencyKey` (replay →
stored response + `replay:true`; key reuse with different params →
`idempotency_conflict`; crash-mid-flight retries matched via the journal).

## Verification (B1–B8, real GLM-5.2 through the shim)

| test | scenario | result |
|---|---|---|
| B1 | create→prompt→stream+ipython tool exec; drop mid-stream, reattach `fromSeq=H` → replay resumes at exactly `H+1`, union contiguous 1..21, no dupes; `B1-SUM=385` | PASS |
| B2 | steer during a 30 s cell → final reply is `B2-STEERED`; prompt while running → `session_busy` | PASS |
| B3 | `kill -9` host mid-cell → `host_exited`+`host_crashed` → respawn gen 2 (`pi --session` resume) → getState consistent → kernel var `TOPAZ-OWL-77` restored from post-cell snapshot | PASS |
| B4 | host down (20 s respawn delay lever) → followUp + prompt journaled `pending` (`queued:true`) → drained in seq order after respawn → follow-up flushed by the prompt's turn → journal 0 pending | PASS |
| B5 | two concurrent sessions: kernels isolated (A has alpha / not beta, B vice versa), distinct files/pids/seq streams over one multiplexed socket | PASS |
| B6 | `kill -9` the BRIDGE → hosts die via stdin EOF → restart → boot reconcile from PG respawns hosts (`reason:bridge_restart`, gen bumped) → client reattach resumes at `H+1` → kernel marker `JADE-FALCON-42` restored | PASS |
| B7 | rlm child spawned via ipython tool → `subagent.lifecycle` admitted→completed; `subagent.tree` returns `b7child` (depth 1, completed, childSessionId) | PASS |
| B8 | bad/missing Origin → 403; bad/missing token → 401; 9 MiB frame → 1009; typed `method_not_found`/`not_implemented`/`session_not_found`/`bad_request`; idempotent create (same sessionId, replay) + conflict; idempotent prompt (one journal row, one user message); `event_gap` via white-box spool trim | PASS |

Run: `bash packages/bridge/test/run-all.sh` (starts a bridge if needed;
B4/B6 restart it with the env they need and restore defaults).

## Findings that shaped the design

1. **pi `follow_up`/`steer` queues are in-memory only** — the bridge journal is
   the sole durable record. Acked-but-unconsumed steer/follow_up on crash →
   `maybe_lost`, deliberately **not** auto-replayed (double-apply risk).
2. **`follow_up` on an idle pi agent does not start a turn** (probe-verified on
   0.84.1: message waits in `_followUpMessages` until the next prompt). B4's
   pair-drain documents the honest semantics; README calls it out.
3. **`pi --session <file>` resume preserves the session id** and tolerates a
   dangling tool call from a `kill -9` mid-cell (B3 verified with GLM).
4. **prime-rlm kernel snapshots** land at
   `<sessionDir>/prime/<sessionId>/kernel/kernel-state.dill` ~1.5 s after each
   successful cell, plus a final flush on extension dispose (which runs on
   stdin-EOF host shutdown — that is why B6's bridge kill still restores the
   kernel). After `kill -9` only the post-cell snapshot exists (B3 waits for
   the file before killing).
5. **`session_name`** (not `childName`) is the prime-rlm lifecycle payload
   field; tree rows are built by absorbing `rlm_child_lifecycle` entries from
   the pi session file (durable) + spool (not-yet-flushed), keyed by
   `rlm_child_id`, phases deduped.
6. npm workspace hoisting put `pi-coding-agent` at the repo root; the bridge
   resolves `dist/cli.js` via `import.meta.resolve` so hoisting is
   transparent. Version pinned exactly `0.84.1`.

## Ops notes

- Bridge: `./run-bridge.sh` (foreground; env: port 8730, token from
  `.pi/m1-demo/.bridge-auth-token`, PG DSN to the test container; starts the
  GLM shim detached if needed — the shim survives bridge restarts, which B6
  relies on).
- Migrations: `node src/migrate.ts` (idempotent, `schema_migrations`-tracked);
  also run automatically at boot.
- **Test PG teardown**: `docker rm -f pi-relay-bridge-test-pg` removes the
  container and its volume. To keep the container but wipe data:
  `docker exec pi-relay-bridge-test-pg psql -U postgres -d pi_relay_bridge -c
  "TRUNCATE event_spool, command_journal, idempotency_keys, sessions"`.
  Recreate from scratch:
  `docker run -d --name pi-relay-bridge-test-pg -e POSTGRES_PASSWORD=$(tr -d '\n\r' < .pi/m1-demo/.bridge-pg-password) -e POSTGRES_DB=pi_relay_bridge -p 127.0.0.1:56432:5432 postgres:16-alpine` then `node src/migrate.ts`.
- Bridge-local state (`packages/bridge/data/`: sessions, journal mirrors, logs,
  scratch cwd) is disposable; PG + pi session files are the durable records.

## Known limitations (P2)

- `maybe_lost` steer/follow_up entries are surfaced, not retried.
- Orphaned Python kernels may linger briefly after host `kill -9` (no dispose
  runs); the respawned host boots a fresh kernel. Track + reap later.
- Concurrent duplicate `idempotencyKey` creates can spawn two hosts (narrow
  race; journal lookup closes it for prompt.send only).
- Event ring trim (2000/session) → `event_gap` is typed, but no long-term
  event archive beyond the pi session file.
- `session.close` (graceful retirement) is intentionally absent from v0;
  sessions persist and are reconciled until a close method lands.
- WSS is plain `ws://` on 127.0.0.1 — TLS termination is an upstream concern,
  as in the rest of the m1-demo stack.
