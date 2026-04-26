# Rust agent stack

Rust implementation of pi-relay's agent runtime. See
[`docs/architecture.md`](docs/architecture.md) for the comprehensive plan.

## Crate layout

| Crate | What it owns |
|---|---|
| `agent-core` | Pure deterministic FSM for agent turns. Emits `TranscriptItem`s and `AgentAction` side effects. No I/O. Internals (`AgentState`, `Mailbox`) are private. |
| `agent-session` | Durable session history atop the core FSM. Owns a session-local `TranscriptStore` forest with one active leaf/path, the materialized `ModelContext` view, the `AgentRunner`, remote compaction requests, and rewind/fork operations. |
| `agent-orchestrator` | Composition struct for the runtime. Currently owns a SessionRegistry that tracks session identity and spawn relationships. Grows as ModelProvider, ToolRegistry, UsageLedger, AgentWorklogStore land. |
| `agent-rpc` | Serde/JSON-shaped per-session host frames plus a headless runner for end-to-end harness tests. Transport is intentionally deferred. |

## Layer discipline

`agent-core` ◂─ `agent-session` ◂─ `agent-orchestrator`

`agent-rpc` depends on `agent-session` and sits beside the control-plane work as
the future `SessionHandle` protocol. Views should still attach through the
control plane.

Each crate depends only on crates below it. Narrow public APIs between
crates; implementation details stay private. See the architecture doc for
the full layer stack including the future control-plane and view layers.

## Status

These crates land the session-layer abstractions: a pure deterministic FSM in
`agent-core`, durable forest-structured session history in `agent-session`, and a
thin composition struct in `agent-orchestrator`, and a transport-free RPC seam in
`agent-rpc`. Downstream work adds (in rough order): `SessionStore` (pluggable
storage), concrete RPC transport, `ControlPlane` (view/control split), event
bus/subscriptions (observability), `ModelProvider`, `Tool`/`ToolRegistry`,
compaction policy/execution, `UsageLedger`, multi-agent tools (spawn/report/idle),
`AgentWorklogStore`, `PromptAssembly`, daemon + `RemoteControlPlane`, and
distributed session processes. See the
[architecture doc](docs/architecture.md) for the full sequencing.

## Design docs

- [`docs/architecture.md`](docs/architecture.md) — overall plan

Feature-specific design docs for worklogs, cost aggregation, and multi-agent
tooling are planned; their current roadmap lives in the architecture doc.

## Running

```
cargo test --manifest-path rust/Cargo.toml --all
cargo clippy --manifest-path rust/Cargo.toml --all-targets -- -D warnings
cargo fmt --manifest-path rust/Cargo.toml --all --check
```
