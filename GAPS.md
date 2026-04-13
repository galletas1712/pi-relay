# Plan Gaps

This file tracks known divergences between the implementation branch and the multi-agent plan.
Update it as later phases land so the remaining work stays explicit.

## Phase 1

### Implemented

- Mailbox-based runtime in `packages/agent-core`
- Unified loop with foreground/background dispatch
- Background opt-in via `__background`
- Append-only `[PENDING]` tool results with later completion messages
- Compatibility surface preserved for `Agent` consumers in `pi-mono`

### Known Gaps

#### Background bash durability and progress are incomplete

Every bash run now captures combined stdout/stderr to a file, and background completion
messages include both the latest tail and the output file path. But the runtime still does
not create the planned session-scoped `tool-output/` files or emit model-visible progress
mailbox messages between dispatch and completion.

#### Orphan pending annotation is restore-scoped, not global

Phase 3 restore now records the specific background tool calls that were still pending at
crash time and rewrites those to `[TERMINATED]` in the restored agent context. But the
generic `annotateOrphanedPending()` helper in `agent-core` is still not wired into the
normal runtime or fork pipeline.

The corresponding session/UI transcript still keeps the historical `[PENDING]` entry. The
model-facing rewrite is correct on restore, but the user-visible transcript is not yet
rendered as `[TERMINATED]`.

#### Legacy scheduling fields still exist as compatibility shims

`steeringMode` and `followUpMode` are still exposed for compatibility with `pi-mono`, even
though the runtime no longer uses them as real scheduling modes.

#### Downstream teardown wiring is incomplete

`Agent.dispose()` exists in `agent-core`, but the upstream `pi-mono` session teardown is not
yet wired through it. That integration remains open.

## Phase 2

### Implemented

- Orchestrator runtime in `packages/orchestrator`
- Foreground `spawn`, `message`, and `report` tools
- Parent/child lifecycle tracking and idle notifications
- Root app wiring for interactive and RPC startup
- Phase 2 system prompt guidance for background tools and agent communication

### Known Gaps

#### Inter-agent delivery uses `AgentSession.sendCustomMessage()`

The plan describes agent reports, directives, and idle notifications as mailbox item kinds.
The current runtime uses `AgentSession.sendCustomMessage()` and the existing session queue
semantics instead. The externally visible behavior is the same, but the routing layer is
leaning on `pi-mono`'s queue/persistence primitives rather than extending the Phase 1 mailbox
for agent-to-agent traffic.

#### Root orchestration tools are wired as SDK custom tools

The plan sketches the root `spawn` and `message` tools as extension-registered tools. The
current implementation injects them as root `customTools` at session construction, the same
mechanism used for children. This keeps the root/child tool wiring uniform, but it diverges
from the document's extension-specific root setup.

#### Agent message rendering is still preformatted at creation time

The plan split agent-message formatting into `context-transform.ts`. The current runtime
still formats report/idle/directive/worklog strings eagerly in `messages.ts`, while
`context-transform.ts` focuses on restore-scoped pending annotations and live roster
injection.

#### Global LLM concurrency is not limited yet

The plan includes a global semaphore for concurrent LLM calls across agents. The current
runtime relies on per-agent serial execution from `AgentSession`, but it does not yet add a
cross-agent concurrency cap.

#### Root idling after child dispatch is still model-sensitive

The Phase 2 prompt guidance now tells the root to batch independent `spawn`/background tool
calls and then go idle. A live `openai-codex/gpt-5.4` run demonstrated the intended behavior,
but repeated reruns still sometimes serialize the spawns or keep the root engaged long enough
that the first child update lands before the root has gone idle. The runtime supports the
pattern once the model follows it, but prompt guidance alone does not make it deterministic.

## Phase 3

### Implemented

- Tree metadata persistence in `tree.json`
- Live subagent roster injection via `transformContext`
- Per-agent worklog forks on `turn_end`
- Ancestor worklog propagation on spawn
- Session restore for child trees and interrupted tools
- App startup now resumes the most recent session and restores the tree

### Known Gaps

#### Root restore resumption still uses a synthetic user message

The plan distinguishes between transcripts that can continue directly and transcripts that
end in an assistant message. The current runtime always resumes the restored root session by
injecting `[Session restored]` on `session_start`, because the extension API does not expose
`continue()` directly. This means restore currently adds one extra user turn even when the
transcript already ended in a continuable non-assistant message.

#### Child session lifecycle still uses generic `fork` and `resume` reasons

The plan sketches dedicated orchestrator-specific startup metadata for spawned and restored
child sessions. `pi-mono` only exposes the standard `session_start` reasons today, so child
spawn uses `reason: "fork"` and child restore uses `reason: "resume"` instead.
