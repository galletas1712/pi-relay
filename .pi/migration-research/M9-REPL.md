# M9 — per-session IPython REPL console (contract v0.1, additive)

Status: **COMPLETE** — R1–R6 all PASS, b-suite B1–B8 green, web suite 703 green, `npm run build` green.

## What shipped

**prime-rlm extension** (`extensions/prime-rlm/`):
- `src/repl-console.ts` (new): sentinel payload parser (`\u0001prime-repl:` + JSON `{v:1,op:"execute",cell_id,code,client_cell_id?}`), per-session LRU dedupe (256), queued-emit before `provisioner.ensure()`, stdout/stderr coalescing (60 ms flush / phase change / 16 KiB), caps: 16 KiB code echo, 16 KiB text chunks, 3 MiB display images (else `truncated:true` empty data), `execute_result`→`display` text/plain, traceback→`error` stream.
- `src/kernel.ts`: `KernelCellMeta`/`KernelCellEvent`, `ExecuteOptions.cell`, `onCellEvent` hook (running/output/display/error/result/finished), exactly-once finish guard, `pendingCellIds`/`activeCellId` queue snapshot. Internal cells (bootstrap/snapshot/restore/contributions) emit nothing.
- `src/ipython-tool.ts`: model cells announced with `cellId m_<toolCallId>`, provenance "model", before `ensure()`; tool result unaffected.
- `index.ts`: `pi.on("input")` claims sentinel payloads from rpc (`{action:"handled"}` — pi fires `input` BEFORE busy check + context injection, so user cells never start a turn and never enter model context; works mid-turn). Capability marker `prime_rlm_ready` — **deferred via setTimeout (0/100/500 ms) because rpc-mode binds extensions before subscribing the event sink (rpc-mode.js rebindSession); a synchronous appendEntry during session_start never reaches the wire.**

**Bridge** (`packages/bridge/`):
- `routes/repl.ts`: `repl.execute {sessionId, code, client_cell_id?}` → `{accepted, cell_id}`; code ≤1 MiB, client_cell_id ≤200 chars; idem key `repl:<sid>:<client_cell_id>` → `replay:true`.
- `supervisor.ts`: `replExecute` (deterministic `u_<client|uuid>` cell ids, sentinel forward via host rpc `prompt`, NOT journaled), mapHostEvent repl cases (`repl_cell`/`repl_output` custom entries → spool → broadcast), bridge-side event-sourced repl reducer (`noteReplEvent`) → `session.getState` gains `repl:{active_cell, queue_depth, queued_cells}`; unfinished cells terminated as `HostCrashed` (host crash) / `BridgeRestarted` (boot reconcile), spooled so replay sees terminal states. `replReady` control flag from `prime_rlm_ready` (never spooled).
- Errors: `session_not_found`, `host_down {state}`, `repl_unavailable`, `session_busy{reason:"compacting"}`, `bad_request`, `idempotency_conflict`.
- README: Contract v0.1 section.

**Web** (`packages/web/src/bridge/`):
- `replStore.ts` + tests: event-sourced projection (watermark dedupe, terminal stickiness, late-attach cell synthesis, 300-cell trim), `ReplStore` client subscriber, `execute()` always generates `client_cell_id`.
- `components/ReplPane.tsx`: per-session console — provenance badges (you/model), status chips (queued #pos / running spinner / done ms / error ename), ansi-ish stdout/stderr (`components/ansi.tsx`, SGR colors/attrs), png/jpeg `<img>`, collapsed `<details>` tracebacks, autoscroll w/ stick-to-bottom, busy indicator, Shift+Enter, ↑/↓ history (localStorage, 50), per-session state. Toggle in header, default ON, persisted `pi-relay:bridge:repl-pane`.
- `types.ts`/`client.ts`/`useBridge.tsx`/`BridgeApp.tsx` wiring.

## Verification (traces `.pi/m1-demo/traces/m9-*.jsonl`)

| Req | Result | Evidence |
|---|---|---|
| R1 lifecycle | PASS | m9-r1: queued→running→stdout→done ordering, shared namespace (x+1=42), ValueError cell, png display, getState repl block, 20 repl events strictly increasing seqs |
| R2 mid-turn serialization | PASS | m9-r2 (GLM): user cell accepted mid-model-cell, ran only after model done (seq 24 > 23), clean attribution, exactly 1 turn, transcript free of user output |
| R3 reconnect replay | PASS | m9-r3: socket killed mid-cell at watermark W; replay starts exactly W+1, contiguous +1, no dups, all 30 ticks present (union deduped by seq) |
| R4 idempotency + typed errors | PASS | m9-r4: replay:true same cell_id, 1 queued event; idempotency_conflict; session_not_found; bad_request (1 MiB); host_down {state} after SIGKILL; in-flight cell HostCrashed; post-respawn NameError proves fresh kernel |
| R5 model provenance | PASS | m9-r5 (GLM): `m_<toolCallId>` provenance model + code echo; getState active_cell mid-run; 2nd client fromSeq=0 replays model cell from spool (replayed:true) |
| R6 web | PASS | vitest 703 passed (incl. 9 replStore tests), `npm run build` green; m9-r6-dom: headless Chrome via /__bridge-ws — 5 replayed cells rendered (badges, img, collapsed traceback), Shift+Enter cell executed (`r6-dom-dom-proof`), toggle persists |

b-suite B1–B8: all PASS (run-all.sh now includes m9-r1..r5).

## Semantics (documented in bridge README v0.1)

- User cells **never** trigger an agent turn and **never** enter model context; the shared kernel namespace is the point (model cells and user cells see the same globals — proven in R6 DOM proof via `r6v`).
- Mid-turn `repl.execute` queues at the **kernel**, not the agent; `position` surfaced on the queued event.
- repl.execute is **not journaled** (no respawn replay — a half-run cell is terminated HostCrashed instead; the kernel queue is the ordering point).
- v0.1 scope: console attaches to the top session's kernel only (rlm.run child kernels are out of scope — child hosts don't reach this rpc pipe).

## rpc-park: comms/follow-up wakeup gap on idle rpc hosts (2026-08-10)

**Symptom.** `.pi/m1-demo/e2e-comms.mjs` (user repl cell → rlm child → explicit
`agent_message.send(..., receiver_role="parent")`) never ran a follow-up turn on
the parent: the child's message was delivered but the parent parked idle;
step-4 `waitIdleAfter(head2)` timed out (240s). Same hole for bridge
`session.followUp` to an idle host: rpc `follow_up` only enqueues.

**Root cause (upstream pi 0.84.1 rpc semantics, all in
`node_modules/@earendil-works/pi-coding-agent/dist/`).**
- rpc `steer`/`follow_up` on an IDLE host only enqueue into agent-core's
  PendingMessageQueue — nothing drains the queue until an unrelated `prompt`
  arrives (rpc-mode.js). Parked forever otherwise.
- rpc `prompt` acks at **preflight** (run started), not after the run —
  back-to-back prompts fail `session_busy`.
- `_runAgentPrompt` = `agent.prompt()` → `while(_handlePostAgentRun())
  agent.continue()` → `_emitAgentSettled()`: queued steer/follow-up messages
  are drained INSIDE the current run (one settle, no second turn).
  `_isAgentRunActive` flips false before `agent_settled` is emitted, so a steer
  landing in the post-drain window parks with no run left to drain it.
- Extension-side `pi.sendMessage(msg, {triggerTurn:true})` on an idle host
  DOES start a real turn (`sendCustomMessage` → `_runAgentPrompt`).
  `agent_settled` IS emitted to extension handlers (before session events).
- `get_state.pendingMessageCount` counts only `_steeringMessages`/
  `_followUpMessages` (rpc text path) — extension custom deliveries via
  `agent.steer()` are invisible to it.

**Experiments** (`.pi/m1-demo/scratch/exp-idle-trigger.mjs`, raw
`pi --mode rpc --tools ipython` host with bridge-identical args/env, repl
sentinel cells + prompts over stdio JSONL):
- **E1 (idle parent): YES** — child `agent_message.send` →
  `sendMessage({triggerTurn:true})` starts a real parent turn (agent_start +
  agent_settled, assistant acknowledged `child-result:391`). → YES branch.
- **E2 (busy parent, pre-fix):** steer delivered mid-turn is consumed IN-RUN
  (message_start at cell end), ONE settle, **no turn 2** — the e2e's required
  second turn cannot come from "re-issue if unconsumed" alone (the message IS
  consumed). Post-fix E2: deferred delivery → real turn 2 after turn-1 settle.
- **E3 (busy child):** mid-run steer consumed in-run exactly once
  (`E3-REPLY:STEERED-OK`), no park-guard double-fire — m3-r2 semantics intact.

**Fix (YES branch — extension-side + bridge-side queue conversion).**

`extensions/prime-comms/index.ts`:
- Busy-**parent** deliveries are DEFERRED (`deliverAs:"deferred"`): queued in
  `deferredByTarget` keyed by target sessionId; the outbox "queued" record
  stays open (restart-safe re-drive); receipt/listeners see `"queued"`.
- Drain at the target's `agent_settled` (setImmediate so the settle event
  propagates first; re-checks `ctx.isIdle()`): first deferred message
  `triggerTurn` (real follow-up turn), rest steer into it. `session_shutdown`
  fails pending so senders' outboxes terminate. Settle-race guard: defer
  registers, then drains immediately (no-op while busy).
- Busy-**child** steer UNCHANGED (m3-r2 mid-run steering) + park guard:
  `message_start` marks consumption by `details.id`; unconsumed at settle →
  ONE `triggerTurn` re-issue; still unconsumed after a full turn → drop+warn.
- `appendTerminalOutboxRecord`: deferred terminal writes are skipped when the
  sender's session dir was deleted meanwhile (rlm delete_subagent of a child
  whose reply was pending — otherwise mkdirSync resurrects the dir, m3-r3 leak).

`extensions/prime-rlm/src/rlm-host.ts`: message listener treats
`deliveryStatus==="queued"` as repliedToParent (else the completion-notice
fallback double-delivers at child settle).

`packages/bridge/src/supervisor.ts`:
- `effectiveRpcKind`: at delivery time, `follow_up`/`steer` with the host not
  running/compacting → rpc `prompt` (fixes `session.followUp`-when-idle and
  the same steer park hole; busy keeps native enqueue semantics).
- Successful prompt marks `handle.state="running"` immediately (preflight ack
  starts the run) so follow-on journaled entries see busy, not a second
  (rejected) conversion.
- Journal drain pacing: entries that start a run wait for the previous drained
  run's settle (`waitForIdleBeforeRun`, 5-min bound) — rpc prompt acks at
  preflight, so unpaced back-to-back prompts would fail session_busy and be
  marked failed.

`packages/bridge/test/b4.mjs`: drain is now two real turns (converted
follow_up first, paced prompt second) — assertions wait for turn-2 idle.
`.pi/m1-demo/e2e-comms.mjs`: text checks use `collectText` (streamed deltas
split markers, e.g. `OBSIDIAN-CROW-` + `19`).

**Semantics now:** child→parent messages ALWAYS start a real parent turn —
immediately when idle (triggerTurn), at the next settle when busy (deferred).
Parent→busy-child stays mid-run steer. Exactly-once: consumption marking +
outbox open-until-terminal + delivered-marker guards.

**Acceptance (2026-08-10, GLM via shim :8571):**
- e2e-comms.mjs: 7/7 ✓ (incl. CHILD-SAID child-result:391, OBSIDIAN-CROW-19,
  turn-2 idle; parent session file has exactly 1 agent_message entry).
- run-all.sh: b1–b8 + m9-r1..r5 ALL PASS (b4 two-turn drain verified).
- m3-r1/m3-r2/m3-r3 PASS (r2 mid-run steer in one child turn; r3 incl.
  no-subdir-leak after delete-during-deferral fix).
- tsc clean: prime-comms, prime-rlm, prime-harness, prime-autonomy, bridge.

**Known limits:** deferred drain re-checks `isIdle()` only — a manual rpc
`compact` in the settle→setImmediate window could collide with the drain's
triggerTurn (not exercised by any scenario; sendMessage failure finalizes
"failed", never hangs). Park-window repair is best-effort single re-issue.
