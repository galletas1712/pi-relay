# M2: prime-harness + prime-comms as upstream-pi extensions

Status: **COMPLETE** — all 10 verification scenarios green on real GLM-5.2
requests against an unpatched `pi --mode rpc` host (0.84.1).

M2 builds two NEW sibling extension packages that load alongside prime-rlm
(M1) via prime-rlm's public host API v1 seam
(`globalThis[Symbol.for("prime-rlm.host-api")]`; versioned; lazy lookup →
load-order tolerant). No fork patches; only files under `extensions/`,
`.pi/m1-demo/`, `.pi/migration-research/` were touched.

## Packages

| package | provides | hard dep |
|---|---|---|
| `extensions/prime-harness` | continual harness: `rlm.harness` kernel CRUD, markdown+python skills, system-prompt section w/ post-compaction reinjection, **full two-phase /refine**, auto-refine | soft dep on prime-rlm (kernel side only) |
| `extensions/prime-comms` | `agent_message` / `agent_observe` kernel modules, family-scoped routing, durable JSONL outbox w/ post-restart re-emission | prime-rlm |

Load order (`settings.json.extensions`): prime-rlm → prime-harness →
prime-comms. See each package's README for the dependency semantics.

## /refine: PA parity (upgrade from the original "minimal /refine" plan)

Full port of PA `core/refinement.ts` (1017 lines → `src/store.ts`) plus the
PA `agent-session.ts` orchestration (`src/refine-engine.ts`, 517 lines):

- two-phase pipeline: background LLM planning (never blocks) + fast apply at
  the turn boundary (only phase turn entry waits on)
- smallest-edit planner over trajectory + harness state + refinement history
- evidence: `HarnessRefinementEvent` in state `refinements[]`, session custom
  entries (`prime.harness.refinement`), global `refinements.jsonl`,
  per-session `refine-results.jsonl` (incl. auto-review decisions)
- rollback by id (`/refine --rollback <id>`, inverted edit application)
- kernel skill `refine.run(instructions?, global_?)` / `refine.status()`
  (bundled python skill at `skills/refine/`, pre-imported into kernels)
- auto-refine: `turn_interval` (default 25 turns) + `compact` triggers behind
  the PA review gate (`AUTO_REFINE_REVIEW_SYSTEM_PROMPT`), 20-min cooldown;
  env overrides `PRIME_REFINE_AUTO/TURN_INTERVAL/COMPACT/COOLDOWN_MS`
- invariant: base system prompt immutable; refine edits harness files only

### Seam adaptations (PA fork internals → upstream pi extension events)

| PA fork seam | upstream-pi equivalent used |
|---|---|
| planning starts at assistant `message_end` (mid-turn, background) | `refine.run` arrives from the kernel during tool execution → background planning starts immediately (same turn phase) |
| apply at `shouldStopAfterTurn` quiescent boundary | `agent_settled` extension event (post-run quiescence); if already idle (manual `/refine`), apply inside the command handler after `isIdle` polling |
| `_waitForRefineIdle` gates turn entry during apply | `before_agent_start` awaits the in-flight **apply** promise only (file I/O, sub-10 ms) |
| auto-refine turn counting (`_assistantTurnsSinceAutoRefine` at message_end) | `turn_end` event counter |
| compact trigger wired around `_compact()` | `session_before_compact` sets a flag consumed at next `agent_settled` |
| PA per-session artifact dir | `<sessionDir>/prime/<sessionId>/` (pi's sessionDir is shared across root sessions; documented simplification) |

**No capability was silently downgraded.** Every PA refine behavior has a
working equivalent on stock pi events.

### Candidate upstream seam requests (nice-to-have, not blockers)

1. `ExtensionContext.waitForIdle()` — exists on command contexts only; the
   engine polls `isIdle()` instead. Exposing it on the base context would
   remove the polling loop.
2. A `message_end`-level extension event with a "session continues" hint
   would let background planning start at the exact PA phase even when the
   trigger is not a kernel call (e.g. auto-refine without tool calls). The
   current `turn_end`-based start is equivalent in practice.
3. `ExtensionContext.hasPendingMessages()`-driven gating: PA defers apply
   while the user queue is non-empty; the extension context exposes
   `hasPendingMessages` but not a drain event, so apply-at-`agent_settled`
   can precede a queued user prompt by milliseconds. Harmless (apply is
   idempotent file I/O; next `before_agent_start` picks it up), but a
   pre-prompt hook would make ordering exact.

## Verification (traces in `.pi/m1-demo/traces/`, scenarios in `.pi/m1-demo/scenarios/`)

| test | trace | result | evidence |
|---|---|---|---|
| H1 memory+prompt-note persistence across sessions | m2-h1.jsonl | PASS | session A wrote local memory PERIDOT-QUAIL-72 + global note; fresh-process session B prompt shows exactly the global note (`H1B-PROMPTS=1 MEMORIES=0 TITLE=h1_global_note`) |
| H2 markdown + python_import skills | m2-h2.jsonl | PASS | codeword ZEPHYR-LEDGER-91 echoed from SKILL.md; `demo_python.add(20,22)` → `H2-SUM=42` |
| H3 post-compaction reinjection (decontaminated) | m2-h3.jsonl | PASS | codeword GARNET-HERON-55 file-mediated (never in conversation; `assertCompaction notContains` verified), recalled post-compaction via reinjected harness section (`"memory":1` in prompt-builds) |
| C1 parent↔child messaging | m2-c1.jsonl | PASS | child spawned via `rlm()`, messaged by name, replied to parent → `C1-PONG=RECEIVED` |
| C2 outbox survives kill -9 mid-delivery | m2-c2.jsonl | PASS | 15 s delivery-delay window, SIGKILL before delivery; after restart: outbox re-stamped `"status":"persisted"`, message (BASALT-WREN-33) appended into child session file |
| C3 prime-rlm alone still works | m2-c3.jsonl | PASS | separate `agent-rlm-only` PI_CODING_AGENT_DIR, only prime-rlm loaded; M1 prompts v1/v3 → SQUARES_SUM + FIB12 |
| R1 refine.run full pipeline | m2-r1.jsonl | PASS | model called `refine.run(..., global_=True)`; background plan → apply at boundary; global `harness_state.json` has `r1_trivial_skill` (CINNABAR-FINCH-64); `refinements.jsonl` evidence; fresh session's prompt lists the skill |
| R2 planning non-blocking | m2-r2.jsonl | PASS | `R2-CONTINUED=42925` same turn as refine.run; second prompt answered at 08:25:48.245Z while the memory was applied at 08:25:59.876Z — 11.6 s of planning overlap without blocking |
| R3 rollback by id | m2-r3.jsonl | PASS | `/refine` created `r3_memory`; `/refine --rollback refine_...` removed it (`entries.memory.r3_memory` absent), `rollbackOf` recorded, session custom entries persisted after next flush. (Evidence log legitimately retains the codeword — PA parity: rollback inverts entries, not history.) |
| R4 auto-refine trigger | m2-r4.jsonl | PASS | `PRIME_REFINE_TURN_INTERVAL=1` → review fired on `turn_interval` trigger, logged as `{"type":"auto_review"}`; reviewer correctly declined a content-free session (`shouldRefine=false`) |

Two latent driver bugs surfaced and fixed during R3: scenario `text` fields
and `assertFile/expectTrace/waitFile contains` strings were not
template-expanded (`{{var}}` passed literally). Fixed in driver-m2.mjs; also
added ops `assertCompaction`, `writeFile`, `assertJsonAbsent`.

## Known limitations / notes

- Demo scenarios share the demo agent dir: global harness state accumulates
  (h1_global_note, r1_trivial_skill). Scenarios are written to be robust to
  that, but a clean slate is `rm -rf .pi/m1-demo/agent/harness`.
- Sessions that only run extension commands never flush a session file
  (upstream behavior); R3 forces one real prompt before asserting on the
  session file.
- prime-comms child re-drive after parent restart goes through the outbox
  tier-2 `SessionManager.open().appendMessage()` path (verified in C2);
  tier-1 live redelivery is exercised implicitly by C1/C2 ordering.
