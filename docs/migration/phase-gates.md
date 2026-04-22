# Migration phase gates

This file defines the milestone gates for the hybrid Rust/TypeScript runtime
migration. The goal is to move one authoritative state machine at a time while
keeping rollback straightforward.

## Common gates for every phase

Before a phase is considered complete:

- runtime selection remains flaggable and reversible
- replay/parity coverage expands with the new surface area
- targeted tests for touched host/runtime packages pass
- existing persistence formats remain stable unless a later dedicated phase says
  otherwise
- docs are updated to describe any new mode, boundary, or rollback rule

## Phase 0 — Baseline safety rails

**Deliverables**

- architecture and boundary docs
- env-flag plumbing for orchestrator/session engine selection
- parity replay scaffolding and fixture layout docs

**Exit gate**

- defaults remain `legacy`
- no runtime behavior change when flags are unset
- app runtime tests and scaffold scripts pass

**Rollback**

- no-op; this phase is documentation/scaffolding only

## Phase 1 — TypeScript orchestrator seam

**Deliverables**

- initial protocol types for orchestrator-facing core messages
- extracted pure TS orchestrator-core layout
- thinner `packages/orchestrator` host/adaptor layer

**Exit gate**

- orchestrator behavior remains unchanged in `legacy`
- pure reducer/state logic is separated from host-side effects
- representative orchestrator tests still pass

**Rollback**

- keep `legacy` as the only authoritative path

## Phase 2 — Orchestrator parity fixtures and diff normalization

**Deliverables**

- captured orchestrator replay fixtures for representative agent-tree scenarios
- normalized effect/snapshot comparison helpers
- deterministic replay entry points for the extracted TS core

**Exit gate**

- fixture replays are deterministic
- diff noise is normalized enough to make shadow-mode failures actionable
- rollback remains a single flag flip to `legacy`

## Phase 3 — Rust orchestrator shadow mode

**Deliverables**

- Rust workspace scaffold (`agent-protocol`, `orchestrator-core`, host bridge)
- TS bridge/client scaffolding for sidecar communication
- `rust-shadow` plumbing for orchestrator replay in parallel with TS

**Exit gate**

- Rust crates build/test cleanly
- shadow mode can be enabled without becoming authoritative
- parity mismatches are observable without affecting the live result

**Rollback**

- switch back to `legacy` or `ts-core`

## Phase 4 — Rust orchestrator authoritative cutover

**Deliverables**

- Rust orchestrator core capable of driving the live agent tree
- parity suite green on the orchestrator surface
- host integration hardened for restore, routing, and worklog-sensitive flows

**Exit gate**

- `rust` orchestrator mode passes targeted integration coverage
- rollback to `legacy`/`ts-core` remains tested
- no persistence-format change is required for cutover

## Phase 5 — TypeScript session-core seam

**Deliverables**

- extracted pure TS session-core skeleton
- narrow queue/state tests around extracted behavior
- `packages/coding-agent` host still owns tools, prompts, extensions, and persistence

**Exit gate**

- session behavior remains unchanged in `legacy`
- queue/turn/retry/cancel state is isolated behind a clean seam
- targeted session-core tests pass

**Rollback**

- keep `legacy` authoritative for session runtime

## Phase 6 — Rust session shadow mode

**Deliverables**

- Rust session-core scaffold plus replay/shadow bridge
- session parity fixtures for queue, cancel, retry, and background-tool flows
- `rust-shadow` support for session runtime

**Exit gate**

- shadow session replays can diff against TS core deterministically
- high-risk flows (cancel/retry/restore/background work) are covered
- rollback to `legacy`/`ts-core` remains immediate

## Phase 7 — Rust session authoritative cutover

**Deliverables**

- Rust session core authoritative behind `PI_RELAY_SESSION_ENGINE=rust`
- parity and integration coverage for live session flows
- host adapters stabilized around tool execution, extension hooks, and persistence

**Exit gate**

- session `rust` mode passes targeted integration tests
- rollback to `legacy` is verified
- persistence formats are still unchanged unless a separate migration phase begins

## Deferred phase 8+ — Persistence/schema evolution

Only after phases 0-7 are stable should the project consider:

- changing `tree.json` or JSONL schemas
- moving restore snapshots to a new serialized format
- cross-language persistence compaction or binary encoding

That work should be a separate migration track with its own rollback plan.
