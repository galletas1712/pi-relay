# pi-relay → prime-agent/OMP Migration Research Wiki

> Research base for migrating pi-relay (Rust harness + React web UI) onto a
> prime-agent RLM/IPython core with batteries from OMP, keeping maximal
> modularity (pi-mono-style seams) and the existing React frontend.

**Status: COMPLETE.** All eight research reports finished and verified; synthesis docs authored.

**Reading order for a newcomer** (some pi-relay + pi/RLM familiarity, didn't develop either):
1. [MIGRATION-STRATEGIES.md](MIGRATION-STRATEGIES.md) — §0 pins all terminology in one table;
   §1 the asset inventory; §2 the four paths at a glance; §3–§6 one chapter per path
   (target architecture diagram, component-fate table, pros/cons, risks); §7 side-by-side;
   §8 recommended phasing. Self-contained.
2. [GAP-ANALYSIS.md](GAP-ANALYSIS.md) — the evidence matrix behind every claim (~120 items).
3. [CONTEXT-ENGINEERING.md](CONTEXT-ENGINEERING.md) — what enters every model call and when,
   per codebase, and the target context pipeline.
4. Per-codebase reports below, as needed. ~800 KB total.

**Late additions:** [t3-code-frontend-assessment.md](t3-code-frontend-assessment.md) — can T3 Code
subsume the frontend? (No — but its rendering tech, `@pierre/diffs`/`@pierre/trees`, should be
npm-adopted directly; ~700–900 LOC of T3 glue vendored. Verified against t3code @ 6f69b44.)
All 36 mermaid blocks across the wiki parse-validated with real mermaid 11.16.1.

**Constraint change 2026-08-09:** the frontend contract is NOT frozen — frontend codebase
stays, RPC contract gets redesigned (see MIGRATION-STRATEGIES.md constraint-update box).

**STATUS 2026-08-09: M1 COMPLETE ✓ — M2 STARTED.** Path A′ approved; plan =
MIGRATION-STRATEGIES.md; why = UPSTREAM-ALIGNMENT.md; execution = IMPLEMENTATION-PLAN.md.
**M1 proof:** [M1-DEMO.md](M1-DEMO.md) — unpatched upstream pi 0.84.1 `--mode rpc` +
`extensions/prime-rlm/` (pure extension, zero patches): persistent ipython kernel, %%bash,
in-process `rlm()` children, registry — all verified on real GLM-5.2 requests
(traces: `.pi/m1-demo/traces/v1–v5`). Findings: no fork-forcing issues (P1 usage attribution
remains the only upstream patch); NVIDIA endpoint SSE broken (demo shim only); wiring fact:
in-process children must `bindExtensions({})` themselves; PA latent bug confirmed at
kernel/index.ts:1231 (spread-order status clobber). M2 ✓ (M2-EXTENSIONS.md: prime-harness incl. FULL two-phase /refine + prime-comms durable outbox; 10/10 scenarios green). M3 ✓ (M3-RLM-SEMANTICS.md: async rlm.run + wakeups, multi-turn steer, delete, dill snapshots kill-9-verified, busy-queue; 11 traces). M4 ✓ (M4-AUTONOMY.md: prime-autonomy — goals/heartbeats/autonomous/compact + 10 skills + observability; 14 traces). **EXTENSION LAYER COMPLETE: every FEATURE-PARITY.md row ✓, all verified on real GLM-5.2 against unpatched upstream pi 0.84.1.** M5 ✓ (M5-BRIDGE.md: packages/bridge — supervisor spawn/respawn/resume, PG control plane, WSS, slim contract v0; B1–B8 green incl. kill-host kernel-restore TOPAZ-OWL-77 + kill-bridge reconcile JADE-FALCON-42). M6 ✓ (M6-FRONTEND.md: packages/web/src/bridge/ — contract-v0 data layer behind ?backend=bridge profile, legacy default untouched; pierre diff/tree views w/ vendored glue + mock workspace seam; F1–F6 green incl. mid-stream resume + 32 unit tests). M7 ✓ (M7-ROLES.md: roles+skills parity — 9 pi-relay roles seeded, role= spawn w/ model/effort/maxTokens override + preloaded kernel skills + role catalog in prompt, harness 'role' kind CRUD, origins precedence harness>project>global; R1-R4 green on GLM+DeepSeek; PLUS prompt-parity restorations: conversation-log line, preinstalled-pkgs, delegation block, model-facing-text conventions). M8 ✓ (M8-WORKSPACE-MCP.md: workspace-lib btrfs pkg + real workspace.*/project.* + kernel-mediated MCP (mock-verified; real OAuth logins = owner checklist) + titles + trimmed-spool rebuild + live-base boot guard + routes split; W/M/G all green, b1 re-verified by orchestrator). M10a ✓ (M10-MIGRATOR.md: 5159 sessions chain-hash verified, boot proof) + M10b ✓ (M10B-CACHE.md) + DOGFOOD RIG live (DOGFOOD.md). M9 ✓ (M9-REPL.md: contract v0.1 repl.execute + repl.cell/output spool events user+model provenance + ReplPane w/ ansi/png/badges; R1-R6 + b-suite green; orchestrator e2e found rpc-park comms gap → comms-drain-fix child). Next: comms-drain-fix → M10 migrator (also: bridge transcript-rebuild-from-session-file for trimmed spools; pi-relay managed project/workspace instruction scopes) → M9 migrator (vs restored pgdump copy; .pi/backups/20260809-112741/) → M9 soak+cutover. **Owner directive: port EVERYTHING, verify EVERYTHING** — master checklist: [FEATURE-PARITY.md](FEATURE-PARITY.md) (M1✓ → M2🔨 → M3 rlm-semantics → M4 autonomy/observability).

**Upstream-alignment audit (2026-08-09, COMPLETE):** [UPSTREAM-ALIGNMENT.md](UPSTREAM-ALIGNMENT.md)
synthesizes [upstream-divergence.md](upstream-divergence.md) (per-package fork deltas; pi-ai +
pi-agent-core thin enough to track upstream, pi-tui droppable, coding-agent the real fork) and
[upstream-seams.md](upstream-seams.md) (all PA differentiators fit upstream extension/RPC seams;
zero fork-forcing residue; patches P1/P2 only) into **Path A′**: upstream `--mode rpc` hosts +
prime-rlm/prime-harness/prime-comms extension packages + supervisor-as-client — no fork.

### TL;DR of the recommendation

**Strategy A — "Façade over prime-agent"**: keep the prime-agent daemon as session runtime;
add one new TS **bridge** service that terminates the browser's WSS exactly like pi-agentd
today (54-method contract, 34-event vocabulary, idempotency keys) and drives prime-agent via
its versioned daemon protocol (`DaemonClient`). Postgres stays as the control-plane store
(durable queue, projects, events spool, delegation control); transcripts migrate one-shot to
pi-mono **v4 JSONL** (written through the storage seam → relational upgrade path stays open).
Enter through the **harness-seam strangler** (`metadata.harness=true` + `harness.model.complete/fail`)
to validate the RLM core against real sessions before any cutover. btrfs becomes top-level
per-session via a new Python workspace-lib; MCP via PA's integration + bridge inventory/OAuth
RPCs; roles/skills port to the continual harness. Details: MIGRATION-STRATEGIES.md §1–§3.

## Codebase reports

| Report | Scope |
|---|---|
| [pi-relay-rust-backend.md](pi-relay-rust-backend.md) | Current Rust backend: 12 crates, wire contract, delegation, workspaces/btrfs, context engineering |
| [pi-relay-frontend-contract.md](pi-relay-frontend-contract.md) | React UI: full backend contract the frontend requires |
| [prime-agent-deep.md](prime-agent-deep.md) | prime-agent: RLM runtime, daemon, embeddability, context engineering |
| [pi-mono-deep.md](pi-mono-deep.md) | Original pi harness: package seams, client/protocol/server, extensions |
| [omp-deep.md](omp-deep.md) | OMP code architecture: crates/packages/wire/natives |
| [omp-batteries.md](omp-batteries.md) | OMP batteries: memory, TTSR, LSP/DAP, plan mode, subagents + context engineering |

## Cross-cutting analyses

| Report | Scope |
|---|---|
| [bridge-transport-options.md](bridge-transport-options.md) | How the React frontend can drive a prime-agent core (WS bridge, ACP, SDK, adapter) |
| [storage-strategy.md](storage-strategy.md) | Postgres vs JSONL session storage; one-shot migration plans |

## Synthesis (parent-authored)

| Doc | Scope |
|---|---|
| [GAP-ANALYSIS.md](GAP-ANALYSIS.md) | Feature-by-feature gap matrix: pi-relay needs × prime-agent/OMP/pi-mono coverage |
| [MIGRATION-STRATEGIES.md](MIGRATION-STRATEGIES.md) | Candidate target architectures + phased migration plans |
| [CONTEXT-ENGINEERING.md](CONTEXT-ENGINEERING.md) | Cross-codebase comparison of context assembly, progressive disclosure, inter-agent visibility, continual learning |

## Reference material

- `.pi/prime-agent-architecture.md` (prior research, being validated/extended by prime-agent-deep)
- `.pi/t3-code-research.md` (prior UI research)
- `repos/` — shallow clones of pi-mono, oh-my-pi, prime-agent
- `M11B-FRONTEND-PORT.md` — legacy-design port completion record (contract v0.3, adapter architecture); `M11B-ACCEPTANCE.md` — 22-item owner-requirement inventory, all verified ✅ 2026-08-10
