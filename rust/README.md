# Rust agent stack

Rust implementation of pi-relay's agent runtime. See
[`docs/architecture.md`](docs/architecture.md) for the comprehensive plan,
and `docs/worklog-design.md` / `docs/cost-aggregation.md` for feature-specific
deep dives.

## Crate layout

| Crate | What it owns |
|---|---|
| `agent-core` | Pure deterministic FSM for agent turns. Emits TranscriptRecord events and AgentAction side-effects. No I/O. Internals (`AgentState`, `Mailbox`) are private. |
| `agent-session` | Durable session history atop the core FSM. Owns the SessionLog (DAG of entries with branch-aware navigation), the materialized Transcript view, the AgentRunner, and boundary operations (compact, rewind, fork, replace_transcript) behind a SessionBoundary view type. |
| `agent-orchestrator` | Composition struct for the runtime. Currently owns a SessionRegistry that tracks session identity and spawn relationships. Grows as ModelProvider, ToolRegistry, UsageLedger, AgentWorklogStore land. |

## Layer discipline

`agent-core` ◂─ `agent-session` ◂─ `agent-orchestrator`

Each crate depends only on crates below it. Narrow public APIs between
crates; implementation details stay private. See the architecture doc for
the full layer stack including the future control-plane and view layers.

## Status

PR #63 lands the session/orchestrator crates and the data-model
abstractions. Downstream PRs add (in rough order): SessionStore,
ControlPlane, SessionEvent stream, ModelProvider, Tool/ToolRegistry,
Compactor, UsageLedger, multi-agent primitives (spawn/report/idle),
AgentWorklogStore, PromptAssembly, daemon + remote control plane,
distributed sessions. See the architecture doc's sequencing table.

## Design docs

- [`docs/architecture.md`](docs/architecture.md) — overall plan
- [`docs/worklog-design.md`](docs/worklog-design.md) — per-agent knowledge side-store
- [`docs/cost-aggregation.md`](docs/cost-aggregation.md) — usage ledger + roll-up

## Running

```
cargo test --manifest-path rust/Cargo.toml --all
cargo clippy --manifest-path rust/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path rust/Cargo.toml --all --check
```
