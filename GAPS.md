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
