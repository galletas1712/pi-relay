# Hybrid runtime boundary

This document fixes the boundary for the phased Rust/TypeScript migration.
Milestone 0 is intentionally conservative: it documents the contract, adds
parity-harness scaffolding, and wires engine-selection flags without changing
the default runtime path.

## Goals

- Extract smaller, state-owning cores that are easier to reason about locally.
- Keep rollback cheap while the new cores are immature.
- Preserve existing TypeScript product surfaces until parity is proven.
- Avoid mixing runtime-core replacement with persistence-format churn.

## Non-goals for phases 0-7

- No repo-wide Rust rewrite.
- No new plugin ABI for extensions or tools.
- No changes to `tree.json`, JSONL session history, worklog files, or other
  on-disk formats.
- No movement of prompt assembly, model/provider glue, or TUI/app composition
  into Rust.

## Ownership map

| Area | Authority during migration | Notes |
| --- | --- | --- |
| `packages/app`, `packages/tui` | TypeScript | User-facing host/runtime composition stays in TS. |
| `packages/orchestrator` host | TypeScript | Keeps file I/O, worklog persistence, UI hooks, and supervisor integration. |
| `packages/coding-agent` host | TypeScript | Keeps prompt assembly, extension loading, tool execution, and persistence adapters. |
| `tool/tool-kit` | TypeScript | Remains the stable SDK/tool boundary; do not fold it into the runtime core. |
| `orchestrator-core` | TS first, Rust later | Owns agent-tree lifecycle, parent/child routing, pending spawn bookkeeping, and supervisor state transitions. |
| `session-core` | TS first, Rust later | Owns queue/turn lifecycle, retry/cancel state, pending tool bookkeeping, and compaction decisions. |

## Boundary rules

1. **Closed state machines only.** Rust is reserved for state-owning cores with a
   single authority over mutable runtime state.
2. **Protocol, not callbacks.** The TS↔Rust boundary must use serialized
   commands, events, effects, and snapshots. No callback-heavy object graphs and
   no shared mutable ownership across the boundary.
3. **TS executes effects.** The core may decide *what* should happen, but the TS
   host still performs dynamic work such as tool execution, extension dispatch,
   model calls, subprocess integration, and persistence writes.
4. **Effects return as events.** Results of host-side work are fed back into the
   core as explicit events so replay and shadow-mode diffing stay deterministic.
5. **Persistence stays stable first.** Phases 0-7 preserve existing on-disk
   formats to keep rollback simple and to isolate runtime-core risk from data
   migration risk.

## Core protocol shape

The boundary is organized around four payload classes:

- **Commands** — intentional requests into a core, e.g. `startRoot`,
  `spawnChild`, `queueUserMessage`, `abortRun`, `markBackgroundToolPending`.
- **Events** — facts observed by the TS host, e.g. tool completion, session
  restore data loaded, child exited, provider stream finished, user cancelled.
- **Effects** — decisions emitted by a core for the TS host to execute, e.g.
  create a child session, persist a worklog entry, append a runtime message,
  dispatch a tool call, or subscribe/unsubscribe from a host hook.
- **Snapshots** — normalized serializable state for replay, shadow-mode diffing,
  diagnostics, and eventual restore handoff.

## Engine modes and flags

Milestone 0 reserves two env vars in `packages/app/src/runtime.ts`:

- `PI_RELAY_ORCH_ENGINE`
- `PI_RELAY_SESSION_ENGINE`

Accepted values:

- `legacy`
- `ts-core`
- `rust-shadow`
- `rust`

Both flags default to `legacy`. In Milestone 0, non-legacy values are accepted
and recorded, but they do **not** change the active runtime path yet.

### Intended meaning of each mode

- `legacy` — current production TypeScript implementation is authoritative.
- `ts-core` — extracted pure TypeScript core is authoritative; host/adaptor code
  stays in TS around it.
- `rust-shadow` — Rust core replays the same inputs in parallel for parity
  comparison while the TS implementation remains authoritative.
- `rust` — Rust core becomes authoritative behind a rollback flag.

## Parity harness expectations

Replay fixtures live under `testdata/parity/` and are split by surface:

- `testdata/parity/orchestrator/`
- `testdata/parity/session/`

Phase 0 only provides fixture discovery/validation scaffolding. Later phases use
these fixtures to replay normalized command/event streams and compare emitted
effects and snapshots between implementations.

## Invariants to preserve

The migration must not regress these behaviors:

- agent-tree lifecycle and status transitions
- parent/child message routing and pending-spawn bookkeeping
- session queue ordering and turn boundaries
- cancellation, retry, and background-tool bookkeeping
- restore/resume behavior and worklog generation flow
- persistence ordering for assistant, tool-result, and custom runtime messages

## Explicitly deferred work

- persistence/schema evolution after runtime parity is stable
- performance-focused native rewrites outside the extracted cores
- moving tool/provider/extension ecosystems behind a native ABI
