# prime-harness — continual harness extension for unpatched upstream pi

Milestone M2 of the pi-relay migration. Ports prime-agent's (PA) continual
harness onto the stock `pi --mode rpc` host (0.84.1) — no fork patches.

## What it provides

1. **In-kernel `rlm.harness` CRUD API** — memories, prompt notes, skills,
   subagent-specs; local (session) vs global (cross-session) scope. Python port
   of PA's `rlm/harness.py` (`python/prime-harness-runtime/`), injected into
   every prime-rlm kernel via prime-rlm's bootstrap-contribution seam.
2. **Skills** — markdown + `python_import` skills from `skills/` (bundled) and
   `<agentDir>/skills/` (user-global), rendered into the system prompt and
   pre-imported into the kernel.
3. **System-prompt assembly** — harness section appended in `before_agent_start`
   (chains after prime-rlm's RLM prompt). State files are re-read on EVERY
   prompt build, so post-compaction reinjection is automatic: the compaction
   summary may drop harness content, but the next prompt's system prompt is
   rebuilt from disk.
4. **Full `/refine`** (PA parity, see below) plus the kernel-side `refine`
   skill (`refine.run` / `refine.status`) as a bundled python skill.

## /refine — two-phase pipeline (PA parity)

Ported from PA's `core/refinement.ts` (planning/prompt/validation/apply) and
the orchestration in PA's `core/agent-session.ts` (`refine`, `_planRefine`,
`_applyRefine`, `_runBackgroundPlan`, `_maybeAutoRefine`,
`autoRefineInstructions`).

- **PLANNING** — background `completeSimple` LLM call over the session
  trajectory + current harness state + refinement history. **Never blocks the
  conversation.** Produces the *smallest* CRUD edit set (memory / skill /
  subagent-spec / prompt-note; create-or-update; local or global).
- **APPLY** — fast phase: re-read the target store, apply edits (with
  `baselineState` conflict rejection so kernel `rlm.harness` writes during
  planning are not clobbered), atomic save, append evidence, session custom
  entry. This is the only phase turn entry waits on (`before_agent_start`
  awaits an in-flight apply; file I/O only).
- **Evidence** — every refinement appends a `HarnessRefinementEvent`
  (trigger + outcome) to the state file's `refinements[]`, the session's
  custom entries (`prime.harness.refinement`), and — for global scope —
  `<agentDir>/harness/refinements.jsonl`. Local observability log:
  `<sessionDir>/prime/<sessionId>/refine-results.jsonl` (also records
  auto-review decisions).
- **Rollback by id** — `/refine --rollback <refinement-id>` inverts the
  recorded edits (create→delete, update→restore snapshot, delete→recreate).
- **Kernel API** — `await refine.run(instructions?, global_?)` schedules a
  refinement (returns immediately); `await refine.status()` returns
  `{pending, in_flight}`. Refinement never runs mid-cell; the apply lands at
  the next turn boundary.
- **Auto-refine** — `turn_interval` (default every 25 assistant turns) and
  `compact` triggers, each gated by a background review LLM call
  (`reviewAutoRefine`, PA's AUTO_REFINE_REVIEW_SYSTEM_PROMPT). Cooldown
  20 min. Env overrides: `PRIME_REFINE_AUTO` (default true),
  `PRIME_REFINE_TURN_INTERVAL`, `PRIME_REFINE_COMPACT`,
  `PRIME_REFINE_COOLDOWN_MS`.
- **Invariant** — the base system prompt is immutable; refine only edits the
  harness layer (state files), which the next prompt build re-reads.

### Seam adaptations (upstream pi events stand in for PA fork internals)

| PA fork seam | M2 upstream-pi equivalent |
|---|---|
| start background planning at assistant `message_end` | `refine.run` arrives from the kernel *during* tool execution → planning starts immediately (same phase of the turn) |
| apply at `shouldStopAfterTurn` quiescent boundary | `agent_settled` extension event; if idle already, apply immediately (manual `/refine`) |
| `_waitForRefineIdle` before turns | `before_agent_start` awaits the in-flight *apply* only |
| auto-refine turn counting at assistant message end | `turn_end` event counter |
| compact trigger around compaction | `session_before_compact` sets a flag consumed at the next `agent_settled` |

## State layout

- Local store: `<sessionDir>/prime/<sessionId>/harness/harness_state.json`
- Global store: `<agentDir>/harness/harness_state.json` + `refinements.jsonl`
  (PA-identical file schemas — copy-compatible with PA state)
- Per-session logs: `prompt-builds.jsonl`, `refine-results.jsonl` in
  `<sessionDir>/prime/<sessionId>/`

## Dependencies / load order

- **Soft dependency on prime-rlm**: without it, the prompt section, `/refine`,
  and auto-refine still work (pure TS), but `rlm.harness` and `refine.*` are
  absent from kernels and python skills are not importable.
- Load order in `settings.json` `extensions`: `prime-rlm` BEFORE
  `prime-harness` (so prime-rlm's system prompt is the base this one appends
  to). Registration is lazy/load-order tolerant, but prompt chaining is not.
- Cross-extension interaction goes exclusively through prime-rlm's public
  host API v1 (`globalThis[Symbol.for("prime-rlm.host-api")]`):
  `registerKernelBootstrapContributor` (env + python cells at kernel start)
  and `registerHostHandlerProvider` (`refine.run` / `refine.status`).

## Commands

- `/refine [--global] [--rollback <id>] [instructions]` — full functionality.
  When the session is mid-turn, the request is queued (planning in the
  background, apply at the next boundary); when idle it plans and applies
  before the command returns.
