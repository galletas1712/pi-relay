# @pi-relay/bridge — M5 bridge/supervisor

The pi-relay product process. Supervises one `pi --mode rpc` host per session
(with the 4 prime extensions loaded), owns the WS client contract, and keeps a
Postgres control plane (sessions, command journal, event spool, idempotency
keys) so sessions, queued commands, and event streams survive host crashes and
bridge restarts.

## Run

```bash
./run-bridge.sh                      # migrate + reconcile + serve (foreground)
node src/migrate.ts                  # migrations only
npx tsc --noEmit                     # typecheck
bash test/run-all.sh                 # B1..B8 verification (writes traces to ../../.pi/m1-demo/traces/m5-*.jsonl)
```

`run-bridge.sh` wires env: `PI_CODING_AGENT_DIR`, `PRIME_RLM_KERNEL_VENV`,
GLM key (piped from the key file, never persisted), `BRIDGE_PG_URL` (test
container 127.0.0.1:56432 only), `BRIDGE_AUTH_TOKEN` (generated once, root-only
file), and starts the GLM SSE shim if needed. The shim survives bridge restarts.

### Config (env)

| var | default | purpose |
|---|---|---|
| `BRIDGE_PORT` | `8730` | WS listen (127.0.0.1 only) |
| `BRIDGE_AUTH_TOKEN` | required | bearer token at WS upgrade |
| `BRIDGE_PG_URL` | required | control-plane DSN |
| `BRIDGE_ALLOWED_ORIGINS` | localhost:3000, relay.pi.test | exact-match Origin allowlist |
| `BRIDGE_MAX_FRAME_BYTES` | `8388608` | 8 MiB frame cap → close 1009 |
| `BRIDGE_DATA_DIR` | `packages/bridge/data` | sessions/, journal/ mirrors, logs/ |
| `BRIDGE_SPOOL_CAP` | `2000` | per-session event ring size |
| `BRIDGE_RESPAWN_DELAY_MS` | `0` | crash→respawn backoff (test lever) |
| `BRIDGE_SPAWN_TIMEOUT_MS` | `90000` | host spawn + get_state timeout |

## Contract v0 (slim)

Single WebSocket per client. Upgrade-time auth: exactly one `Origin` header,
exact allowlist match (`403 forbidden_origin`), `Authorization: Bearer <token>`
(`401 unauthorized`). Frames are JSON text; binary rejected; >8 MiB → 1009.

Requests: `{id, method, params}` → `{id, result}` or `{id, error: {code, message, data?}}`.

Events (server→client, per-session monotonic `seq`):

```json
{"event": "message.delta", "sessionId": "…", "seq": 42, "data": {…}, "at": "…", "replayed": true?}
```

### Methods

| method | params → result | notes |
|---|---|---|
| `session.list` | → `{sessions[]}` | PG + live merge |
| `session.create` | `{cwd?, name?, idempotencyKey?}` → `{sessionId, state}` | bridge generates the UUID and pins it via `--session-id` |
| `session.attach` | `{id, fromSeq?}` → `{sessionId, headSeq, replayed}` | subscribe; see resume semantics |
| `session.detach` | `{id}` | unsubscribe |
| `prompt.send` | `{sessionId, text, idempotencyKey?}` → `{accepted, seq, queued}` | `session_busy` if a turn is running; `queued:true` while host down |
| `session.steer` | `{sessionId, text}` → `{accepted, seq, queued}` | steering queue (mid-turn) |
| `session.followUp` | `{sessionId, text}` → `{accepted, seq, queued}` | follow-up queue (post-turn) |
| `session.abort` | `{sessionId}` | abort current turn |
| `session.getState` | `{sessionId}` → state + `live` | `hostPid`, `hostGeneration`, `headSeq`; `transcript.blocks` omits textless assistant message blocks (M11c: tool-call-only steps surface as `tool` blocks only) |
| `subagent.tree` | `{sessionId}` → `{children[]}` | from prime-rlm lifecycle entries (session file + spool) |
| `workspace.*`, `mcp.*` | — | typed stubs: `not_implemented` |

### Events

| event | data |
|---|---|
| `session.state` | `{state, piType?}` — `starting/idle/running/compacting/host_down/respawning/host_exited/host_respawned/closed` |
| `message.delta` | `{kind: start/end/text/thinking, role?, delta?}` |
| `tool.exec` | `{phase: start/update/end, toolName, toolCallId, isError?, result?}` |
| `subagent.lifecycle` | `{phase: admitted/completed/error/deleted, rlm_child_id, session_name, status, …}` |
| `session.error` | `{code, message, …}` |

### Error taxonomy

`unauthorized`, `forbidden_origin` (upgrade-time) · `bad_request`,
`method_not_found`, `not_implemented`, `session_not_found`, `session_busy`,
`host_unavailable`, `event_gap` (with `minAvailable`/`headSeq`),
`idempotency_conflict`, `internal`.

## Contract v0.1 (additive, M9) — per-session REPL console

v0.1 adds the `repl.*` surface. Nothing above changes.

**Semantics.** `repl.execute` runs a cell on the session's prime-rlm IPython
kernel through the SAME serialized execution queue the model's `ipython` tool
uses — the user and the model share one namespace; that is the point. A user
cell NEVER starts an agent turn and is NEVER injected into model context (the
host claims the payload at pi's `input` extension event, before pi's
streaming/busy check and before any prompt assembly). `repl.execute` during a
running turn queues at the KERNEL, not the agent — no `session_busy`, no
follow_up/steer semantics. Execution is fire-and-forget: the response acks
queueing; `repl.*` events carry the lifecycle.

**Method.**

`repl.execute {sessionId, code, client_cell_id?}` → `{accepted: true, cell_id}`
· idempotent on `client_cell_id` (bridge idem key `repl:<sid>:<client_cell_id>`;
the returned `cell_id` is deterministic — `u_<sanitized client_cell_id>` — and
the host dedupes, so crash-mid-flight retries never re-run a cell) ·
`code` ≤ 1 MiB · errors: `session_not_found`, `host_down` (host not running —
repl is NOT journaled for respawn; the kernel queue is the ordering point),
`repl_unavailable` (extension not ready on the host), `session_busy`
`{reason:"compacting"}`, `bad_request`.

**Events** (spooled + reconnect-replayable, watermark-contiguous like all v0
events):

| event | data |
|---|---|
| `repl.cell` | `{cell_id, provenance: "user"\|"model", status: "queued"\|"running"\|"done"\|"error", code?, code_truncated?, client_cell_id?, tool_call_id?, position?, queued_at?, started_at?, finished_at?, duration_ms?, error?{ename,evalue}, stdout_truncated?, stderr_truncated?}` — one `queued` then one `running` then exactly one terminal event per cell. `code` echo ≤ 16 KiB. |
| `repl.output` | `{cell_id, stream: "stdout"\|"stderr"\|"display"\|"error", data, mime_type?, truncated?}` — text streams chunked ≤ 16 KiB (split, no loss; stream-level truncation is flagged on the terminal `repl.cell`). `display` carries `{mime_type, data}` for `text/plain` (execute_result + display_data) and `image/png`/`image/jpeg` (base64, ≤ 3 MiB, else `truncated:true` with empty data). `error` carries the traceback text. |

Model cells (the model's own `ipython` tool calls) appear in the same stream
with `provenance:"model"` and `tool_call_id` — the console shows everything
the kernel runs. Host crash / bridge restart closes unfinished cells with a
terminal `repl.cell` `status:"error"` (`HostCrashed` / `BridgeRestarted`).

`session.getState` gains `repl: {active_cell, queue_depth, queued_cells}`.

## Contract v0.2 (additive, M11a) — model surface, subagent drill-down, comms

v0.2 adds the model surface, a per-child transcript reader, and comms
visibility. Nothing above changes.

**Model surface.** `models.list` is an in-bridge probe over the SAME
`ModelRuntime` the pi hosts use (`auth.json` + `models.json` + settings,
network refresh disabled), cached for 15 s with stale-on-error fallback — it
answers what sessions see. The catalog = custom providers (models.json) +
built-ins; `available`/`authConfigured` mark what is selectable (auth check is
credential presence only — expired OAuth still reports configured, exactly
like pi). `defaults` come from `settings.json` (`defaultProvider` /
`defaultModel`).

| method | params → result | notes |
|---|---|---|
| `models.list` | `{refresh?}` → `{models[], providers[], defaults, thinkingLevels[]}` | `models[]` = `{provider, id, name, available, authConfigured, thinkingLevels[]}`; `providers[]` = `{id, name, source, authConfigured, modelCount}` |
| `session.setModel` | `{sessionId, provider, modelId}` → `{provider, modelId, name, thinkingLevel}` | rpc `set_model` passthrough (no busy guard — pi allows mid-turn); emits `session.model` + re-emits `session.state`; persists via pi (`model_change` + settings) |
| `session.setThinkingLevel` | `{sessionId, level}` → `{thinkingLevel, availableLevels[]}` | reconciled via `get_state` (upstream clamps silently); emits `session.model` |
| `session.create` | gains `model?: "provider/id"` | applied before the session row is inserted; failure → typed error, no orphan row; result gains `model` |
| `subagent.transcript` | `{sessionId, childId}` → `{childSessionId, sessionFile, blocks[], replCells[]}` | M8 JSONL v3 tree-walk over the child's session file (`data/sessions/sub-<rlmId8>/…`); load-on-open + client refresh (no fs.watch); `blocks` carry pi message kinds, `replCells` carry the prime-rlm kernel console |
| `comms.list` | `{sessionId}` → `{messages[]}` | merged inbound (session-file `custom_message` entries) + outbound (outbox fold by id, last record wins), ts-sorted; a session that never sent has no outbox → `[]` |

**New events** (spooled + reconnect-replayable like all v0 events):

| event | data |
|---|---|
| `session.model` | `{provider, modelId, name, thinkingLevel}` — emitted after `setModel` / `setThinkingLevel` / `create{model}`; a `thinking_level_changed` host event may carry only `thinkingLevel` (null-field merge semantics) |
| `comms.message` | outbound: `{fromSessionId, fromName, role, receiverName, targetSessionId, message, deliveryStatus, ts}` from the ONE `prime_comms_message` appendEntry per terminal status (sender's session file; `queued` excluded; redrive silent). inbound: `{direction:"in", fromSessionId, fromName, role, message, deliveryStatus:"delivered", ts}` mapped from the `custom_message` `agent_message` the host receives |

**New errors.** `model_not_found` (unknown id — ALSO returned for models from
unauthed providers: upstream gates `set_model` on the AVAILABLE snapshot before
consulting auth; the `models.list` `available` flags are the UI signal),
`model_unavailable` (post-gate auth failure: "No API key for …"),
`subagent_not_found` (unknown childId, or child file not yet flushed right
after admission — retry).

## Resume semantics

- Per-session `seq` is a monotonic high-water mark, persisted in PG
  (`sessions.last_event_seq`) and reloaded across bridge restarts.
- `session.attach {id}` (no `fromSeq`) → live-only at head. Load transcript via
  `session.getState`.
- `session.attach {id, fromSeq: H}` → replays spool rows `H+1..head`
  (flagged `replayed:true`), then live. Replay holds the per-session event
  lock, so live events cannot interleave; a per-connection watermark suppresses
  duplicates.
- If `H+1` was trimmed from the ring (cap 2000/session) → typed `event_gap`
  with `minAvailable`; the client rebuilds state via `session.getState` and
  reattaches at head.

## Durable command journal

Every user-facing command (`prompt`, `steer`, `follow_up`, `abort`) is journaled
in PG **before** send: `pending → acked | failed`. If the host is down, the
command stays `pending` (`{queued:true}`) and is drained in seq order to the
respawned host. If the host crashes after ack but before pi consumed a
steer/follow_up (pi holds those queues in memory only), the row is marked
`maybe_lost` and reported in `session.error.maybeLostQueued` — **not**
auto-replayed (it may have been consumed; replay could double-apply). A JSONL
mirror per session lives in `data/journal/<sessionId>.jsonl`.

pi semantics to know: `follow_up` on an **idle** host only queues the message
in-memory; it does not start a turn. Use `prompt.send` to start work
immediately. (B4 demonstrates the pair: follow_up journaled while down is
flushed by the prompt drained after it.)

## Idempotency

`session.create` / `prompt.send` accept `idempotencyKey`. First execution stores
`sha256(params)` + the response; a retry with the same key returns the stored
response with `replay:true` (same `sessionId` / same journal `seq`, no
double-apply). Same key + different params → `idempotency_conflict`. A retry
arriving after a crash mid-flight (journaled but response not stored) is matched
against the journal and returns the original `seq`.

## Fault model

- **Host crash** (incl. `kill -9`): supervisor detects exit → `host_exited` +
  `session.error(host_crashed)` → respawn with `pi --session <file>` resume →
  `host_respawned` (generation bumped) → journal drain. prime-rlm restores the
  kernel from the post-cell snapshot (`data/sessions/prime/<id>/kernel/`).
- **Bridge death**: hosts exit on stdin EOF (pi shutdown; extension dispose
  flushes a final kernel snapshot). On boot the bridge migrates, reconciles all
  non-closed sessions from PG, respawns hosts (`reason: bridge_restart`), then
  serves. Clients reattach with `fromSeq` for gap-free continuation.
- **Orphaned kernels after `kill -9`**: the old Python kernel may linger until
  its current cell finishes (no dispose runs). P2: track + reap.

## Layout

```
src/config.ts      env config
src/db.ts          pg pool, migrate(), session/spool/journal/idempotency ops
src/host.ts        HostProcess: spawn pi --mode rpc, JSONL stdio, acked rpcs
src/supervisor.ts  session registry, event mapping, respawn, journal drain, tree
src/server.ts      WS upgrade auth + JSON-RPC dispatch + subscription watermarks
src/models.ts      v0.2 ModelRuntime probe (models.list catalog + 15 s cache)
src/agentfiles.ts  session-file readers (v0.2 subagent.transcript / comms.list)
src/routes/        method tables merged by server.ts (repl, mcp, models, subagent, comms, …)
src/index.ts       migrate → reconcileOnBoot → serve
migrations/        SQL (idempotent, schema_migrations-tracked)
test/              client.mjs, harness.mjs, b1..b8.mjs, m9-*.mjs, m11a.mjs, run-all.sh
```
