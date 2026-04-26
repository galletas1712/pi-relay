# agent-rpc

`agent-rpc` is the first thin session-host boundary in the Rust agent runtime.

The crate hosts one `AgentSession` behind serde-friendly request/response
frames:

- `SessionRpcRequest::Enqueue` feeds `SessionInput` into the session.
- `SessionRpcRequest::Drive` advances local session state and returns drained
  `SessionAction`s, `SessionEvent`s, and a small status payload.
- `SessionRpcRequest::Snapshot` returns the current `ModelContext`, transcript
  entries, active leaf, context token count, and status.

No transport or action executor lives here yet. A later PR can put stdin/stdout,
TCP, WebSocket, or a process supervisor under these same frame types, and a
harness layer can decide how to satisfy `SessionAction`s. That keeps the
boundary dead simple: each OS process can host one session host, while the
control plane talks to it through a `SessionHandle`. The TUI should still attach
to the control plane, not directly to individual sessions.

The crate's tests use local scripted helpers to prove model and tool cycles work
end to end without real network, provider, or tool execution. That test runner
is deliberately not public API; the real action loop belongs in a harness crate
or module above this RPC seam.

The concrete JSON shape is intentionally experimental until the first
TypeScript client lands, but the current tests pin the top-level request shape
to avoid accidental churn.
