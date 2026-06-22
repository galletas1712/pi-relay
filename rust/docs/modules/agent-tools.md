# agent-tools

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-tools` owns tool definitions, the provider-aware registry, the execution
context, output bounding, and the builtin tool implementations. [agent-core](./agent-core.md)
only models tool *requests* and *results*; the actual schemas, provider wire
shapes, and filesystem/network behavior live here. The daemon builds one
`ToolRegistry::with_builtin_tools()` registry at startup, renders provider tool
declarations from it on every model request, and dispatches each tool call back
through it.

## Responsibilities

- Define the `AgentTool` trait and the canonical builtin tools.
- Map each canonical tool to one provider-facing form per `ProviderKind`
  (a `ProviderTool`: model-visible name, schema, and the exact JSON sent on the
  wire).
- Canonicalize provider wire names back to internal names for execution.
- Carry the per-call `ToolContext` (session cwd + timeout).
- Bound tool output with a character-budget approximation before it re-enters
  model context.

## Key types

- `AgentTool` — async trait: `definition() -> ToolDefinition` and
  `execute(&ToolCall, &ToolContext) -> ToolResult<ToolResultMessage>`.
- `ToolRegistry` — holds, per `(ProviderKind, name)`: the provider declarations,
  the wire-name aliases, and the boxed executors. Built once and shared as
  `Arc<ToolRegistry>` in daemon state.
- `ProviderTool` — one provider-facing form of a canonical tool. Carries
  `canonical_name` (internal dispatch/transcript key), optional `prompt_alias`
  (PI.md key such as `edit`/`shell`), the model-facing `name`, schema, the raw
  `declaration` JSON, and a `ToolExecution` tag.
- `ToolExecution` — `LocalJson` or `LocalFreeformText`. Both mean pi-relay
  executes the tool locally; the variant only records whether the model emits a
  JSON argument object or a raw freeform payload (the OpenAI `apply_patch`
  grammar).
- `ToolDescriptor` — builder pairing a canonical name with its per-provider
  `ProviderTool`s and per-provider executors. `ToolExtension` /
  `FirstPartyToolExtension` register descriptors; an extension registers once,
  keyed by its `id`.
- `ToolContext` — `{ cwd: PathBuf, timeout: Duration }`, default timeout 30s.
- `ToolError` / `ToolResult` — unknown tool, invalid args, IO, timeout, edit
  target not found, invalid input.
- `tool_display` — replay-display labels (pretty name + a one-line input
  summary) for transcript rendering.
- `limit_tool_output*` — output bounding (see Notes).

## Registered tools

`with_builtin_tools()` registers the canonical first-party tools through
`FirstPartyToolExtension`. They all execute inside pi-relay; some have registry
executors and some are intercepted by the daemon runtime before registry
execution. "Runtime-handled" means the tool is declared to the provider and
reported by `tools.list`, but `run_tool_turn` handles the call before the
registry executor table. The model-visible form differs only where provider
semantics justify it.

| Canonical    | OpenAI form                               | Anthropic form                                  | prompt_alias       | Executor                         |
|--------------|-------------------------------------------|-------------------------------------------------|--------------------|----------------------------------|
| `Edit`       | `apply_patch` (custom Lark grammar)       | `str_replace_based_edit_tool` (`text_editor_20250728`) | `edit`      | `ApplyPatchTool` / `TextEditorTool` |
| `Bash`       | `Bash` (JSON function)                    | `Bash` (JSON client tool)                       | `shell`            | `BashTool`                       |
| `Grep`       | `Grep` (JSON function)                    | `Grep` (JSON client tool)                       | `workspace_search` | `GrepTool`                       |
| `WebSearch`  | `web_search` (JSON function)              | `web_search` (JSON client tool)                 | `web_search`       | `WebSearchTool`                  |
| `WebFetch`   | `web_fetch` (JSON function)               | `web_fetch` (JSON client tool)                  | `web_fetch`        | `WebFetchTool`                   |
| `LoadSkill`  | `LoadSkill` (JSON function)               | `LoadSkill` (JSON client tool)                  | `skill_loader`     | runtime-handled (no registry executor) |
| `delegate_writing_task` | `delegate_writing_task` (JSON function) | `delegate_writing_task` (JSON client tool) | `delegation` | runtime-handled (no registry executor) |
| `delegate_readonly_tasks` | `delegate_readonly_tasks` (JSON function) | `delegate_readonly_tasks` (JSON client tool) | `delegation` | runtime-handled (no registry executor) |
| `inspect_delegation` | `inspect_delegation` (JSON function) | `inspect_delegation` (JSON client tool) | `delegation` | runtime-handled (no registry executor) |
| `cancel_delegation` | `cancel_delegation` (JSON function) | `cancel_delegation` (JSON client tool) | `delegation` | runtime-handled (no registry executor) |
| `steer_subagent` | `steer_subagent` (JSON function) | `steer_subagent` (JSON client tool) | `delegation` | runtime-handled (no registry executor) |

There are no `read`/`write` tools. File reads go through `Edit`'s `view`
command (Anthropic) or through `Bash` (`cat`, `sed`, `rg`, …) on OpenAI.

### edit — provider-native

The only tool where the two providers see structurally different schemas,
because both providers are trained on a specific edit surface and paraphrasing
either as a generic function tool would discard that prior.

- **OpenAI** — `apply_patch`, an OpenAI `custom` tool whose input is a Lark
  grammar (`APPLY_PATCH_LARK_GRAMMAR`, re-exported from the crate root). The
  model emits a raw `*** Begin Patch … *** End Patch` body, not JSON
  (`LocalFreeformText`), so large diffs escape JSON-string quoting.
  `ApplyPatchTool` parses the patch in-process — Add / Delete / Update (with
  optional `*** Move to:`) — applies hunks by exact-substring match against file
  contents, and returns a compact `A`/`D`/`M`/`R` change summary. No external
  `apply_patch` binary is spawned; a missing file or unmatched hunk is an error
  `ToolResult`.
- **Anthropic** — `str_replace_based_edit_tool` declared as
  `text_editor_20250728`. `TextEditorTool` implements `view` (file or directory
  listing, optional `view_range`), `create`, `str_replace` (first occurrence;
  missing target is `EditTargetNotFound`), and `insert`. `undo_edit` is not
  offered.

### bash — uniform custom function

One `BashTool`, registered identically as `Bash` for both providers from a
single definition. pi-relay does **not** use Anthropic's native
`bash_20250124`: that wire shape advertises a persistent shell session with a
`restart` op, and the runtime is stateless — each call spawns a fresh
`bash -lc` rooted at the session cwd. The schema is kept honest about what the
runtime can back. There is no `workdir` override; the model chains with `&&` or
calls `cd` inline (the announced cwd lives in the dynamic prompt context). The
default timeout is `ToolContext`'s 30s; the model may pass `timeout_ms` and
`max_output_tokens` per call. Output is `exit: … / stdout: … / stderr: …`;
non-zero exit returns an error `ToolResult`.

### grep — uniform custom function

One `GrepTool`, registered identically as `Grep` for both providers; inputs are
small enough that a strict JSON schema costs little. Shells out to `rg` directly
(no shell interpolation) with `--line-number --column --hidden --glob !.git`,
rooted at the session cwd, returning paths relative to it. `path` is normalized
(absolute paths under cwd are made relative, `./` components stripped).
Optional `case_sensitive`, `context`, `max_matches`. `rg` exit code 1 (no
matches) is still a success result.

### web_search / web_fetch — local JSON wrappers

Registered as ordinary JSON tools for both providers (`web_search`,
`web_fetch`) so the main model turn always sees client-executed tools, keeping
transcript replay and token accounting on one surface. They are dispatched
specially by the daemon runtime rather than through the registry's executor
table.

- `WebFetchTool` performs a real `reqwest` GET (5-redirect limit, `ToolContext`
  timeout, `pi-relay-web-fetch/*` UA), HTML-to-text strips `script`/`style`,
  tags, and basic entities, and returns bounded text with URL/status/content-type
  headers; an optional `prompt` is echoed for the caller's intent.
- `WebSearchTool` currently has no configured backend: it validates the query
  and returns an error `ToolResult` explaining that no web-search backend is
  wired. The provider-neutral wrapper shape lets a real backend (or a
  provider-native sidecar call) drop in later without changing the model surface.

### LoadSkill

`LoadSkill` activates an available skill by name (optionally scoped to a
workspace dir) so its instructions are injected into model context. It is
registered as a provider tool for declaration/replay, but has no registry
executor — the daemon runtime intercepts `tool_name == "LoadSkill"` and resolves
it against the session's loaded-skill set and workspace skills.

### delegation tools

`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`,
`cancel_delegation`, and `steer_subagent` are provider-visible JSON tools registered by
`FirstPartyToolExtension`, but they have no registry executor. The daemon
runtime intercepts them and dispatches to the delegation engine in
`delegation_tools.rs`.
`delegate_writing_task` launches the single full/writing delegation subagent;
`delegate_readonly_tasks` launches a homogeneous fan-out of read-only
subagents; `inspect_delegation` returns the canonical structured state/outcome
snapshot (with artifact paths, not inline full transcripts);
`cancel_delegation` cancels an existing delegation; `steer_subagent` queues an
additional instruction to a running full subagent. Delegation subagents may produce
`subagent.spawned`/`subagent.running` progress events, but delegation completion
arrives later as a parent steer containing an `inspect_delegation`-equivalent
snapshot plus artifact paths, not as a model tool result or per-child idle event.

Their internal delegation types, handoff `delegation_id`, and web/inspector RPC methods
(`delegation.start_full`, `delegation.start_readonly_fanout`, `delegation.status`,
`delegation.cancel`, `delegation.list`, `delegation.read_handoff_file`) are client JSON-RPC APIs for the
web/inspector surface, not provider-visible tool names.

## How it works

```
model emits tool call (provider wire name, e.g. "apply_patch")
  -> daemon: ToolContext::new(session outer_cwd)   [timeout 30s]
  -> LoadSkill?  -> runtime skill loader (no registry executor)
  -> web tool?   -> runtime web dispatch (WebSearch/WebFetch)
  -> delegation? -> runtime delegation dispatch (no registry executor)
  -> else        -> registry.execute(provider, call, ctx)
                      canonical_tool_name_for_provider() maps wire name
                      -> canonical name  (apply_patch -> Edit)
                      -> per-(provider, canonical) executor.execute()
  -> ToolResultMessage (success | error) fed back into the session
```

Provider tool declarations are produced from the same registry:
`provider_tools_for_provider(kind)` returns the `ProviderTool`s (sorted by
model-facing name) that the provider layer renders into the request, and that
the [`tools.list`](../websocket-rpc.md) RPC and `system.prompt` surface report.
`definitions_for_provider(kind)` exposes the canonical-named `ToolDefinition`s.

Registration uses provider-keyed maps. An alias table maps both the canonical
name and the model-facing name (per provider) back to the canonical name, so an
incoming `apply_patch` (OpenAI) or `str_replace_based_edit_tool` (Anthropic)
call dispatches to the `Edit` executor for that provider.

## Notes

- **Always allowed, unsandboxed.** Tools run under the session `outer_cwd` as
  ordinary local processes/filesystem ops. There is no approval interface,
  approval state, or sandbox — these are personal-use primitives. See design
  decisions [No Approval UI](../design-decisions.md#no-approval-ui).
- **Posture buckets and the rationale** (uniform custom function vs.
  provider-native vs. local wrapper) are recorded in design decisions
  [Provider Tool Surfaces Diverge Only When Semantics Justify It](../design-decisions.md#provider-tool-surfaces-diverge-only-when-semantics-justify-it).
- **Output bounding.** `limit_tool_output` caps results at
  `DEFAULT_MAX_TOOL_OUTPUT_TOKENS` (10k) via a 4-chars-per-token approximation
  (the crate carries no tokenizer). Over-budget output keeps a 3/5 head and 2/5
  tail with a `[tool output truncated: N characters omitted]` marker on a char
  boundary. `Bash`, `WebSearch`, and `WebFetch` honor a per-call
  `max_output_tokens` override.
- **`ProviderKind` is `{ OpenAi, Claude }`.** OpenAI always routes through the
  ChatGPT/Codex subscription transport; "codex" is an auth transport, not a
  provider kind.
- **Thinking is not a tool concern.** `AssistantItem` is `{ Text, ToolCall }`;
  thinking blocks are dropped at the provider parse layer, so they never reach
  this crate.
- **Future direction** — persistent-shell runtime, worktree/path scoping below
  the model-visible schema, native provider web/server tools, and MCP — is not
  implemented here. See [tool surface](../plans/tool-surface.md).
