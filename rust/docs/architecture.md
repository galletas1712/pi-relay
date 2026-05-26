# Rust Agent Stack Architecture

This is the Rust rewrite of the core pi-style runtime in this repo. It is not a
literal port of the local TypeScript fork's hierarchical subagent work. The
Rust stack keeps the semantics that are useful for personal agent work:
branch-aware transcript history, switch, compaction, automatic local tools, and
a Postgres-backed websocket control plane.

## Goals

1. Keep the runtime small enough to understand and change quickly.
2. Preserve branch-aware session history: implicit resume, switch, and
   compaction are core semantics.
3. Make the frontend protocol durable and recoverable by treating Postgres as
   the websocket source of truth.
4. Keep providers intentionally narrow: Codex/OpenAI and Anthropic/Claude only.
5. Keep tools separate from the agent loop so tool sets can be customized
   without changing the FSM.
6. Do not include subagent orchestration or generic injected-message routing.

## Crate Stack

```text
agent-daemon
  websocket RPC + provider/tool dispatch

agent-session
  AgentSession, TranscriptStore, ModelContext,
  resume/switch, storage snapshots

agent-core
  deterministic FSM for one turn loop; no I/O

agent-store
  Postgres session/event/action/input persistence

agent-provider
  ModelProvider trait plus OpenAI/Codex and Anthropic adapters

agent-tools
  Tool trait, ToolRegistry, builtin read/write/edit/bash tools

agent-vocab
  shared ids, message blocks, tool calls/results, transcript items
```

`agent-vocab` stays low in the stack so providers, tools, storage, session code,
the CLI, and the daemon can talk about messages without depending on the FSM.

## Agent Daemon Modules

`agent-daemon` is intentionally a thin control plane around the crates below it.
Its module split is:

- `main.rs`: websocket accept loop, JSON-RPC routing, and RPC handlers. It is
  allowed to translate protocol parameters into runtime calls, but it should not
  own provider execution, storage SQL, auth refresh, or transcript parsing.
- `config.rs`: command-line and environment parsing for daemon startup.
- `types.rs`: daemon-local RPC envelopes, request parameter structs, runtime
  handles, and websocket errors.
- `state.rs`: process-local daemon state: repository handle, active session
  projections, per-session driver locks, event broadcaster, tool registry, and
  tool context.
- `codec.rs`: translation between JSON-RPC payloads and core/session vocabulary,
  plus transcript recovery helpers that operate on storage-neutral session
  shapes.
- `auth.rs`: OpenAI/Codex/Anthropic credential loading and Codex token refresh.
- `provider_runtime.rs`: provider selection and model execution. It is the only
  daemon module that knows how provider config maps to concrete provider
  adapters.
- `runtime.rs`: `SessionDriver`, live session loading, crash recovery,
  queued-input consumption, action completion, model/tool dispatch, and event
  publishing.
- `agent-store::PostgresAgentStore`: concrete Postgres persistence. SQL,
  transaction boundaries, row mapping, event replay, input ledger, action rows,
  daemon config, and recovery persistence live in the storage crate rather than
  inside the daemon.

This split keeps the daemon as transport/runtime glue. Postgres is the only
supported durable backend; there is no storage trait until a second real
backend earns the abstraction. `agent-session` remains independent from
websocket transport and SQL by owning only live session semantics plus storage
snapshot shapes.

## Feature Audit

Implemented user-facing behavior:

- Structured text and image user input.
- Redacted assistant thinking markers without storing thinking content.
- String tool-call ids.
- Automatic local tool execution with no approval interface.
- Durable session rows in Postgres for websocket sessions.
- Reconnect event replay with `events.subscribe(after_event_id)`; initial
  subscriptions attach from the current head and load state from snapshots.
- Derived session activity: `idle`, `queued`, `running`.
- Steer/follow-up sends with idempotent `client_input_id` for both idle
  accepted input and busy queued input.
- Queued follow-up promotion to steer priority, consumed in promotion order.
- Mid-turn steer insertion after completed tool results and before the next
  model request; follow-ups remain next-turn work, and compaction remains an
  action barrier.
- Turn-level interrupt.
- Idle-only retry/continue for terminal model turns. This resumes from the
  original model checkpoint and appends a sibling branch instead of duplicating
  the user message.
- Idle-only active-branch switch.
- Idle-only compaction request with structural validation.
- Daemon restart recovery for open transcript tails.
- Stale action rejection through persisted `attempt_id`.
- Repo-level `PI.md` prompt composition, with each workspace directory's
  `AGENTS.md` included by the template.
- Provider config with `max_tokens` and `prompt_cache.key`.
- Real Codex provider path through `~/.codex/auth.json` or
  `CODEX_ACCESS_TOKEN`.
- OpenAI Responses provider through ChatGPT/Codex subscription auth and
  Anthropic API-key provider adapter.

Not implemented by design:

- Hierarchical subagent orchestration.
- Approval UI or tool permission policy.
- Explicit open/close/resume/delete session RPC.
- General plugin/provider marketplace.
- Non-Postgres storage backends. The old in-memory/JSONL store layer was
  removed once the websocket path became Postgres-only.

## Vocabulary

`agent-vocab` owns serializable data shapes:

- `TurnId` and `ActionId`: numeric local runtime ids.
- `ToolCallId`: opaque provider/tool id string.
- `UserMessage`: `Vec<ContentBlock>`.
- `ContentBlock`: `Text` and `Image`.
- `ImageContent`: `mime_type` plus `ImageSource`.
- `ImageSource`: `Base64` or `Url`.
- `AssistantMessage`: ordered `Vec<AssistantItem>`.
- `AssistantItem`: `Text`, `ThinkingRedacted`, or `ToolCall`.
- `ToolDefinition`, `ToolCall`, `ToolResultMessage`, `ToolResultStatus`.
- `TranscriptItem`: `TurnStarted`, `UserMessage`, `AssistantMessage`,
  `ToolCallStarted`, `ToolResult`, `TurnFinished`, and `CompactionSummary`.

Thinking block content is intentionally discarded. Images are first-class input
because the agent needs vision-capable provider requests.

## Core FSM

`agent-core` owns only deterministic turn transitions:

- Inputs: user steer/follow-up, interrupt, model completion/failure, tool
  completion.
- Outputs: transcript items and requested side effects.
- Requested side effects: model request, tool request, and turn cancellation.
- No filesystem, network, async runtime, provider SDK, tool execution, durable
  storage, or websocket concepts.

The core does not model injected context, sources, agents, or routing metadata.

## Session Semantics

`agent-session` wraps the pure FSM with durable history semantics.

`AgentSession` owns:

- `AgentCoreLoop`.
- `TranscriptStore`.
- Private outstanding action tracking.
- Optional context-token count.
- Action and event outboxes.

`TranscriptStore` is an append-only forest:

```rust
pub struct TranscriptStorageNode {
    pub id: String,
    pub parent_id: Option<String>,
    pub timestamp_ms: u64,
    pub item: TranscriptItem,
}
```

The active session view is one root-to-leaf path. `ModelContext` is
materialized from that path for provider requests and daemon-owned compaction
jobs.

Important operations:

- Restore/resume: build an idle core from stored transcript history. Open tails
  are recovered as crashed before the session is exposed as idle.
- Switch: move the active leaf to a prior turn boundary without deleting rows.
  This is transcript-only; workspace files are not checkpointed or restored.
- Compaction: the daemon asks the provider to summarize the active
  `ModelContext`; `agent-store` atomically appends a
  `TranscriptItem::CompactionSummary` root and makes that root active. The old
  branch remains available for same-session active-leaf switching and tree
  inspection. Compaction is not a session boundary.

The session primitive can invalidate active work during a local switch. The
websocket contract is stricter: source-mutating history writes are idle-only so
frontend lifecycle rules are easy to reason about and test.

## Storage And Recovery

There is one durable storage backend today:

- `agent-store::PostgresAgentStore`: normalized Postgres persistence for
  sessions, transcript entries, queued inputs, actions, events, and daemon
  config.

`agent-session` also owns `StoredSession` and `StoredTranscriptEntry` as
storage snapshot shapes used to rehydrate the live session semantics. Those
types are not a backend abstraction by themselves.

Postgres is the only source of truth. The daemon may hold an in-memory
`AgentSession` while a turn is running, but accepted
transitions are written transactionally before follow-on work is dispatched.
If that transactional write fails after the live session has advanced, the
daemon evicts the live session so the next interaction reloads from Postgres.
Idle user inputs are materialized directly into transcript/action/event state.
Busy user inputs are kept in Postgres until the session reaches a boundary.
When the daemon is ready to consume one, it first claims the row as
`consuming`; the final `consuming -> consumed` transition happens in the same
transaction as the transcript/action/events that materialized it. Abandoned
claims are reset to `queued` on first touch after daemon restart. That avoids a
consumed-but-not-transcripted mailbox gap during daemon death while still
letting queued edits fail cleanly once consumption has begun.

Unfinished actions are execution leases owned by the daemon process. Startup
marks leftover unfinished action rows stale before accepting websocket clients.
If a stale action left the active transcript in an open turn, first touch
rehydrates the session and appends a crashed turn boundary. A clean boundary
with an unfinished action is otherwise treated as legitimate live work, which is
what lets queued follow-ups wait behind provider-backed compaction without
triggering transcript repair.

The Postgres data model is documented in
[`websocket-rpc.md`](websocket-rpc.md). Its important recovery invariants are:

- Open transcript tails are valid while unfinished actions explain them.
- Interrupt commits the closed interrupted tail and action invalidation
  together.
- Session-wide cancellation is represented by `session.work_cancelled`; it does
  not create a model/tool action row.
- Daemon death before commit leaves the old open tail recoverable.
- Daemon death after commit leaves replayable events.
- Daemon death during outstanding model/tool work is repaired on next touch with stale
  actions plus a crashed turn tail.
- Late completions from stale attempts cannot mutate history.

## Providers

`agent-provider` defines:

- `ModelRequest`.
- `ModelResponse`.
- `ModelProvider`.
- `OpenAiProvider`.
- `AnthropicProvider`.

`ModelRequest` includes `model`, `PromptSections`, transcript items, tool
definitions, optional explicit `max_tokens`, and optional `prompt_cache_key`.
`PromptSections` separates a stable prefix from dynamic daemon context so the
cacheable prefix is rendered before request-specific context and transcript
history.

Provider adapters:

- OpenAI/Codex backend via streamed Responses API, bearer ChatGPT token,
  optional `ChatGPT-Account-ID`, and the Codex residency routing header.
- Anthropic Messages API via `ANTHROPIC_API_KEY`.

Supported provider features:

- Text input/output.
- Image input via URL or base64 data URL.
- Tool definitions and tool calls.
- Tool result replay.
- Redacted thinking markers.
- Stable-prefix/dynamic-context prompt rendering.
- Prompt cache key on OpenAI request paths.
- Priority service tier, explicit parallel tool calls, and disabled output
  storage on OpenAI/Codex Responses requests.
- No daemon-enforced OpenAI/Codex output-token cap when `max_tokens` is omitted.

Streaming is currently normalized inside the OpenAI/Codex provider by reading
the SSE response and producing one `AssistantMessage`.

## Tools

`agent-tools` owns:

- `AgentTool`.
- `ToolRegistry`.
- `ToolContext`.
- Builtin `read`, `write`, `edit`, and `bash`.

Tools are async and registry-driven. The agent loop requests a tool call; the
daemon or CLI chooses the registry and feeds a `ToolResultMessage` back into
the session.

Current builtins:

- `Grep`: search files under the session `outer_cwd`.
- `Edit`/`apply_patch`: view or edit files under the session `outer_cwd`.
- `Bash`: run a fresh command under the session `outer_cwd` with a 30-second
  default timeout.

Tools are intentionally unsandboxed personal-use primitives. Tool calls are
always allowed, and there is no approval interface.

## Daemon And RPC

`agent-daemon` exposes `pi-agentd`, a websocket JSON-RPC-ish server backed by
Postgres. It owns:

- Schema migration for the normalized Postgres tables.
- Websocket request routing.
- Reconnect event replay.
- Session recovery before first touch.
- Provider credential loading.
- Automatic tool dispatch.
- Development harness methods for deterministic model/compaction edges.

The implemented RPC contract is documented in
[`websocket-rpc.md`](websocket-rpc.md).

## Removed Pieces

- `agent-orchestrator` crate.
- `SessionRegistry`.
- Async channel `AgentRunner`.
- Hierarchical subagent-specific control surfaces and routing metadata.

Durable storage is the registry. Process-local state only exists to drive
current work.

## Verification Status

The current implementation has been checked with:

- Full workspace formatting.
- Full workspace compile.
- Full workspace unit tests.
- Manual websocket exercises against a real Postgres database.
- Browser flow checks against the real websocket daemon.
- Real Codex text and image-URL turns through the websocket daemon.
- Daemon death/restart recovery with snapshot reload plus reconnect event
  replay.

Anthropic real-provider websocket tests require a raw `ANTHROPIC_API_KEY`; the
local Claude Code credentials were not a raw Anthropic key.

## Useful Next Steps

- Add a small scripted websocket exercise runner for the manual scenarios.
- Add SQLx offline/prepare metadata if we decide compile-checked queries are
  worth the extra local setup. The runtime store already uses SQLx for pooling,
  binding, transactions, and row decoding.
- Add richer usage/cost accounting inside provider adapters without changing
  `agent-core`.
