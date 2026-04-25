# agent-orchestrator

Multi-agent composition: a session registry plus fire-and-forget routing primitives for cross-session directives and reports.

## Responsibility

This crate owns the top-level composition struct `AgentOrchestrator`. Today that struct owns exactly one thing — a `SessionRegistry<AgentSession>` that holds live sessions keyed by `SessionId` and tracks a spawn-parent map representing the agent tree. The orchestrator's public surface wraps the registry with two routing primitives, `send_message` (parent -> child directive) and `send_report` (child -> parent report), that validate the spawn relationship and enqueue a tagged `AgentInput` on the target session's mailbox.

Everything is in-process and synchronous. The orchestrator does not schedule, drive, or await sessions; callers push inputs through the registry and drive each session themselves (via `AgentSession::drive`, or the async `AgentRunner` in `agent-session`). Routing is strictly fire-and-forget: validate the relationship, enqueue one input, return.

What is explicitly *not* here yet: no tool layer wrapping `send_message`/`send_report` into callable tools; no worklog store; no usage ledger / cost aggregation; no async driver (that belongs to `agent-session::AgentRunner`); no `ControlPlane` trait or `SessionStore`. Those join as peer fields on `AgentOrchestrator` in later PRs — see `rust/docs/architecture.md` for the roadmap.

## Public interface

Re-exported from `lib.rs`:

- **Registry types** — `AgentOrchestrator`, `SessionRegistry<S>` (defaults `S = AgentSession`), `SessionId` (alias for `String`)
- **Registry errors** — `RegistryError` with variants `SessionAlreadyExists`, `SessionNotFound`, `ParentNotFound`, `HasChildren`
- **Routing** — `AgentOrchestrator::send_message`, `AgentOrchestrator::send_report`
- **Routing errors** — `RouteError` with variants `SenderNotFound`, `TargetNotFound`, `NotAChild`, `NoParent`
- **Kind tag constants** — `KIND_AGENT_DIRECTIVE = "agent_directive"`, `KIND_AGENT_REPORT = "agent_report"`

`SessionRegistry` and both error enums live in `src/registry.rs`; the orchestrator struct, constants, and routing methods live in `src/lib.rs`. No `agent-session` or `agent-core` symbols are re-exported from this crate; downstream callers import those directly.

Key method signatures:

```rust
impl AgentOrchestrator {
    pub fn new() -> Self;
    pub fn registry(&self) -> &SessionRegistry<AgentSession>;
    pub fn registry_mut(&mut self) -> &mut SessionRegistry<AgentSession>;

    pub fn send_message(
        &mut self,
        from: &SessionId,
        to: &SessionId,
        content: String,
    ) -> Result<(), RouteError>;

    pub fn send_report(
        &mut self,
        from: &SessionId,
        content: String,
    ) -> Result<(), RouteError>;
}

impl<S> SessionRegistry<S> {
    pub fn spawn(&mut self, id: impl Into<SessionId>, session: S)
        -> Result<(), RegistryError>;
    pub fn spawn_child(
        &mut self,
        id: impl Into<SessionId>,
        session: S,
        parent: impl Into<SessionId>,
    ) -> Result<(), RegistryError>;

    pub fn get(&self, id: &str) -> Result<&S, RegistryError>;
    pub fn get_mut(&mut self, id: &str) -> Result<&mut S, RegistryError>;
    pub fn remove(&mut self, id: &str) -> Result<S, RegistryError>;

    pub fn contains(&self, id: &str) -> bool;
    pub fn parent(&self, id: &str) -> Option<&SessionId>;
    pub fn children<'a>(&'a self, parent: &'a str)
        -> impl Iterator<Item = &'a SessionId> + 'a;
    pub fn ids(&self) -> impl Iterator<Item = &SessionId> + '_;
    pub fn len(&self) -> usize;
    pub fn is_empty(&self) -> bool;
}
```

Note: there is no `spawn_session` method on `AgentOrchestrator` today. Callers that want to insert a session reach through `orchestrator.registry_mut().spawn(...)` for a root or `.spawn_child(...)` for a child. A higher-level spawn entry point (autonomous prompt assembly, child tool registry setup, initial FollowUp injection) is future work.

## Internals

### Module map

- `src/lib.rs` — `AgentOrchestrator` struct, `send_message` / `send_report` methods, `KIND_AGENT_DIRECTIVE` / `KIND_AGENT_REPORT` constants, integration tests.
- `src/registry.rs` — `SessionRegistry<S>` (generic over session type, defaults to `AgentSession`), `SessionId` type alias, `RegistryError`, `RouteError`, spawn-tree bookkeeping.

### The agent tree

The registry stores two `BTreeMap`s: one from `SessionId` to session, and one from child `SessionId` to parent `SessionId`. A root session inserted via `spawn(id, session)` has no entry in the spawn-parent map; a session inserted via `spawn_child(id, session, parent)` records `id -> parent` there atomically with the child insert.

```
          A (root)
         / \
        B   C
       / \
      D   E

spawn_parents:
  B -> A
  C -> A
  D -> B
  E -> B
  (A has no entry — it is a root)
```

`SessionRegistry::children(parent)` scans the spawn-parent map for matches. `remove(id)` refuses with `RegistryError::HasChildren` if any entry in the spawn-parent map still points at `id`, so removing a subtree requires bottom-up traversal — no orphaned parent references can leak.

### Routing: send_message vs send_report

| Operation                         | Direction                               | Delivers via                                                 | Kind tag                |
|-----------------------------------|-----------------------------------------|--------------------------------------------------------------|-------------------------|
| `send_message(from, to, content)` | parent -> direct child                  | `AgentInput::steer_tagged(from, KIND_AGENT_DIRECTIVE, content)` | `KIND_AGENT_DIRECTIVE` |
| `send_report(from, content)`      | child -> spawn parent (registry lookup) | `AgentInput::follow_up_tagged(from, KIND_AGENT_REPORT, content)` | `KIND_AGENT_REPORT` |

Both primitives are fire-and-forget. They validate the relationship, push a single `AgentInput` onto the target's mailbox via `AgentSession::enqueue_input`, and return. No wait for ack, no turn driving, no observation of downstream state.

When the target session next starts a turn from a tagged input, the FSM in `agent-core` materialises a `TranscriptRecord::Injected(InjectedMessage { kind, content, metadata: { "from": <sender_id> } })` at the turn boundary instead of a plain `UserMessage`. This preserves sender identity in the durable transcript. The orchestrator owns the `KIND_*` string constants; `agent-core` and `agent-session` treat `from` and `kind` as opaque tags and never reference the specific values by name.

`RouteError` variants, from `registry.rs`:

- `SenderNotFound` — `from` is not in the registry.
- `TargetNotFound` — `to` is not in the registry (`send_message` only).
- `NotAChild` — `to` exists but is not a direct child of `from` in the spawn tree (`send_message` only). Unrelated sessions and non-immediate descendants both fail this check.
- `NoParent` — `from` exists but has no entry in the spawn-parent map, i.e. it is a root (`send_report` only).

Routing flow, `send_message`:

```
 orchestrator.send_message(&parent_id, &child_id, "do X".into())
        |
        v  validate target is a direct child of sender
 AgentInput::Steer {
     from: Some(parent_id),
     kind: Some("agent_directive"),
     content: "do X",
 }
        |
        v  AgentSession::enqueue_input  (Steer priority)
 child mailbox
        |
        v  child.drive() on next tick
 TranscriptRecord::Injected(InjectedMessage {
     kind: "agent_directive",
     content: "do X",
     metadata: { "from": parent_id },
 })
```

`send_report` is symmetric: it looks up `registry.parent(from)`, constructs `AgentInput::follow_up_tagged(from, KIND_AGENT_REPORT, content)`, and enqueues it on the parent's mailbox. Follow-ups take normal priority and wake the parent on its next idle turn.

### Why no idle-vs-busy branching

The TypeScript counterpart (`packages/orchestrator/src/orchestrator.ts::deliverMessage`) inspects the target session's `isStreaming / isRetrying / isCompacting` flags and a `reactivating` latch before deciding whether to call its current `sendCustomMessage` API with `triggerTurn: true` (reactivate an idle agent) or `deliverAs: "steer"` (interrupt a busy one). The Rust mailbox model absorbs inputs uniformly: `AgentSession::enqueue_input` pushes onto a single queue without observing the target's live state. The FSM consumes queued `Steer` / `FollowUp` input only when it is `Idle`. If the state is `RunningModel` or `RunningTools`, routed input waits behind the active work. If the state is `ReadyToContinue`, the mailbox emits the synthetic `ContinueModel` event before user input, so the current turn resumes with another model request rather than starting a new routed turn. `Steer` inputs are higher priority than `FollowUp` once the core reaches `Idle`, but they do not interrupt an active turn by themselves.

## Relationship to other crates

- **`agent-session`** (direct dep): the registry's default session type is `AgentSession`. Routing calls `AgentSession::enqueue_input` to deliver inputs; it never reaches into `Context` directly. History-edit ops (`SummarizeSpan`, `Compact`, `Rewind`, `ReplaceTranscript`, `fork`) stay on `AgentSession` and are invoked by callers through `registry_mut().get_mut(id)`.
- **`agent-core`** (direct dep, transitively via session): the FSM types `AgentInput::Steer` and `AgentInput::FollowUp` carry `from: Option<String>` and `kind: Option<String>`. Tagged inputs become `TranscriptRecord::Injected` entries when consumed; the invariant `from.is_some() == kind.is_some()` is enforced by the `AgentInput::steer_tagged` / `follow_up_tagged` constructors that this crate uses.

See `rust/docs/architecture.md` for the full crate stack and PR sequencing.

## TS parity

`send_message` and `send_report` are the primitives that future Rust `message` / `report` tools will call, mirroring `packages/orchestrator/src/tools/message.ts` (`runtime.routeMessage`) and `packages/orchestrator/src/tools/report.ts` (`runtime.handleReport`). No tool layer exists in Rust yet — when it lands, each tool will be a thin wrapper that calls the matching orchestrator method. `SessionRegistry::spawn_child` is the lower half of what `packages/orchestrator/src/tools/spawn.ts` (`runtime.spawnAgent`) does; the upper half — role/prompt assembly, child tool-registry configuration, initial FollowUp injection — is future work and will live in a higher-level spawn API on `AgentOrchestrator`.

## What this crate does NOT do

- No runtime scheduling or async driving (that is `agent-session::AgentRunner`).
- No tool registry and no tool execution.
- No `message` / `report` / `spawn` tool definitions on top of the routing primitives.
- No usage or cost aggregation.
- No worklog store or ancestor-context prefix building.
- No session persistence (future `SessionStore` trait).
- No remote control plane (future `ControlPlane` trait; `LocalControlPlane` will wrap this crate).
- No event bus or observer subscription plumbing.
