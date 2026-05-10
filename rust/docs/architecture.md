# Rust Agent Stack Architecture

This is the Rust rewrite plan for the personal pi-style agent runtime in this
repo. It is not a literal port of the local TypeScript fork's hierarchical
subagent orchestration. The Rust target keeps the session semantics that are
already valuable here: resumable transcript history, rewind, fork, compaction,
and clean boundaries for storage, providers, and tools.

## Goals

1. Keep the runtime small enough to understand and change quickly.
2. Preserve branch-aware session history: resume, rewind, fork, and compaction
   are core semantics, not UI features.
3. Keep storage swappable from day one. JSONL and memory exist now; Postgres can
   implement the same trait later.
4. Keep providers intentionally narrow: OpenAI and Anthropic/Claude only.
5. Keep tools separate from the agent loop so tool sets can be customized per
   session without changing the FSM.
6. Avoid hierarchical subagent machinery until there is a concrete personal-use
   reason to bring any of it back.

## What We Keep From pi-mono

The useful upstream shape is the separation between transcript vocabulary,
provider adapters, tools, and the agent turn loop. The Rust rewrite does not
need the full UI stack, extension surface, OAuth/provider breadth, or generalized
multi-agent product model.

The Rust code can deviate where our semantics are better:

- The session owns a transcript forest, not just a flat conversation.
- Rewind moves the active leaf without deleting history.
- Fork creates a new independent session from a turn-boundary branch.
- Restore crash-recovers open tails before resuming.
- Compaction is an explicit session operation driven by a harness/provider
  rather than hidden inside the core loop.

## Crate Stack

```text
pi-cli
  uses agent-session + agent-provider + agent-tools

agent-session
  owns AgentSession, TranscriptStore, ModelContext, AgentRunner,
  SessionRegistry, resume/rewind/fork/compaction, and storage snapshots

agent-core
  deterministic FSM for one turn loop; no I/O

agent-store
  backend-neutral StoredSession plus SessionStore trait

agent-provider
  ModelProvider trait plus OpenAI and Anthropic adapters

agent-tools
  Tool trait, ToolRegistry, and builtin read/write/edit/bash tools

agent-vocab
  shared ids, message blocks, tool calls/results, transcript items
```

`agent-vocab` is deliberately low in the stack so providers, tools, storage, and
session code can talk about messages without depending on the core FSM.

## Vocabulary

`agent-vocab` owns the serializable data shapes:

- `TurnId` and `ActionId` remain numeric local runtime ids.
- `ToolCallId` is an opaque string. Providers emit string ids, and storage
  should not coerce them through a numeric-only representation.
- `UserMessage` contains `Vec<ContentBlock>`.
- `ContentBlock` supports text and image input.
- `ImageContent` supports base64 and URL sources.
- `AssistantMessage` contains ordered `AssistantItem`s.
- `AssistantItem::ThinkingRedacted` records that hidden thinking existed without
  storing or replaying its content.
- `ToolDefinition`, `ToolCall`, and `ToolResultMessage` are provider/tool
  neutral.
- `TranscriptItem::UserMessage(UserMessage)` stores structured user content in
  the durable transcript.

Thinking block content is intentionally discarded. For this personal runtime,
the content is not needed for resume, replay, or later provider calls. Images,
however, are first-class because the agent needs image input support.

## Core FSM

`agent-core` owns only deterministic turn transitions:

- Inputs: user steer/follow-up, interrupt, model completion/failure, tool
  completion.
- Outputs: transcript items and requested side effects.
- Requested side effects: `RequestModel`, `RequestTool`, and `CancelTurn`.
- No filesystem, network, async runtime, provider SDK, tool execution, or
  durable storage.

Tagged steer/follow-up inputs are still supported as generic injected context,
but the core does not know about parent/child agents. A tagged input becomes
`TranscriptItem::Injected`; untagged input becomes `TranscriptItem::UserMessage`.

## Session Semantics

`agent-session` is the most important layer.

`AgentSession` owns:

- `AgentCoreLoop`
- `TranscriptStore`
- pending external model/tool work
- compaction state
- action and event outboxes

`TranscriptStore` is an append-only forest of `TranscriptStorageNode`s:

```rust
pub struct TranscriptStorageNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u128,
    pub item: TranscriptItem,
}
```

The active session view is one root-to-leaf path. `ModelContext` is materialized
from that path when a provider request or compaction request needs it.

Important operations:

- `resume`: build an idle core from a stored transcript path. Open tails are
  crash-recovered so the restored session is quiescent.
- `rewind`: move the active leaf to a prior turn boundary without deleting
  alternate branches.
- `fork`: copy a boundary path into a new independent `AgentSession`.
- `compact`: ask the harness for a replacement `ModelContext`, validate it, and
  install it as the new active path.

Queued user inputs survive history operations. Live external work is invalidated
when history changes so stale model/tool completions cannot attach to the new
path.

## Storage

`agent-store` defines backend-neutral persistence:

```rust
#[async_trait]
pub trait SessionStore: Send + Sync {
    async fn load_session(&self, session_id: &str) -> StoreResult<Option<StoredSession>>;
    async fn write_session(&self, session: &StoredSession) -> StoreResult<()>;
    async fn append_entries(
        &self,
        session_id: &str,
        entries: &[StoredTranscriptEntry],
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()>;
    async fn set_active_leaf(
        &self,
        session_id: &str,
        active_leaf_id: Option<&str>,
    ) -> StoreResult<()>;
}
```

Current backends:

- `InMemorySessionStore` for tests and short-lived sessions.
- `JsonlSessionStore` for simple local durability.

`AgentSession::to_stored_session` and `AgentSession::from_stored_session` bridge
between the live session and the backend-neutral stored shape. The JSONL format
is our own format, not a pi-mono compatibility format. It stores a session
header followed by serialized entries.

Future Postgres support should implement `SessionStore` directly. It should not
change `agent-core`, `agent-session`, providers, or tools.

## Providers

`agent-provider` defines:

- `ModelRequest`
- `ModelResponse`
- `ModelProvider`
- `OpenAiProvider`
- `AnthropicProvider`

Both adapters translate our transcript vocabulary into provider request bodies
and translate provider outputs back into `AssistantMessage`.

Provider scope is intentionally small:

- OpenAI chat completions.
- Anthropic messages.
- Text, images, tool definitions, tool calls, and redacted thinking markers.

Streaming, richer usage accounting, and model-specific tuning can be added
inside this crate without changing `agent-core`.

## Tools

`agent-tools` owns:

- `AgentTool`
- `ToolRegistry`
- `ToolContext`
- builtin `read`, `write`, `edit`, and `bash`

Tools are async and registry-driven. The agent loop only requests a tool call;
the harness or CLI decides which registry to use and feeds the result back into
the session. This keeps personal customization easy: add or remove tools from a
registry without changing the core/session crates.

The current builtins are intentionally minimal. They are enough for a local
coding loop, but they are not a sandbox or permission system.

## Live Session Registry

The old `agent-orchestrator` crate has been removed. Its only remaining useful
type, `SessionRegistry`, now lives in `agent-session`.

`SessionRegistry` is in-memory process state, not durable storage. It is a
`SessionId -> AgentSession` map for keeping multiple sessions open, switching
between them, and inserting forks as independent sessions. Durable history stays
in `agent-store`.

## CLI

`pi-cli` is a minimal smoke-test binary:

```text
pi-rs [claude|openai] [model] <prompt>
```

It creates one `AgentSession`, one provider, and the builtin tool registry, then
drives the session loop until quiescent. It is not meant to be a full TUI or
general product shell.

## Implementation Status

Implemented now:

- shared `agent-vocab`
- structured user messages with image input
- redacted thinking markers
- string tool-call ids
- pluggable `SessionStore`
- in-memory and JSONL storage backends
- session <-> stored-session conversion
- OpenAI and Anthropic provider adapters
- separate tool crate and builtin tools
- minimal CLI driver
- live `SessionRegistry` inside `agent-session`

Useful next steps:

- Add CLI flags for loading/saving named JSONL sessions.
- Add provider streaming normalization.
- Add provider usage/context-token reporting.
- Add a safer command execution policy around `bash`.
- Add a Postgres `SessionStore` implementation when needed.
- Fix the local macOS test-link environment for `-liconv` so full workspace
  tests can run here.
