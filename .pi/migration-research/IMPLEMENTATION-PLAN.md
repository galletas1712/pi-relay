# Implementation Plan — Week 0/1 (started 2026-08-09)

> Spine: `MIGRATION-STRATEGIES.md` (Path A′). This file is the actionable
> checklist. Rule from AGENTS.md: **nothing in the live pi-relay deployment is
> deleted until Phase 4** — cutover happens by migration script + frontend
> profile switch, and the Rust backend stays authoritative for old sessions
> until then. The "mass deletion" is real but SEQUENCED (see §4).

## 1. Where the new code lives (proposed — confirm before scaffolding)

In the pi-relay repo, additive only:

```
pi-relay/
  rust/                       # LEGACY — untouched until Phase 4 deletion
  packages/
    web/                      # existing SPA — data-layer redesign later
    bridge/                   # NEW TS: WSS ↔ rpc-mode supervisor (the product process)
  extensions/                 # NEW TS npm packages (loaded by upstream pi hosts)
    prime-rlm/                #   ipython tool + KernelManager + host bridge + RLM children
    prime-harness/            #   memories/skills//refine/menu/goals
    prime-comms/              #   agent_message/agent_observe + durable outbox
  workspace-lib/              # NEW Python package: btrfs top-level per session, multi-dir
  scripts/migrate/            # W1 one-shot migrator (PG → v4 JSONL), run once, then delete
```

Upstream consumed as pinned npm deps: `@earendil-works/pi-coding-agent`,
`pi-agent-core`, `pi-ai` (+ P1/P2 patches carried locally until upstreamed).

**Decisions locked (2026-08-09):** PA code extraction = plain copy with a one-line
provenance comment per file (source repo@version + path); no git history preservation needed.
Milestone order: **M1 = prime-rlm (kernel tool + rlm children) on unpatched upstream pi,
verified on real GLM-5.2 requests (DONE, see M1-DEMO.md) → M2 = prime-harness
+ prime-comms extensions (IN PROGRESS, child `m2-extensions`) → then bridge/supervisor (S3)**.
**M3 = RLM-semantics parity (scheduled after M2; M1 deliberate cuts that are load-bearing):**
(a) admission-async `rlm.run` — PA returns a handle at admission and delivers the child's
result later as a wakeup message; M1's blocking version can't express "spawn N children and
keep working". Needs M2's comms layer for result delivery.
(b) kernel state snapshots/restore (dill) — the crash-resume-with-variables story depends on
it; pairs with supervisor respawn in the bridge milestone.
(c) busy-kernel queueing — M1 errors when the kernel is busy; PA queues. Matters once
steer/messages can arrive mid-cell.
Smaller M1 cuts, acceptable for now (revisit if needed): fork-server fast provisioning,
attachments/MIME rendering, bootstrap lock + skill-sync extras, harness/mcp/find_models
host handlers (harness lands in M2; mcp/find_models with the bridge).

**P1 usage attribution: DEFERRED by owner (2026-08-09)** — do not build; child spend stays
in child session files for now. P1 spec remains in M1-DEMO.md/upstream-seams.md for whenever
cost attribution matters again. Durable queues stay on plan: agent-message outbox in M2
(prime-comms, kill-9-verified), steer/follow-up durability via supervisor journal at the
bridge milestone; P2 upstream patch optional.

**Refine scope (owner decision 2026-08-09): FULL PA-parity /refine, not minimal** — two-phase
background-plan/turn-boundary-apply, trajectory-reading smallest-edit planner, evidence
recording, rollback-by-ID, `refine.run`/`refine.status` kernel API, `/refine` command,
auto-refine triggers; port refinement.ts (1017 lines) wholesale. If two-phase wiring hits
a fork-internal dependency, document as an upstream seam request; do not downgrade. De-risk PA-functionality-on-upstream
fully before building the product process.

## 2. Phase 0 spikes (days; nothing production-facing)

| # | Spike | Proves | Output |
|---|-------|--------|--------|
| S1 | `prime-rlm` skeleton in unpatched pi: register `ipython` tool, boot a kernel, run a cell | extension seam carries the kernel | throwaway demo + notes |
| S2 | RLM child spawn from inside a cell via aliased SDK `createAgentSession`; usage attribution gap → spec **P1** exactly | RLM without fork | P1 patch text |
| S3 | Supervisor drives 2 `pi --mode rpc` hosts; `kill -9` mid-turn; snapshot/journal resync | rpc-mode as load-bearing transport | supervisor skeleton |
| S4 | Durable queue via extension outbox; spec **P2** | steer/follow-up survive restarts | P2 patch text |
| S5 | Provider parity: Codex-subscription + Claude-OAuth request shapes old-vs-new (incl. W2 cache bundle hooks) | no provider regressions | parity diff doc |
| S6 | workspace-lib btrfs clone/materialize from Python | A6 port | workspace-lib v0 |
| S7 | B0: drive one scratch pi-relay session through the `metadata.harness` seam | strangler entry route | go/no-go for Phase 1 |

**M5 = bridge/supervisor (STARTED 2026-08-09, child `m5-bridge`).** Owner decisions locked:
control-plane store = **Postgres**; transport = slim product JSON-RPC now (pi-protocol later);
test PG = NEW container `pi-relay-bridge-test-pg` @ 127.0.0.1:56432 ONLY — live
`pi-relay-postgres` @ 55432 and `infra-control-1` are strictly off-limits. Scope: supervisor
(spawn/respawn/resume rpc hosts, journal, event spool), WSS (Origin/token/8MiB), slim
contract v0, PG control-plane schema, B1–B8 verifications incl. kill-host + kill-bridge
durability and idempotency.

**M8 migrator scope (owner decision 2026-08-09): build + verify against a RESTORED COPY only;
migrating old sessions on the live deployment is DEFERRED to cutover.** Backup captured:
`.pi/backups/20260809-112741/` — `pi_relay.dump` (2.9GB custom-format pg_dump of the 5.7GB
live DB, pg_restore-verified, 12/12 tables) + `pi_relay_e2e.dump` + SHA256SUMS. M8 restores
this dump into the TEST container (pi-relay-bridge-test-pg @ 56432) and verifies the migrator
there. The live `pi_relay` DB is never a migration source until final cutover.

**Milestone map (updated 2026-08-09):** M1–M4 extensions ✓ · M5 bridge ✓ · M6 frontend data
layer (running) · **M7 subagent roles+skills port (running — pi-relay SubagentRole parity:
role SKILL.md files w/ model/effort/max_tokens frontmatter, role-configured child prompts,
preloaded skills, role catalog in parent prompt)** · M8 workspace-lib (btrfs) + MCP/OAuth
parity (un-stub workspace.*/mcp.*) ✓ (2026-08-10: W1–W3, M1–M3, G1–G2 all PASS; .pi/migration-research/M8-WORKSPACE-MCP.md) · M9 REPL interface (NEW — owner request 2026-08-09: full per-session IPython console in SPA; UPGRADE over pi-relay which never had one. Contract v0.1: repl.execute provenance:user via kernel queue; repl.* spool events; model cells already flow as tool blocks. Fires after M8 — shares bridge server.ts/host.ts). Then M10 migrator vs restored pgdump copy · M10 soak + cutover
→ Phase-4 deletion. Also scheduled: **W2 cache-parity bundle** (CONTEXT-ENGINEERING §10.2:
deep breakpoint, per-session cacheRetention plumbing, attribution fingerprint, compaction
cache-isolation cherry-pick 9b3a2059, count_tokens gate) lands in M9 hardening (pre-soak —
it decides soak cost). **Bridge product gaps to fold into M8:** projects (session grouping —
pi-relay frontend has them; not in contract v0), session titles (sidecar model call),
workspace search. **Provider watch-item:** NVIDIA SSE streaming broken endpoint-wide (shim is
demo-only) — prod needs upstream fix, a different endpoint, or a supported non-stream path;
decide before cutover. Post-cutover nice-to-haves: P2/seam PRs upstream, pi-protocol transport
revisit, OMP Tier-1 batteries, harness-v2 adoption. Roles were flagged by the owner as a missing pi-relay feature — correct;
the M1–M4 stack had subagent-spec CRUD but rlm.run did not consume roles.

## 3. Build order after spikes (Phase 1–2 highlights)

1. bridge/supervisor skeleton: WSS termination (Origin check, 8MiB), spawn/respawn
   rpc hosts, journal + snapshot resync, event spool
2. W1 migrator against COPIES of prod PG (never prod): topological, id-remap
   sidecar, verify by context-reconstruction diff
3. prime-harness extraction (memories/skills//refine/menu/goals)
4. prime-comms + durable queue (outbox → P2)
5. New slim frontend contract v0 (sessions/prompt/steer/subagent-tree/workspace/
   MCP-inventory) + SPA data-layer branch behind a profile
6. MCP OAuth parity matrix (W5) — Slack/Linear/Outlook/NVCarPs before any cutover

## 4. The mass-deletion schedule (what dies, when)

| Wave | Target | When | Gate |
|---|---|---|---|
| D0 | **Nothing in pi-relay/rust.** Deleted only: PA-donor code we don't take (pi-tui fork, TUI modes, ACP, daemon protocol) — during extraction, in the NEW packages' provenance, not in the live PA install | during extraction | — |
| D1 | `rust/crates/agent-tools`, `agent-prompt`, `agent-provider` internals subsumed by upstream+kernel | Phase 3, after feature-parity suite green | W3 green |
| D2 | pi-agentd, agent-core/session/store/vocab, agent-mcp*, agent-runtime* + legacy PG tables | Phase 4, after W1 migrator verified on all old sessions + soak | cutover checklist |
| D3 | (maybe) pi-runtime kept ONLY if multi-host survives decision #1 | Phase 4 | decision #1 |

**Live-PA caution:** the running prime-agent install (`~/.npm-global/...`) is
what powers the agent doing this migration — never edit it in place; develop
against the repo + pinned npm builds, install only at tagged checkpoints.
