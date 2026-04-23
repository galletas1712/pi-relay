# Rust agent stack

Rust implementation of pi-relay's agent runtime. See
[`docs/architecture.md`](docs/architecture.md) for the comprehensive plan,
and `docs/worklog-design.md` / `docs/cost-aggregation.md` for feature-specific
deep dives.

## Crate layout

| Crate | What it owns |
|---|---|
| `agent-core` | Pure deterministic FSM for agent turns. Emits TranscriptRecord events and AgentAction side-effects. No I/O. Internals (`AgentState`, `Mailbox`) are private. |
| `agent-session` | Durable session history atop the core FSM. Owns the Context (DAG of entries with branch-aware navigation), the materialized Transcript view, the AgentRunner, and history-edit operations (compact, rewind, fork, replace_transcript) behind a ContextEdit view type. |
| `agent-orchestrator` | Composition struct for the runtime. Currently owns a SessionRegistry that tracks session identity and spawn relationships. Grows as ModelProvider, ToolRegistry, UsageLedger, AgentWorklogStore land. |

## Layer discipline

`agent-core` ◂─ `agent-session` ◂─ `agent-orchestrator`

Each crate depends only on crates below it. Narrow public APIs between
crates; implementation details stay private. See the architecture doc for
the full layer stack including the future control-plane and view layers.

## Status

These crates land the session-layer abstractions: a pure deterministic FSM in
`agent-core`, durable DAG-structured session history in `agent-session`, and a
thin composition struct in `agent-orchestrator`. Downstream work adds (in rough
order): `SessionStore` (pluggable storage), `ControlPlane` (view/control split),
`SessionEvent` stream (observability), `ModelProvider`, `Tool`/`ToolRegistry`,
`Compactor` (auto-compaction executor), `UsageLedger`, multi-agent primitives
(spawn/report/idle), `AgentWorklogStore`, `PromptAssembly`, daemon +
`RemoteControlPlane`, and distributed session processes. See the
[architecture doc](docs/architecture.md) for the full sequencing.

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
