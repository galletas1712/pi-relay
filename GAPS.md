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

#### Orphan pending annotation is not integrated yet

Automatic `[PENDING] -> [TERMINATED]` rewriting was removed because it could mislabel live
background work. The remaining fork/restore-specific orphan annotation still needs a proper
integration point.

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

#### Global LLM concurrency is not limited yet

The plan includes a global semaphore for concurrent LLM calls across agents. The current
runtime relies on per-agent serial execution from `AgentSession`, but it does not yet add a
cross-agent concurrency cap.
