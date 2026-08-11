# M4 — Autonomy + Observability (prime-autonomy)

Milestone M4 closes every remaining row of FEATURE-PARITY.md: G1 thread goals,
G2 rlm heartbeats, G3 autonomous mode, G4 kernel compact API, A1 MIME-rich kernel
results, R4 `rlm.find_models`, the remaining content skills, and the O1/O2/O3
observability events — all as **pure pi extensions on unpatched upstream pi
0.84.1**. No pi fork, no daemon.

## What was built

### `extensions/prime-autonomy` (new)

| File | Ports |
|---|---|
| `index.ts` | Extension factory: 10 CLI flags, `/goal` + `/autonomous` commands, per-session runtimes, the settle pipeline, host-handler provider registration via `Symbol.for("prime-rlm.host-api")` v1 |
| `src/goals.ts` | PA `core/goals.ts` full port: `thread_goal_state` custom entries, `goal_context` custom messages (objective/budget_limit/continuation kinds), token+wall-clock budget accounting with per-turn dedup, budget_limited one-shot delivery, pause/resume/clear, `goal.get/create/complete` host handlers with PA's exact rejection errors |
| `src/autonomous.ts` | PA `core/agent-session-autonomous.ts` port: 8 flags, continuation prompts, quality gates with bounded output (6000 chars), skip-if-workspace-unchanged via git snapshot (PA's exact pathspec excludes + untracked-file hashing), retry/maxRetries, limits (continuations/turns/tokens/timeout), detached-spawn group kill |
| `src/heartbeats.ts` | PA `core/cron-jobs.ts` + `daemon-mode.ts` heartbeat semantics: per-session JSON store, interval schedules, single-timer scheduler, steer/follow_up delivery, `rlm_heartbeat.list/create/update/delete` host handlers |
| `src/compact.ts` | `compact.status` (tokens/context_window/percent/scheduled) + `compact.run` (schedule at turn end) host handlers |

### `extensions/prime-harness/skills/` (+10)

All PA python skills copied verbatim (with `PROVENANCE.md`): edit, attach-image,
websearch, linear, notion, prime-intellect, skill-creator, goal, compact,
rlm-heartbeat. They auto-load via the M2 bundled-skills rule and call
`from rlm import host_request` — covered by the stage-1 `sys.modules["rlm"]`
alias. Verified listed in the assembled system prompt (jiti `loadHarnessSkills`
+ `formatSkillsForPrompt`) and importable in the kernel (m4-skills.jsonl).
`agent_message`/`agent_observe` stay owned by prime-comms (M2 prompt section).

### `extensions/prime-rlm` (stage-1 upgrades)

- kernel.ts: parses `application/vnd.prime-agent.{diff,attachment,agent-message}+json`
  from `display_data` (A1/O2 wire formats), caps/oversized handling
- ipython-tool.ts: ImageContent blocks in tool results for attachments
- index.ts: `ipython_sent_agent_message` custom session entries (O2)
- rlm-host.ts: `rlm_child_lifecycle` custom session entries (admitted/completed/
  error/deleted) (O1), `rlm.find_models` + `model.info` host handlers (R4)
- python runtime: `rlm.find_models`/`RLMModel`, `sys.modules["rlm"]` alias,
  lazy MCP `__getattr__`, ported `mcp_base.py`
- provision.ts: EXTRA_REQUIREMENTS += nest_asyncio/dill/httpx/Pillow

## Verification (all green, traces in `.pi/m1-demo/traces/`)

| Scenario | Proves |
|---|---|
| m4-smoke | all 4 extensions load on upstream pi |
| m4-g1-goals | goal set → `goal_context` continuation re-prompt across turns → work done → `goal.complete()` → **loop stops** (0 injections after mark); `thread_goal_state` persisted |
| m4-g1b-budget | `token_budget=100` → budget crossing at message_end → budget_limit context (kind=budget_limit only, no continuation) → session file records `budget_limited` |
| m4-g1c-resume | SIGKILL host → resume `--session` → `goal.get()` = active → re-prompting resumes |
| m4-g1d-flag | `--goal` + `--goal-token-budget` CLI seeding on fresh branch; context rides first turn; completes; stops |
| m4-g2-heartbeat | `rlm_heartbeat.create(interval="every 15s", delivery_mode="follow_up")` fires HB-TICK while idle; `delete` → silence for 25s |
| m4-g3-autonomous | `--autonomous --autonomous-gate=test -f …`: gate attempt 1 fails → continuation with bounded failure text → model fixes → gate passes → no more continuations |
| m4-g3b-cap | always-failing gate + `--autonomous-max-continuations=2` → exactly 2 continuations, then hard stop |
| m4-g4-compact | `compact.status()` fields; `compact.run()` scheduled → `compaction_start` (manual) at settle → compaction entry in session file |
| m4-a1-attach | attachment display → `{"type":"image"}` ImageContent block in tool result; attach-image skill refuses non-vision model (PA parity) |
| m4-r4-models | `rlm.find_models()` / `("glm")` from kernel against models.json |
| m4-skills | 6 skills imported in one cell; `edit.run` round-trip edits file |
| m4-o1-o2 | child spawn → `rlm_child_lifecycle` phase=admitted entries; parent→child send → `ipython_sent_agent_message` in both session files |
| m4-o3 | `rlm.harness.record_refinement` → `rlm.get_harness_state().refinements` queryable |
| m3-r1 (regression) | async fanout unchanged: F10=55 F12=144 SUMSQ=2870 |

Driver additions (`driver-m2.mjs`): `flags` on start steps (verbatim argv tokens,
use `--name=value`), `waitIdle`, prompt retry-on-busy, settle-count-based prompt
waits (stale `agent_settled` from harness turns can't resolve a wait early),
`markTrace` + `expectTraceCount` with `sinceMark` (count injections by their
`message_start` prefix — `agent_end` echoes full message lists), `waitFile`
without `contains`.

## Extension-vs-PA deltas (documented, all forced by unpatched pi)

1. **Continuations are settle-driven.** PA hooks `_getContinuationMessages`
   (unreachable). Ours: on `agent_settled` — pending compaction → owed
   budget_limit → goal continuation (`sendMessage(triggerTurn)`) → autonomous
   continuation (`sendUserMessage`). Reentrancy-guarded.
2. **`--autonomous-gate` is single-valued** (pi flags are a last-wins Map).
   Chain commands with `&&` — the gate is a shell string anyway.
3. **Post-compaction resume**: pi extensions can't `agent.continue()`; when the
   last context message isn't assistant we send "Compaction complete. Continue
   where you left off." (same condition as PA's
   `_continueAfterThresholdCompaction`).
4. **Queued goal contexts can't be recalled** (no pi API); `/goal pause` stops
   future injections instead of removing queued ones (PA removes queued ones).
5. **Heartbeats are interval-only** ("every 30s/5m/1h", min 10s), stored per
   session at `<sessionFile>.heartbeats.json` instead of PA's daemon-level
   proper-lockfile store. No cron expressions/one-shot/run-now. Deferral parity:
   compacting or pending messages defers; steer interrupts a busy turn,
   follow_up waits.
6. **`compact.run` pre-check** replicates pi's `prepareCompaction` falsy
   conditions ("already compacted", "session too short") because pi's
   package.json exports don't expose it.
7. **O1 events are custom session entries**, not new AgentSessionEvent types
   (pi extension API can't extend the event union) — rpc-visible through
   get_state/session files rather than the live event stream.
8. **prime-comms shim normalizes receipts** to PA's sent-message display shape
   (`deliveryStatus` delivered|queued, `target.activeSessionId`); failed sends
   emit no display (kernel parser stays PA-exact).
9. **Autonomous gate processes**: `spawn(detached)` + `process.kill(-pid,
   SIGKILL)` group kill instead of PA's `killProcessTree`/
   `trackDetachedChildPid` shell utils.
10. **Goal token dedup** via a 512-capped Set (PA
    `_goalAccountedAssistantMessages` parity); budget_limit delivered once per
    crossing.

## Fork-forcing candidates (none adopted)

- Live `AgentSessionEvent` types for child lifecycle (we used custom entries).
- `agent.continue()` post-compaction (we send a user message).
- Recalling queued nextTurn messages on pause (we just stop injecting).

Run everything: `cd .pi/m1-demo && ./run-m2.sh scenarios/m4-<name>.json m4-<name>`
