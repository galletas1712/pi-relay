# Rust Agent Stack Architecture

This is the Rust rewrite of the core pi-style runtime in this repo. It is not a
literal port of the local TypeScript fork's hierarchical subagent work. The Rust
stack keeps the semantics that are useful for personal agent work: branch-aware
transcript history, switch, compaction, automatic local tools, and a
Postgres-backed websocket control plane.

This document is the overview and map. Each crate has its own reference under
[`modules/`](modules/); the cross-cutting product/engineering rationale lives in
[`design-decisions.md`](design-decisions.md); the frontend wire contract is in
[`websocket-rpc.md`](websocket-rpc.md); the React client is documented in
[`../../packages/web/docs/web-ui.md`](../../packages/web/docs/web-ui.md); and
the audited provider capability matrix is in
[`provider-api-support.md`](provider-api-support.md). In-flight future work
lives under [`plans/`](plans/).

## Goals

1. Keep the runtime small enough to understand and change quickly.
2. Preserve branch-aware session history: implicit resume, switch, and
   compaction are core semantics.
3. Make the frontend protocol durable and recoverable by treating Postgres as
   the websocket source of truth.
4. Keep providers intentionally narrow: OpenAI/Codex and Anthropic/Claude only.
5. Keep tools separate from the agent loop so tool sets can be customized
   without changing the FSM.
6. Support bounded parent/child subagent delegation as **delegations**: the parent
   runs one full (writing) subagent or a parallel fan-out of read-only
   subagents, parks, and is resumed with a daemon-authored wakeup observation.
   No generic injected-message routing layer or event bus between arbitrary
   sessions.

## Crate Stack

```text
agent-daemon     websocket RPC + provider/tool dispatch, recovery, events
   |
   |  drives
   v
agent-session    transcript forest, model-context materialization,
   |             resume / switch / compaction, replay sidecar lane
   |
   +-- agent-core      deterministic FSM for one turn loop; no I/O
   +-- agent-store     Postgres persistence (the only durable backend)
   +-- agent-provider  ModelProvider + OpenAI/Codex and Anthropic adapters
   +-- agent-tools     AgentTool, ToolRegistry, builtin tools
   +-- agent-mcp       stdio/HTTP MCP + rmcp-backed OAuth + immutable manifests
   +-- agent-prompt    renders the PI.md system prompt
            |
            v
agent-vocab      shared ids, message blocks, tool calls/results,
                 transcript items, provider config (no behavior, no I/O)
```

| Crate | What it owns | Reference |
| --- | --- | --- |
| `agent-vocab` | Serializable ids, message blocks, images, assistant items, tool calls/results, transcript items, and provider config. | [modules/agent-vocab.md](modules/agent-vocab.md) |
| `agent-core` | Pure deterministic FSM for one agent turn loop. No I/O. | [modules/agent-core.md](modules/agent-core.md) |
| `agent-session` | Durable transcript forest, model-context materialization, resume, switch, compaction, and the provider-replay lane. | [modules/agent-session.md](modules/agent-session.md) |
| `agent-store` | Postgres-only session/transcript/queue/action/event persistence plus recovery and revision/queue projections. | [modules/agent-store.md](modules/agent-store.md) |
| `agent-provider` | `ModelProvider` plus OpenAI/Codex (Responses) and Anthropic (Messages) adapters, prompt-cache shaping, and provider compaction. | [modules/agent-provider.md](modules/agent-provider.md) |
| `agent-tools` | `AgentTool`, `ToolRegistry`, and the builtin `edit`/`bash`/`web_search`/`web_fetch`/`LoadSkill`/delegation tools. | [modules/agent-tools.md](modules/agent-tools.md) |
| `agent-mcp` | Operator-configured stdio/Streamable HTTP MCP clients, Codex-parity rmcp-backed OAuth with daemon-owned permission-protected file credentials, restart restoration/refresh, bounded bearer injection, sanitized public status/login/manual-completion/cancel/local-logout, New Session auth plus inventory selection, and deterministic MCP-only session manifests kept separate from `ToolRegistry`. | [plans/mcp-client.md](plans/mcp-client.md) |
| `agent-daemon` | `pi-agentd` websocket RPC server with runtime/provider/tool dispatch, recovery, and event publishing. | [modules/agent-daemon.md](modules/agent-daemon.md) |
| `agent-prompt` | Renders the repo-level `PI.md` system prompt from session/workspace/tool/skill context. | [modules/agent-prompt.md](modules/agent-prompt.md) |

`agent-vocab` stays at the bottom of the graph so providers, tools, storage,
session code, and the daemon can all talk about messages without depending on
the FSM.

## How A Turn Flows

```text
user input
   |  (RPC)
   v
agent-daemon ---- persist accepted transition ----> agent-store (Postgres)
   |
   |  materialize ModelContext from the active leaf
   v
agent-session --> agent-core FSM --requests--> agent-provider (model call)
   |                   |                            |
   |                   |                       agent-tools (tool calls)
   |                   v
   |             transcript items + side effects
   v
agent-store (append, bump revisions, emit events) --> websocket subscribers
```

Postgres is committed before follow-on provider/tool work is dispatched, so a
crash leaves either a recoverable open tail, replayable events, or the narrow
leased post-compaction dispatch intent. That intent is reclaimed at least once;
a crash after provider acceptance can duplicate the external call because the
provider requests have no idempotency key. Long provider, tool, and compaction
I/O runs outside the per-session row lock and reconverges through action
attempt/dispatch-owner fences. The mechanics live in
[agent-store](modules/agent-store.md) and [agent-daemon](modules/agent-daemon.md).

## Feature Audit

Implemented user-facing behavior:

- Structured text and image user input.
- Automatic local tool execution with no approval interface.
- Durable session rows in Postgres for websocket sessions.
- Reconnect event replay with `events.subscribe(after_event_id)`; initial
  subscriptions attach from the current head and load state from snapshots.
- Derived session activity: `idle`, `queued`, `running`.
- Steer/follow-up sends with idempotent `client_input_id` for both idle
  accepted input and busy queued input.
- Queued follow-up promotion to steer priority, plus follow-up edit, cancel, and
  full-list reorder (steers stay pinned on top and are not reorderable). The web
  UI wires all of these.
- Mid-turn steer insertion after completed tool results and before the next
  model request; follow-ups remain next-turn work.
- Turn-level interrupt; idle-only retry/continue (`turn.resume`) for terminal
  model turns; idle-only active-branch switch; idle-only `session.delete`.
- Manual and automatic compaction always use the selected provider's native
  compaction API. Compaction is a typed transcript root, not a session boundary.
  Replayed Anthropic compaction blocks remain opaque and require the provider's
  compaction beta header plus matching strategy edit. Because Anthropic has no
  apply-only mode, ordinary Messages uses a paused trigger at the resolved model
  input ceiling, while token counting uses the documented non-triggering bare
  edit. The ceiling value is schema-valid under Anthropic's documented
  minimum-only rule.
  A paid production Sonnet 5 automatic E2E accepted that replay shape, resumed
  the same blocked action after one native checkpoint, and reduced the
  effective count from the 541,564-token gate to 15,628. Ordinary inline
  compaction blocks still fail closed at the provider boundary.
- Provider/model-aware compaction thresholds through the provider-neutral
  `ModelProvider::model_metadata` contract. OpenAI exact-resolves the selected
  slug from an authenticated, account-scoped private Codex catalog before
  ordinary and compact requests; verified 372k GPT-5.6 windows recommend
  334.8k, while GPT-5.4's 272k current/default window recommends 244.8k rather
  than using its 1M maximum. There is no static OpenAI runtime fallback.
  Anthropic preserves its verified 1M→500k policy. Explicit session values
  remain highest precedence, and absent authoritative metadata leaves only
  reactive overflow recovery. See the
  [provider API audit](provider-api-support.md#account--and-client-version-sensitive-codex-catalog).
- Turn-oriented selected-session loading: collapsed turn cards plus lazy
  per-turn detail, so normal loads do not scale with transcript size, and raw
  `provider_replay` never reaches any UI/RPC response.
- Daemon restart recovery for open transcript tails; stale action rejection via
  persisted `attempt_id`.
- Repo-level `PI.md` prompt composition, with each workspace's `AGENTS.md`
  included by the template.
- Real OpenAI/Codex (ChatGPT subscription transport) and Anthropic API-key
  provider paths, with prompt-cache shaping on both.
- Subagent delegation runs as **delegations** through provider-visible delegation
  tools (`delegate_writing_task`, `delegate_readonly_tasks`,
  `inspect_delegation`, `cancel_delegation`, `steer_subagent`,
  `interrupt_subagent`). A delegation is one **full** subagent
  (writes the parent's workspace in place) or a parallel fan-out of
  **read-only** subagents (each in a disposable btrfs snapshot, destroyed on
  return). The parent parks after launching a delegation and is delivered a
  parent-scoped completion **daemon observation** containing a structured
  snapshot equivalent to `inspect_delegation`, including per-subagent
  `outcome` and compact handoff file references.
  `inspect_delegation` refreshes or recovers that same structured state
  later/running.
  Delegation subagents may emit `subagent.spawned`/`subagent.running` progress
  events, but parent-visible completion is the delegation wakeup observation and
  handoff, not a per-child idle event. Reusable patterns are **workflow skills** (`SKILL.md` +
  `LoadSkill`), not a DSL. Web/inspector RPCs use the canonical
  `delegation.*` client API. See
  [agent-daemon](modules/agent-daemon.md).

Not implemented by design:

- Generic cross-session message routing or an event bus. Subagent delegation is
  bounded parent/child forks (see above), not arbitrary inter-session routing.
- Approval UI or tool permission policy.
- Explicit `open`/`close` or session-level `resume` RPC.
- General plugin/provider marketplace.
- Non-Postgres storage backends. The old in-memory/JSONL store layer was removed
  once the websocket path became Postgres-only.
- Cross-subagent workspace merging. There is one durable workspace with a single
  writer in time; read-only subagents are isolated in throwaway snapshots and
  never merged back.
- Daemon-executed workflow graphs/DSLs and a workflow variable store. Workflow
  control flow lives in parent-interpreted skills.

## Removed Pieces

- `agent-orchestrator` crate.
- `SessionRegistry`.
- Async channel `AgentRunner`.
- The TypeScript fork's hierarchical subagent routing metadata and generic
  control surfaces. The Rust stack instead spawns children as plain forked
  sessions with a small spawn/list/wait/steer/interrupt surface.
- The old in-memory/JSONL `agent-store` layer.

Durable storage is the registry. Process-local state only exists to drive
current work.

## Verification Status

The implementation is checked with full-workspace format/compile/unit tests,
plus the manual websocket exercises in [websocket-rpc.md](websocket-rpc.md)
against a real Postgres database (real Codex text and image-URL turns, daemon
death/restart recovery with snapshot reload, and reconnect event replay).
Anthropic real-provider websocket tests require a raw `ANTHROPIC_API_KEY`.

## Where To Read Next

- Per-crate detail: [`modules/`](modules/).
- Audited provider capabilities and limitations:
  [provider-api-support.md](provider-api-support.md).
- Why the visible/invisible choices were made: [design-decisions.md](design-decisions.md).
- The frontend wire contract and manual exercise plan: [websocket-rpc.md](websocket-rpc.md).
- The React client: [`../../packages/web/docs/web-ui.md`](../../packages/web/docs/web-ui.md).
- In-flight future work: [`plans/`](plans/).
