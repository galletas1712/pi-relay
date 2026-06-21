# Tool Surface Evolution

Status: planned. Last reviewed 2026-06-07.

## Motivation

The tool surface is deliberately small and mostly provider-neutral today:
`Bash`, `Grep`, the web wrappers, `LoadSkill`, and the delegation tools are
uniform JSON tools, and only edit diverges to provider-native schemas. That
keeps dispatch, caching, and replay simple, but it leaves quality on the table
in four places:

- **Shell.** Bash is a stateless `bash -lc` per call. The model cannot keep a
  working directory, environment, or background process alive across calls, and
  pi-relay cannot expose a provider-native shell that promises persistence
  (Anthropic's `bash_20250124` carries `restart` semantics the runtime cannot
  honor).
- **Web.** `web_search` and `web_fetch` are local wrappers. `web_search` has no
  backend wired up at all and currently returns an error placeholder; `web_fetch`
  is a real but plain HTTP-and-strip-tags fetch. Both providers already host far
  better search/fetch tools server-side.
- **MCP.** There is no way to expose external toolsets to the model.
- **Workspace state.** Sessions run against a single `cwd` with no worktree
  isolation or cloud sandbox option.

This plan covers only the unimplemented direction. The implemented baseline is
documented in [agent-tools](../modules/agent-tools.md).

## What exists today

See [agent-tools](../modules/agent-tools.md) for the full registry. In brief:

- **edit** is the only provider-native tool: OpenAI sees `apply_patch`
  (freeform Lark grammar) and Anthropic sees `str_replace_based_edit_tool`
  (`text_editor_20250728`), both executed by local runtimes.
- **Bash** is one uniform JSON function/client tool for both providers, backed
  by a stateless `bash -lc` subprocess rooted at the session `cwd`.
- **Grep** is one uniform JSON function/client tool for both providers, backed
  by ripgrep.
- **web_search** / **web_fetch** are uniform JSON local wrappers (web_search has
  no backend; web_fetch does a bounded HTTP fetch).
- **LoadSkill** activates a named skill.
- **Delegation tools** (`delegate_writing_task`, `delegate_readonly_tasks`,
  `inspect_delegation`, `cancel_delegation`) are uniform JSON tools handled by
  the stage runtime; stage completion is reported later via a handoff steer.

Tools are registered through `ToolDescriptor` / `ProviderTool` in
`agent-tools/src/registry.rs`, which already separates the model-visible
provider form (`ProviderTool.declaration`) from local execution
(`ToolExecution::{LocalJson, LocalFreeformText}`). That split is the seam the
work below builds on — none of it requires reworking the registry shape.

See [design decisions](../design-decisions.md) § "Provider Tool Surfaces Diverge
Only When Semantics Justify It" for why bash stayed uniform JSON while edit went
native.

## Proposed work

### 1. Provider-native shell, once a persistent shell runtime exists

The blocker is the runtime, not the wire shape. Provider-native shell tools
assume a session that survives across calls:

- OpenAI `shell` emits `shell_call` actions with a list of commands and a
  continuation `shell_call_output`.
- Anthropic `bash_20250124` carries a `restart` flag that resets a *persistent*
  bash session.

Neither maps onto a one-shot `bash -lc`. The work is to build a persistent
terminal runtime first, then reconsider the wire shape:

```
PersistentTerminal(SessionId, Worktree)
  - long-lived shell process per session
  - command start, stdin write/poll
  - timeout, output truncation
  - restart / cleanup
```

With that runtime in place:

- Anthropic `bash_20250124` (name `bash`) becomes honest: `restart` resets the
  terminal session.
- OpenAI `shell` can be reconsidered. Note the ChatGPT/Codex subscription
  transport rejected the platform `{ "type": "shell" }` shape with
  `Unsupported tool type: shell`. The fallback is to keep the model-visible name
  `shell` but render it as a function tool for that transport, and parse
  `shell_call` defensively in case a future transport accepts the native shape.

Both adapters call the same `PersistentTerminal`; the session FSM only sees
"local tool requested / tool result appended" and never inspects which provider
wire shape produced the call. Until this runtime lands, keep bash as the uniform
stateless JSON tool — do not advertise persistence the runtime cannot deliver.

### 2. Provider-hosted web tools instead of local wrappers

Replace the local web wrappers with the providers' own server-hosted tools where
they fit:

- OpenAI hosted `web_search` (`{ "type": "web_search", "search_context_size":
  "high" }`), which emits `web_search_call` items with `search`, `open_page`,
  and `find_in_page` actions plus citations. OpenAI has no separate top-level
  `web_fetch`; page open / in-page find are hosted `web_search` actions.
- Anthropic `web_search_20250305` and `web_fetch_20250910`
  (`citations.enabled: true`), which emit `server_tool_use` and
  `web_search_tool_result` / `web_fetch_tool_result` blocks.

These are **provider-hosted**, not local client tools. The defining constraint:

```
provider runs the tool inside the model request
  -> response carries hosted tool-use + result blocks
  -> pi-relay stores them in provider replay
  -> UI renders them as transcript decorations (like tool cards)
  -> NO local pending action / ActionRequested row is created
```

This requires the provider output parser to grow a category for hosted tool
events that is distinct from local tool calls, so hosted events stay append-only
replay data and never enter the session FSM as pending actions. Keeping the
local `web_fetch` runtime is still worthwhile as a fallback for parity or for
authenticated / local-network fetches, but it should not be the default once
hosted tools are wired.

Effort defaults stay high and stable, set by pi-relay at render time, not by the
model: OpenAI `search_context_size: high`; Anthropic web tools omit `max_uses` /
`max_content_tokens` by default; web_fetch keeps `citations.enabled: true`. Add
provider `max_*` caps only for explicit workflow budgets — they are truncation
guardrails, not quality knobs.

### 3. MCP tool integration

Two modes:

```
Local MCP client mode (preferred for personal use):
  pi-relay runs an MCP client, discovers tools,
  renders them through provider-specific tool profiles,
  and executes calls against the local MCP server.

Provider-hosted MCP mode:
  the provider connects to a remote MCP server directly;
  pi-relay only observes hosted MCP events.
```

Local client mode is the better default: it supports stdio / local servers and
keeps credentials under pi-relay control. Provider-hosted MCP only reaches public
remote servers and complicates the credential boundary. Defer both until there is
a real MCP server to support, and add OpenAI `tool_search` / Anthropic
`defer_loading` only once the tool catalog is large enough to justify deferred
loading.

### 4. Worktree scoping and cloud sandboxes

The persistent terminal and any future file runtimes should be scoped to a
per-session worktree rather than a bare `cwd`, giving session-level isolation for
process and filesystem state. Cloud sandboxes (provider hosted shell, code
execution, computer use) remain explicitly excluded from the default coding
profile; they can be separate opt-in drivers later but should never be enabled by
default.

## Open questions

- **OpenAI native shell shape.** Does any subscription-auth transport accept the
  platform `{ "type": "shell" }` item, or is the function-tool rendering
  permanent? The persistent-terminal runtime is independent of this answer.
- **Local `web_fetch` after hosted tools land.** Keep it as a fallback for
  authenticated / local-network fetches and OpenAI parity (OpenAI has no hosted
  `web_fetch`), or retire it entirely?
- **Hosted-event continuity.** Provider replay must preserve hosted tool events
  exactly enough to reconstruct the next stateless provider request across
  switch, compaction, and retry. The exact replay representation for hosted
  search/fetch blocks is undecided.
- **Concurrency.** A persistent terminal is stateful, so tool calls against it
  must serialize (Anthropic `tool_choice` would start with
  `disable_parallel_tool_use: true`). Read-only `grep` could tolerate
  concurrency — worth a per-runtime concurrency policy rather than a global lock.
