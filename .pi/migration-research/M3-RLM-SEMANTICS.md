# M3 — RLM-semantics parity (admission-async rlm.run, steer, delete, kernel snapshots)

Milestone: 2026-08-09 · Model: nvidia/zai-org/glm-5.2 (shim-proxied) · Traces: `.pi/m1-demo/traces/m3-*.jsonl`

## Deliverables (all ✓)

### 1. Kernel state snapshots (dill) — S1
- `src/state-snapshot.ts`: verbatim copy of PA `kernel/state-snapshot.ts` (one-line provenance header).
- `src/kernel.ts`: `KernelSnapshotConfig` + `snapshot` option; successful non-internal cells schedule a
  debounced snapshot (`PRIME_RLM_SNAPSHOT_DEBOUNCE_MS`, default 1500ms); `snapshotState`/`restoreState`/
  `listNamespaceNames` run as internal queue entries (PA parity: `SNAPSHOT_MAX_OUTPUT_CHARS`); SIGINT/
  SIGTERM/beforeExit handlers → `shutdown({snapshot:true})`; `dispose()` flushes a final snapshot
  (`SNAPSHOT_DISPOSE_TIMEOUT_MS=5000`).
- `src/ipython-tool.ts`: per-session snapshot dir `<sessionDir>/prime/<sessionId>/kernel/`; on kernel
  start, `restoreState()` runs BEFORE bootstrap when `kernel-state.dill` exists; restore counts surface
  via the bootstrap-warnings channel on the first tool result (PA "revived on a best-effort basis").
- `src/provision.ts`: venv extras now `nest_asyncio` + `dill`; stamp file records extras so old venvs
  auto-reprovision; smoke test imports dill.
- `rlm.snapshot_save()/snapshot_restore()` kernel APIs: scheduled semantics — return `{scheduled, path}`
  immediately, snapshot/restore executes right after the current cell. (A synchronous in-cell snapshot
  deadlocks by construction: the internal execute would queue behind the very cell that requested it.
  PA never snapshots synchronously either — snapshots are post-cell/dispose events.)

**S1 (full-stack GLM, m3-s1.jsonl):** model set `marker_s1='EMERALD-FALCON-88'`, `nums=[3,1,4,1,5,9,2,6]`
→ debounced snapshot → host SIGKILL → `pi --session <file>` resume → restore-before-bootstrap → model
printed `S1-RESTORED EMERALD-FALCON-88 [3, 1, 4, 1, 5, 9, 2, 6]`. Restore note surfaced:
`kernel namespace restored from snapshot: 2 variable(s); 4 could not be revived` (the 4 are
bootstrap-injected shim modules re-created by bootstrap — PA-consistent best-effort behavior).
Kernel-level mechanics (SIGKILL the kernel process itself) additionally verified in m3-s2.jsonl.

### 2. Busy-kernel queueing — S2
M1's promise-chain queue already matched PA (`enqueueExecute`, no cap). Verified via kernel harness
`.pi/m1-demo/kernel-check-m3.mjs` driving the real KernelManager + demo venv: cell B submitted 1s into
a 6s cell A → B queued, resolved 3ms after A completed, both ok (m3-s2.jsonl). (Sequential tool
execution means the model cannot submit concurrent cells; the queue serves internal executes —
snapshots — exactly as in PA.)

### 3. Admission-async `rlm.run` + result delivery — R1 (+ C3)
- `rlm-host.ts` rewritten: `rlm.run` validates (name/kwargs/depth guard/model selector), creates
  `sub-<id>` dir + registry entry, kicks a detached task, and returns the handle
  `{rlm_child_id, name, session_dir, model}` AT ADMISSION. Blocking API removed (PA is async-only).
- Detached task: createAgentSession → pending depth/parent registration → bindExtensions(rpc) → child
  tracked in `liveChildren` from CREATION (fixes the M1 leak where parent shutdown orphaned running
  children) → prompt `[task from parent]\n\n…` → race first `agent_settled` vs deletion → set registry
  status/result_preview → deliver outcome to the parent IFF the child did not already reply via
  agent_message (tracked through the comms listener; PA `completed_without_reply` doctrine).
- Delivery prefers the new **prime-comms host-api seam v1** (`sendAs(childSessionId, "parent", body)` —
  real outbox record + wakeup); without prime-comms (rlm-only) it falls back to
  `pi.sendMessage({customType: "rlm_child_result", content: "[from child:<name>]\n… Final assistant
  text:\n<full text>"}, {triggerTurn: true})`. The fallback carries the FULL final assistant text
  (bounded 8000 chars), not PA's short preview — documented adaptation: the notice is the only result
  channel in rlm-only mode.

**R1 (m3-r1.jsonl):** GLM parent spawned fib10+fib12 in ONE cell (handles returned immediately),
settled ("children spawned"), received both `[from child:fibN]` wakeups, parsed the values FROM THE
MESSAGES, printed `R1-DONE F10=55 F12=144 SUMSQ=2870` (SUMSQ computed by the parent itself while
children ran). All wakeup messages persisted in the parent session file.
**C3 (m3-c3.jsonl, rlm-only agent dir):** v1 regression after restart, then async v3
(`prompts/v3-async.txt`) — child spawned, fallback `rlm_child_result` notice delivered `144`,
parent printed `FIB12-RESULT 144` and listed the completed subagent. Modularity intact: prime-rlm
works without prime-comms (the comms seam is resolved lazily via globalThis, never imported).

### 4. Multi-turn children / follow-up steer — R2
Child sessions+kernels are retained after their first turn (settle does not dispose), so a living
child can be steered via `agent_message.send(receiver_role="child", …)` — delivered as a pi steering
message when busy, triggerTurn wakeup when idle (M2 machinery).

**R2 (m3-r2.jsonl):** child slept 15s then wrote COUNT-BASE; parent steered mid-run ("also append
COUNT-EXTRA"); the steer was delivered between tool calls of the child's running turn; child wrote
both words and replied `WROTE:COUNT-BASE+COUNT-EXTRA`; parent echoed `R2-DONE WROTE:COUNT-BASE+COUNT-EXTRA`;
`scenarios-out/r2.txt` contains both lines.

### 5. `rlm.delete_subagent` — R3
Resolve by rlm_child_id | session_id | exact name (ambiguous/none → error). Tombstone-first, then
abort+dispose live session (deletion resolves the settle race directly — no reliance on abort() emitting
agent_settled), `disposeSessionState(childSessionId)` kills the kernel, registry entry removed,
session dir `rm -rf` (task requirement; deviation from PA, which keeps transcripts — documented).
Returns the removed entry.

**R3 (m3-r3.jsonl):** quickx (completed) + slowy (sleeping 120s, running) both deleted mid-parent-turn:
`R3-BEFORE [('quickx', 'completed'), ('slowy', 'running')]`, `R3-DELETED-X quickx completed`,
`R3-DELETED-Y slowy running`, `R3-AFTER []`; no `sub-*` dirs left on disk; after a 150s grace window
no session file contains slowy's would-be reply (`Y-DONE`) — the running child was genuinely terminated
and the parent was informed via the deletion outcome (no spurious wakeup).

### 6. prime-comms host-api seam + prompt section
`globalThis[Symbol.for("prime-comms.host-api")]` v1: `sendAs(fromSessionId, role, message,
receiverName?)`, `registerMessageListener(fn)`, `isBound(sessionId)`; every send (success or failure)
notifies listeners with `{fromSessionId, fromName, role, receiverName, targetSessionId, message,
deliveryStatus}`. New `before_agent_start` section documents agent_message/agent_observe, the PA child
reply doctrine ("reply explicitly with `await agent_message.send(message, receiver_role="parent")`"),
and the no-busy-polling rule for parents. Bugfix: `receiver_name: null` from the kernel shim is now
treated like undefined (was rejecting all parent-addressed sends).

### 7. Python runtime (`prime_rlm_runtime`)
`RLMSpawnHandle` dataclass (+ `__getitem__` so both `h.name` and `h["name"]` work); `run()`/`__call__`
return the handle at admission; `RLMSubagent` gains `active_session_id`; `delete_subagent(target)`
accepts id | session_id | name | handle/dataclass; `snapshot_save()/snapshot_restore()`; missing-runtime
stub updated. Module docstring describes the async contract.

## Regressions (all ✓)
V1 (SQUARES_SUM 285), V2 (%%bash BASH_TOKEN_42), V4 (kernel var persistence PERSIST_57),
V5 (depth guard fires at admission under RLM_MAX_DEPTH=0), M2 C1 comms (C1-PONG=RECEIVED under the
new async semantics + new prompt section). Old blocking-API prompts v3.txt + scenarios r1/r2/r3 (M2
refine) are superseded — the blocking API no longer exists (PA parity).

## Driver additions (driver-m2.mjs)
`waitTrace` (poll trace for async wakeup turns), `assertAbsent`, `expectNoTrace`, `assertNoSubdirs`,
`assertDirNoFile`, `{{demo}}` template var. **Assertion-integrity fix:** trace scans now strip `#`-prefixed
driver log lines — previously expectTrace could self-match its own step description. Residual class of
weakness (prompt text is legitimately in rpc traffic): M3 scenario tokens are designed so the asserted
literal never appears in the prompt (values joined/computed at runtime), linted by a scenario check.

## Deviations from PA (all documented adaptations)
1. Fallback result notice carries full final text (≤8000 chars), not PA's preview — it's the only
   result channel without prime-comms.
2. `rlm.delete_subagent` removes the child's session directory (task requirement; PA keeps transcripts).
3. `snapshot_save/restore` are scheduled post-cell operations (sync would deadlock; PA never snapshots
   synchronously either).
4. Snapshot restore lists bootstrap-injected shim modules as "could not be revived" before bootstrap
   re-creates them — cosmetic, truthful, PA-consistent best-effort semantics.
5. Session-id knowledge gap: PA knows child sessionId pre-creation; pi creates it inside
   SessionManager — pending depth/parent maps are keyed after creation, before bindExtensions.

## Traces
| Trace | Scenario | Result |
|---|---|---|
| m3-s1.jsonl | GLM kill/resume kernel restore | ✓ S1-RESTORED exact |
| m3-s2.jsonl | kernel harness: busy-queue + SIGKILL/restore mechanics | ✓ |
| m3-r1.jsonl | 2 async children → wakeups → synthesis | ✓ R1-DONE F10=55 F12=144 SUMSQ=2870 |
| m3-r2.jsonl | mid-run steer | ✓ R2-DONE WROTE:COUNT-BASE+COUNT-EXTRA |
| m3-r3.jsonl | delete completed+running | ✓ registry+disk clean, no zombie reply |
| m3-c3.jsonl | rlm-only + fallback delivery | ✓ FIB12-RESULT 144 |
| m3-v1/v2/v4/v5.jsonl | regressions | ✓ |
| m3-c1.jsonl | M2 comms regression | ✓ C1-PONG=RECEIVED |
