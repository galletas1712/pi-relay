# agent-prompt

> Part of the [Rust Agent Stack](../architecture.md) | [Design decisions](../design-decisions.md)

`agent-prompt` renders the global system prompt from the repo-level `PI.md` template. It is a pure rendering crate: it takes a template string plus a `PromptContext` describing the session (cwd, workspaces, available tools, discovered skills) and returns the final prompt text. It does not read session storage, talk to providers, or inject any implicit runtime data — the template decides what appears.

## Responsibilities

- Load the repo-level templates `PI.md` and `PI.compaction.md` from a given repo root.
- Render either template against a `PromptContext` using [minijinja](https://docs.rs/minijinja) (Jinja2 syntax).
- Expose a small, stable set of template variables (project instructions, workspace facts, tool specs/aliases, skills index) and nothing else.
- Compose per-workspace `AGENTS.md` files into a single project-instructions block.
- Collapse runs of blank lines so conditional template blocks do not leave large gaps.

This crate does not decide *when* the prompt is rendered or how it reaches a provider. The daemon does that (see [How it works](#how-it-works)).

## Public surface

```
load_pi_md(repo_root)            -> reads <repo_root>/PI.md
load_pi_compaction_md(repo_root) -> reads <repo_root>/PI.compaction.md
render_prompt(template, ctx)            -> String
render_compaction_prompt(template, ctx) -> String   (same renderer, separate entry point)
```

Input types the caller fills in:

- `PromptContext` — `cwd: PathBuf`, `has_project: bool`, `workspaces: Vec<PromptWorkspace>`, `tools: Vec<ToolSpec>`, `skills: Vec<Skill>`.
- `PromptWorkspace` — `kind` (`Git` | `Local`), `workspace_dir`, and git lineage fields (`remote_url`, `remote_branch`, `source_path`, `base_sha`, `local_branch`).
- `ToolSpec` — `name`, `description`, `input_schema` (JSON), `canonical_name`, `prompt_alias`.
- `Skill` — optional `workspace`, `name`, `description`, `file_path`. Built via `Skill::global(..)` or `Skill::workspace(workspace, ..)`.

## Template variables

`PI.md` is rendered with exactly these variables. Anything not listed here is not available to the template.

| Variable | Type | Contents |
| --- | --- | --- |
| `project.agents_md` | string | Concatenated per-workspace `AGENTS.md` content. Empty when `has_project` is false. |
| `session.cwd` | string | The session `cwd`, with backslashes normalized to `/`. |
| `session.has_project` | bool | Whether the session is attached to a project. |
| `session.workspaces` | array | Per-workspace objects (`kind`, `workspace_dir`, git lineage fields). |
| `session.workspaces_markdown` | string | Human-readable bullet list of workspaces (see below). Falls back to a "No project workspaces" sentence when empty. |
| `tools.specs` | string | Markdown for every tool (`### name`, description, `Parameters:` JSON block), sorted by name. Falls back to "No tools are currently available." when empty. |
| `tools.aliases` | object | Map of `prompt_alias -> tool name`, used to reference tools by role. |
| `skills.index` | string | Pretty JSON object with an `available_skills` array, or empty string when there are no skills. |

`tools.aliases` lets the template name a tool by its role rather than its concrete name, which differs per provider. The template uses minijinja `default(...)` so it still renders when an alias is absent, e.g.:

```jinja
Use `{{ tools.aliases.workspace_search | default(value="Grep") }}` instead of calling `grep` directly.
```

The registered builtin tools and their aliases are owned by the daemon tool registry, not this crate. Current tools: `edit` (rendered as `apply_patch` for OpenAI, `text_editor_20250728` for Anthropic), `bash` (uniform JSON `Bash`), `grep` (uniform JSON `Grep`), `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`). See [agent-tools](./agent-tools.md) and [websocket-rpc](../websocket-rpc.md) (`tools.list`).

## How it works

### Workspaces -> project instructions

When `has_project` is true, `agents_md` is built by reading `<cwd>/<workspace_dir>/AGENTS.md` for each workspace, in order. Missing or whitespace-only files are skipped. Each surviving file becomes a section headed by its workspace dir:

```
### repo

<contents of repo/AGENTS.md>

### docs

<contents of docs/AGENTS.md>
```

This is the only place project-specific instructions enter the prompt. There is no separate global instructions file and no recursive scan — only the AGENTS.md at each workspace root is read.

### `workspaces_markdown`

Git and local workspaces render differently so the model knows the publish posture of each directory:

```
- repo
  - type: Git
  - remote: https://example.com/repo.git
  - starting branch: main
- vendor
  - type: local folder copy
```

`PI.md` gates this whole block (and the workspace prose) behind `session.has_project`.

### Skills index

`skills.index` emits pretty JSON for discovered skills, sorted by the model-facing `name`:

```json
{
  "available_skills": [
    {
      "name": "repo/repo-build",
      "description": "Use for repo build issues."
    },
    {
      "name": "rust-refactor",
      "description": "Use for Rust refactors."
    }
  ]
}
```

Workspace-scoped skills are exposed to the model with their workspace directory
as a prefix (`workspace/name`); skills without a slash prefix are global.
`LoadSkill` should be called with the exact `name` from this JSON. JSON escaping
is handled by `serde_json`. With no skills the variable is the empty string, and
`PI.md` drops the entire Skills section via `{% if skills.index %}`.

Skill *discovery* (scanning `~/.agents/skills` and each workspace root's `.agents/skills`, parsing SKILL.md frontmatter) lives in the daemon's `provider_runtime`, not this crate. The crate only formats the `Skill` list it is handed.

### Render and cleanup

`render` builds a fresh minijinja `Environment` per call, registers the template under the name `prompt`, and renders it against `template_context`. Template parse/render failures panic — these are repo-authored templates, not user input. After rendering, `compact_blank_lines` trims trailing whitespace per line and collapses any run of blank lines to at most two, then trims the ends. `render_compaction_prompt` is the same renderer pointed at `PI.compaction.md`.

### Where the daemon plugs in

```
session.start (workspaces materialized)
  -> render_pi_prompt: load_pi_md(prompt_root) + render_prompt(ctx)
  -> stored once in SessionConfig.system_prompt
        |
        v
model call: assemble_agent_prompt
  -> PromptSections.stable_prefix = config.system_prompt
  -> normal turns do not append delegation dashboard context
  -> agent-provider renders stable prefix, then transcript history

compaction call:
  -> stable prefix + transcript/model history
  -> provider summarizes without live delegation dashboard input
  -> after provider return, top-level parent sessions append
     "## Delegation state at compaction time" to the stored summary
```

The prompt is rendered exactly once, at `session.start`, after project workspaces are materialized (so AGENTS.md and skills are present on disk). The rendered string is persisted in `SessionConfig.system_prompt` and is the session's immutable global prompt. The `/system` RPC (`system.prompt`) re-renders the same template to show the prompt and its source for a selected session.

## Stable prefix vs dynamic context

The rendered `PI.md` is the **stable prefix** of [agent-provider](./agent-provider.md)'s `PromptSections`. The daemon's `assemble_agent_prompt` wraps the stored `system_prompt` as the stable prefix for ordinary model turns. It does not inject `## Current delegations` or any other delegation dashboard into normal parent turns; those requests are driven by stable prompt + transcript history, where durable delegation tool results and typed wakeup observations already live.

Compaction is the exception, but the ledger is appended after the provider
returns rather than sent as compaction input. For top-level parent sessions, the
daemon stores the provider summary plus a fresh
`## Delegation state at compaction time` section. That ledger lists every
delegation row for the parent session (running, done, done_with_failures,
cancelled, failed) with bounded progress/subagent fields, `outcome`
control data when available, and artifact paths. It deliberately does not inline
full transcripts or final-message prose, or refresh handoff artifacts. Running entries are
point-in-time facts; summaries must not assume they completed before a later
completion observation or refreshed `inspect_delegation`. Future compactions summarize
prior summary text normally, including any older point-in-time ledgers, then
append a fresh ledger again. The latest appended ledger supersedes older ledger
text by position.

Subagent compactions do **not** receive the parent delegation ledger, sibling
subagent state, or `## Current delegations` information. A subagent summary is
limited to the subagent's own stable prompt/role contract, delegated task,
transcript/model history, and own tool results/facts.

## Notes

- The `agent-prompt` crate itself injects no date, time, or cwd implicitly. If `PI.md` does not reference `session.cwd`, the rendered stable prefix never includes it. Any daemon-owned volatile context is added later by `agent-daemon`.
- Because the template can choose to surface `session.cwd` / `session.workspaces_markdown` / `tools.specs`, a custom template *can* place dynamic-looking data in the rendered text — but that text is still part of the stable prefix and will churn the prompt cache. Keep volatile data out of `PI.md`; when dynamic context is needed for ordinary model calls, providers append it after transcript history. Delegation state is different: parent-session delegation ledgers are appended to the stored compaction summary after provider compaction returns, not sent as compaction input.
- The prompt is rendered once and stored; editing `PI.md` does not retroactively change existing sessions. New sessions pick up the new template.
- `render` panics on a malformed template. This is intentional for a repo-authored file; do not feed untrusted templates through this crate.
- Thinking/reasoning blocks never reach the prompt. The provider parse layer keeps only `Text` and `ToolCall` assistant items, so the prompt has no notion of reasoning content.
