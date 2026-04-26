# agent-rpc

`agent-rpc` is the first thin session-host boundary in the Rust agent runtime.

The crate hosts one `AgentSession` behind serde-friendly request/response
frames:

- `SessionRpcRequest::Enqueue` feeds `SessionInput` into the session.
- `SessionRpcRequest::Drive` advances local session state and returns drained
  `SessionAction`s, `SessionEvent`s, and a small status payload.
- `SessionRpcRequest::Snapshot` returns the current `ModelContext`, transcript
  entries, active leaf, context token count, and status.

No transport lives here yet. A later PR can put stdin/stdout, TCP, WebSocket, or
a process supervisor under these same frame types. That keeps the boundary dead
simple: each OS process can host one session host, while the control plane
talks to it through a `SessionHandle`. The TUI should still attach to the
control plane, not directly to individual sessions.

The crate also includes a deterministic `HeadlessSession` runner. It drives a
session and feeds every emitted action into a local `HeadlessActionHandler`.
Tests use `ScriptedActionHandler` to prove model and tool cycles work end to end
without real network, provider, or tool execution. Missing scripted work is a
test failure rather than silent "idle" success.

The concrete JSON shape is intentionally experimental until the first
TypeScript client lands, but the current tests pin the top-level request shape
to avoid accidental churn.
