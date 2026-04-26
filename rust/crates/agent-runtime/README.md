# agent-runtime

`agent-runtime` is the per-session runtime shell for the Rust agent stack.

The crate owns one `AgentSession` through `SessionRuntime`. Its semantic
boundary is intentionally small:

- `SessionInput` goes in through `SessionRuntime::enqueue`.
- `SessionRuntime::drive_with_executor` advances the session, passes
  `SessionAction`s to a caller-supplied executor hook, and returns observer
  `SessionEvent`s.

No transport lives here. A later PR can wrap `SessionRuntime` with stdin/stdout,
TCP, WebSocket, or process supervision. That transport can add whatever framing
it needs, but the durable design stays simple: inputs enter the runtime, events
flow out, and transcript state is persisted incrementally by the session/store
layer rather than fetched through ad hoc snapshots.

The TUI should still attach to the control plane, not directly to individual
session runtimes. In the distributed version, the control plane talks to each
runtime through a `SessionHandle`.

The crate's tests use local scripted helpers to prove model and tool cycles work
end to end without real network, provider, or tool execution. That test runner
is deliberately not public API; the real executor hook will be backed by the
model/tool/compaction modules as they land.
