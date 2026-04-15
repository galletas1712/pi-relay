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
crash time and rewrites those to `[INTERRUPTED]` in the restored agent context. But the
generic `annotateOrphanedPending()` helper in `agent-core` is still not wired into the
normal runtime or fork pipeline.

The corresponding session/UI transcript still keeps the historical `[PENDING]` entry. The
model-facing rewrite is correct on restore, but the user-visible transcript is not yet
rendered as `[INTERRUPTED]`.

#### Legacy scheduling fields still exist as compatibility shims

`steeringMode` and `followUpMode` are still exposed for compatibility with `pi-mono`, even
though the runtime no longer uses them as real scheduling modes.

#### Crash-time tool reconciliation is still incomplete

Graceful runtime teardown now aborts the root session before disposal and session switches.
But after a hard crash, restore still has no durable PID/job metadata to distinguish a truly
dead tool from a detached process that survived the parent. Restore therefore falls back to
conservative `[INTERRUPTED]` annotations instead of proving liveness or termination.

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

#### Subagent model/thinking overrides are not exposed through `spawn` yet

`SpawnConfig` and the session factory already support per-child `model` and `thinkingLevel`
overrides, but the `spawn` tool schema does not expose those fields to the model yet.
Children therefore inherit the parent's current `model` and `thinkingLevel` unless the
runtime constructs `SpawnConfig` programmatically.

#### Model/thinking changes are still immediate while a run is active

Changing the session model or thinking level mutates the live session state immediately.
That means a mid-run model/thinking change is still racy with the in-flight request and any
follow-on retry/compaction logic that inspects the current session model. The current
implementation does not yet defer or reject those changes until the session is idle.

## Phase 3

### Implemented

- Tree metadata persistence in `tree.json`
- Live subagent roster injection via `transformContext`
- Per-agent worklog forks on `turn_end`
- Worklog forks stay local to the agent/worklog files and ancestor propagation instead of surfacing as live parent transcript messages
- Ancestor spawn inheritance now uses the latest completed worklog plus recent unsummarized transcript tail, so child spawn does not block on in-flight worklog forks
- Session restore for child trees and interrupted tools
- App startup now resumes the most recent session and restores the tree

### Known Gaps

#### Child session lifecycle still uses generic `fork` and `resume` reasons

The plan sketches dedicated orchestrator-specific startup metadata for spawned and restored
child sessions. `pi-mono` only exposes the standard `session_start` reasons today, so child
spawn uses `reason: "fork"` and child restore uses `reason: "resume"` instead.

## Phase 4

### Implemented

- Relay-aware interactive runtime host in `packages/app`
- `/agents` TUI command for switching between root and child agents
- Live relay roster widget in the stock coding-agent TUI
- Safe attach behavior that reuses in-memory child sessions instead of rebuilding the
  orchestrator around a child session file
- Upstream TUI patch so attaching to an already-running agent restores loader state, queued
  input state, live tool completions, and branch-navigation completion state
- Detached sessions have their extension UI bindings reset to no-op, and attached child exit
  falls back to the root view automatically

### Known Gaps

#### TUI attach is in-process, not subprocess-based

The later infrastructure plan recommends separate interactive child processes for attachable
agents. The current implementation keeps all agents in one process and switches the active
TUI attachment between live in-memory sessions. This is simpler and preserves runtime
correctness for Phases 1-3, but it is a different attach model than the Phase 5 doc's
preferred subprocess design.

#### Attach is view switching, not a dedicated multi-pane relay console

The current UI adds a roster widget plus `/agents` switching inside the stock coding-agent
TUI. It does not yet provide a dedicated relay dashboard, split-pane transcript view, or
agent tree browser beyond the selector/widget surface.

#### Root-only session-management flows are still the canonical path

When attached to a child, regular prompt submission goes to that child session, but broader
session-management flows are intentionally blocked with a message that tells the user to
switch back to root first. There is not yet a dedicated child-local resume/new/fork UX with
explicit relay semantics.
