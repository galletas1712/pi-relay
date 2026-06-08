# Subagent Workflows

Status: planned. Last reviewed 2026-06-07.

## Motivation

pi-relay has a branch-aware, durable single-session runtime, but it still lacks
the recursive "agent coordinates other agents" workflow that Claude Code
workflows and recursive language-model experiments make useful:

- spawn multiple agents with different roles,
- let each child keep its own context window,
- inspect child transcripts and filesystem diffs from the parent,
- inject follow-ups or interrupts into children,
- route information from one child to another, and
- merge useful child work back into the parent intentionally.

There is also a related workflow that should not be modeled as a subagent: a
session may discover adjacent work that should become a new visible top-level
session, seeded with context from the current transcript, but not controlled by
the current agent as part of the current task.

The workflow layer should primarily move bounded context and structured results
between sessions. A Python REPL is one possible interface, but it is not the
minimal primitive. The simpler core is a daemon-owned workflow bus: variables,
agent/session handles, context templates, result schemas, and automatic
fanout/fanin execution.

The new workspace materialization path in `main` removes the biggest filesystem
blocker for top-level project sessions: project workspaces are refreshed into
daemon-owned bases, then session workspaces are instantiated with Btrfs
subvolume snapshots when available and reflink/copy fallback otherwise. This
plan describes what remains to build subagents, related sessions, and workflow
orchestration on top of that baseline.

This is a Rust-only plan. The old TypeScript subagent model is not a dependency
and should not be ported.

## What exists today

### Session and workspace baseline

- `session.start` creates a new session row, session config, first input,
  transcript entries, actions, and events in one start transition.
- Project sessions snapshot the project's workspace definitions into
  `SessionConfig.workspaces` at creation.
- `WorkspaceManager::materialize_session` creates
  `$XDG_STATE_HOME/pi-relay/sessions/<session_id>/cwd`.
- Workspace bases live under
  `$XDG_STATE_HOME/pi-relay/workspace-bases/<project_id>/<workspace>/base`.
- Git bases are refreshed with `git fetch`, detached checkout,
  `reset --hard`, and `clean -ffdx`.
- Local-folder bases are refreshed with `rsync --delete` plus symlink/tree
  sanitization.
- Session workspace instantiation prefers `btrfs subvolume snapshot`, falls
  back to a Btrfs subvolume populated by reflinks, and finally falls back to a
  recursive copy.
- Git session workspaces switch to a per-session local branch
  `pi/session/<session>/<workspace>`.

This is the right behavior for **new top-level sessions**: sync the project
bases first, then snapshot/copy into an isolated session workspace.

### Runtime baseline

- `AgentSession` is still a single-session FSM.
- `SessionDriver` is the daemon facade for recovering, driving, dispatching,
  and persisting one session.
- Model/tool dispatches are spawned concurrently by `dispatch_all`.
- Tool execution currently receives only a `ToolContext` with `cwd`; tools that
  need daemon state (`LoadSkill`, hosted web paths) are special-cased in
  `runtime/tool.rs`.
- `input.follow_up`, `input.interrupt`, history operations, turn resume, and
  compaction already provide most of the primitives needed to steer a child
  session once it exists.

### Skill baseline

- Skills are discovered from `$HOME/.agents/skills/*/SKILL.md` and from each
  session workspace's `.agents/skills/*/SKILL.md`.
- `LoadSkill` injects one selected `SKILL.md` into the current model context.
- This is close enough to use as the first role-definition surface for
  subagents. A separate `.agents/roles` directory can be added later if roles
  need different frontmatter or lifecycle semantics.

## Goals

1. Let a parent session spawn child sessions with named roles and task prompts.
2. Fork child filesystem state from the parent's **current** session `cwd`,
   without refetching, pulling, or refreshing project bases.
3. Keep child context append-only from the parent's perspective: the parent can
   inspect and append messages, but cannot rewrite child history.
4. Let the parent wait on, interrupt, stop, and steer children the same way a
   user can steer a normal pi-relay session.
5. Expose orchestration through daemon-owned workflow primitives first:
   variables, context templating, agent/session spawn, wait, result capture, and
   context forwarding.
6. Support dynamic workflows that can fan out, wait, collect structured results,
   and launch follow-up agents without requiring the top-level model to
   intervene after each step.
7. Make merge-back explicit and reviewable before any child filesystem changes
   are applied to the parent.
8. Support visible related top-level sessions for adjacent work that should be
   seeded from the current transcript but not controlled as a subagent.
9. Keep Python as an optional ergonomic adapter over the daemon primitives, not
   the source of truth.

## Non-goals

- Do not port the TypeScript subagent implementation.
- Do not add a security sandbox in the first version.
- Do not refetch or pull remotes when spawning a subagent.
- Do not auto-merge child changes into the parent without an explicit parent
  action.
- Do not let the parent mutate existing child transcript entries.
- Do not require web UI changes for the first usable Rust implementation.
- Do not force every spawned session into a hidden parent-controlled subagent
  relationship.

## Proposed architecture

### 1. Session relationship metadata and graph

Add a durable session graph in `agent-store`. A dedicated table is better than
hiding everything in `SessionConfig.metadata` because the daemon needs to query
subagents/related sessions by origin, recover them after restart, enforce
limits, and expose status without scanning arbitrary JSON.

Suggested shape:

```text
session_relationships
  relationship_id       primary key
  source_session_id     references sessions(session_id)
  target_session_id     references sessions(session_id)
  root_session_id
  kind                  subagent | related | related_fork
  control_mode          parent_controlled | none
  visibility            hidden | nested | top_level
  role_name
  role_workspace        nullable
  display_name          nullable
  task                  text
  spawned_at
  spawned_from_leaf_id  nullable
  spawned_from_action_row_id nullable
  workflow_id           nullable
  result_variable       nullable
  status                running | idle | stopped | completed | error
  filesystem_mode       btrfs_snapshot | reflink_copy | plain_copy
  baseline_cwd          nullable path
  metadata              jsonb
```

The target is still a normal pi-relay session internally. The graph records
provenance and control semantics:

- `subagent`: current-task helper, parent-controlled, usually hidden/nested.
- `related`: adjacent same-project top-level session, not parent-controlled.
- `related_fork`: adjacent same-project top-level session forked from the
  origin `cwd`, not parent-controlled.

Visibility policy can be decided separately:

- subagents: hide from normal `session.list` unless explicitly asked, or show
  nested under the parent in a later UI,
- related sessions: always show as normal top-level sessions in the same
  project,
- developer/debug mode: optionally show all relationships with metadata.

### 2. Role resolution

For the first version, reuse `SKILL.md` files as role definitions:

```text
$HOME/.agents/skills/<role>/SKILL.md
<workspace>/.agents/skills/<role>/SKILL.md
```

`agents.spawn(role="reviewer", task="...")` should resolve the same names that
`LoadSkill` exposes. The role content becomes child startup instructions, not a
parent transcript mutation.

Child system prompt construction:

```text
base PI.md render for the child session
+ subagent contract:
   - you are a child agent
   - parent can inspect transcript and send follow-ups
   - report concise status/results
   - do not assume your changes are merged automatically
+ role SKILL.md content
```

Open question: whether roles should eventually move to `.agents/roles`. That is
not necessary for v1.

### 3. Filesystem fork for subagents

Top-level project sessions already refresh bases before snapshot/copy. Subagent
spawn must do something different:

```text
parent current cwd  ──CoW/copy──► child cwd
                  └─CoW/copy──► child baseline cwd
```

Rules:

- Never refresh project bases during subagent spawn.
- Never fetch, pull, reset, or clean remotes during subagent spawn.
- Fork the parent's current `outer_cwd` exactly, including dirty tracked files,
  untracked files, generated files, and local workspace state.
- Preserve a stable spawn baseline so later merge-back can compare:

```text
B = child baseline cwd at spawn
P = parent cwd at merge time
C = child cwd at merge time
```

Implementation work:

- Add `WorkspaceManager::fork_session_from_parent(...)`.
- Reuse the existing Btrfs/reflink/copy helpers, but source from the parent
  session `cwd` rather than from workspace bases.
- Create both:
  - child working `sessions/<child>/cwd`
  - child baseline, for example `sessions/<child>/baseline-cwd`
- If Btrfs is available, prefer subvolume snapshots for both. The baseline can
  be marked read-only later; v1 can simply avoid exposing it as a tool cwd.
- If Btrfs/reflink is unavailable, fall back to plain copies. This is slower but
  keeps semantics correct.

After copying, validate and retarget Git workspaces:

- For every copied Git workspace, ensure `git rev-parse --git-dir` and
  `git rev-parse --git-common-dir` resolve inside the child workspace. A copied
  worktree whose `.git` file still points to parent state is not isolated.
- Switch each child Git workspace to
  `pi/session/<child_session_id>/<workspace>`.
- Do not change the parent workspace.
- Store additional spawn provenance per Git workspace if needed:
  `spawn_head_sha`, `spawn_branch`, and whether the spawn baseline was dirty.

### 4. Filesystem barriers

Subagent spawn must not race with parent tools mutating files.

Current dispatch can spawn multiple model/tool actions concurrently. A workflow
tool call that forks the filesystem while a sibling `Bash` or `Edit` call is
running can capture an inconsistent copy.

Required v1 barrier:

- Add a per-session filesystem operation lock or an exclusive "fork barrier".
- Ensure subagent filesystem fork runs only when no other filesystem-mutating
  action for that parent is active.
- If implementing the full lock is too large for v1, make `agents.spawn` fail
  with a clear error when there are other running actions in the parent session:
  "subagent spawn requires an exclusive filesystem point; retry after sibling
  tools finish."

Longer term, classify tools as read-only, filesystem-mutating, or
daemon-only-workflow so the runtime can safely parallelize read-only work while
serializing mutation/fork operations.

### 5. Subagent manager

Add an `agent-daemon/src/subagents/` module that owns the orchestration
operations and uses existing session primitives internally.

Core operations:

```text
spawn(parent_session_id, role, task, options) -> child_session_id
send(child_session_id, message, priority) -> queued/accepted input
wait(child_session_id, condition, timeout) -> status/events
tail(child_session_id, limits) -> transcript excerpt
transcript(child_session_id, scope, limits) -> transcript view
interrupt(child_session_id, optional_message) -> interruption result
stop(child_session_id) -> stopped
diff(child_session_id) -> child diff against baseline
merge_preview(child_session_id) -> proposed parent patch/conflicts
apply_merge(child_session_id, strategy) -> result
```

`spawn` should not call public `session.start` unchanged, because
`session.start(project_id=...)` materializes from refreshed project bases.
Instead, factor the session-start logic so subagents can provide:

- a pre-forked `outer_cwd`,
- inherited `SessionWorkspace` values, with child local branches retargeted,
- parent-linked metadata,
- a role-augmented system prompt,
- the initial task input.

The child then becomes a normal active `RuntimeSession` driven by
`SessionDriver`.

### 6. Parent read and control semantics

The parent needs strong but simple permissions:

- Parent may read child status, transcript excerpts, and filesystem diffs.
- Parent may append user messages to the child via the same input path as
  `input.follow_up`.
- Parent may interrupt/stop child work via existing interrupt/cancel paths.
- Parent may not edit prior child transcript entries.
- Child may not implicitly edit parent context or filesystem.
- Merge-back is a separate explicit operation.

This preserves transcript integrity while giving the parent the same steering
power a user has.

### 7. Related top-level sessions

Not every spawned session is a child task. Sometimes the current session notices
adjacent work that should become its own top-level thread. Examples:

- "This refactor suggests a separate audit of the logging subsystem."
- "The transcript contains enough context to start a follow-up investigation,
  but the current agent should keep working on the original task."
- "Open a new session for the user to pick up later with this handoff."

This should be a separate operation from subagent spawn.

Suggested workflow/API surface:

```text
RelatedSessionSpawn(
    title="Audit logging subsystem",
    task="Investigate the logging subsystem using the handoff context.",
    context={"source": "active_branch", "mode": "summary", "max_tokens": 4000},
    workspace={"mode": "same_project_fresh"},
)
```

Semantics:

- The new session is visible as a top-level session in `session.list`.
- The new session belongs to the same `project_id` as the origin session. It is
  not a new project and does not duplicate project configuration.
- The origin session records provenance, but does not get subagent control.
- The workflow receives a `SessionRef`, not an `AgentHandle`.
- A `SessionRef` can report the new session id/title/activity, but should not
  expose parent-style `send`, `tail`, `interrupt`, or `merge` methods. If the
  current agent needs to orchestrate the result, it should spawn a subagent
  instead.
- The spawned session gets a handoff package in its initial prompt/input. It
  does not share live transcript state with the origin after spawn.

The handoff package should be explicit and bounded:

```text
source session id/title
source active leaf id
parent-provided task
bounded transcript summary or selected excerpts
optional current diff / file references
```

Do not blindly inject the entire current transcript. Use one of:

- `summary`: daemon/model-generated summary plus recent turns,
- `excerpt`: bounded active-branch transcript excerpt,
- `selected_entries`: parent-selected transcript entry ids,
- `manual`: only the parent-provided handoff text.

Workspace modes:

- `same_project_fresh` default: behaves like a normal new top-level session in
  the origin session's project. The project's existing workspace definitions are
  reused, project bases are refreshed first, then the new session is
  instantiated from those bases using the existing Btrfs/reflink/copy fallback.
  Use this when the adjacent work is independent.
- `fork_current`: forks the origin session's current `cwd`, like a subagent,
  but the result is visible and not parent-controlled. Use this only when the
  adjacent work depends on dirty/untracked/generated state in the origin
  session.

Durable relationship metadata should use the `session_relationships` graph
described above. The important distinction is the relationship kind:

```text
subagent          hidden or nested, parent-controlled, used for current task
related           visible top-level, no parent control, same-project fresh workspace
related_fork      visible top-level, no parent control, fork-current workspace
```

Subagent-only lifecycle fields can remain nullable on that table or move to a
small companion table if the row becomes too broad.

### 8. Workflow bus and orchestration interfaces

The first workflow primitive should be a daemon-owned bus, not a Python process.
The bus stores variables, agent/session handles, structured results, and
workflow journals in Rust/Postgres. Python can be added later as one client of
that bus.

The minimum useful primitive is:

```text
parent/subagent/session output -> variable
variable/template -> parent/subagent/session input
```

This supports the original motivation for a REPL — passing context/messages
between agents — without committing to an interpreter on day one.

#### 8.1 Workflow variables

Add durable variables scoped to a workflow or parent session:

```text
workflow_variables
  workflow_id
  name
  value_json
  value_text
  producer_session_id nullable
  producer_action_id nullable
  created_at
  updated_at
```

Variables should support both text and JSON. JSON is necessary for structured
agent results; text is useful for transcript excerpts, diffs, and f-string-like
templates.

Initial model-facing tools can be plain daemon-owned tools:

```text
WorkflowVarsList
WorkflowVarRead
WorkflowVarWrite
WorkflowContextSend
```

Example:

```text
WorkflowVarsList()
WorkflowVarRead(name="reviewer_findings")
WorkflowContextSend(
  target_agent="implementer",
  template="Reviewer found:\n{reviewer_findings}\nPlease address these."
)
```

`WorkflowContextSend` renders a bounded template using workflow variables, then
appends the rendered message to a subagent or related session. This is simpler
than asking the parent model to manually paste content between tool calls.

#### 8.2 Agent result capture

Subagents should be able to return structured results directly into workflow
variables. There are two complementary paths:

1. The parent can spawn a child with a `result_variable` and optional
   `result_schema`.
2. The child can explicitly call a daemon-owned tool such as
   `WorkflowVarWrite`.

Example spawn:

```text
SubagentSpawn(
  role="reviewer",
  task="Review this diff. Return JSON matching the schema.",
  result_variable="reviewer_findings",
  result_schema={...}
)
```

Example child write:

```text
WorkflowVarWrite(
  name="reviewer_findings",
  value={ "summary": "...", "issues": [...] }
)
```

The explicit write tool is important because not every useful result appears at
the final turn boundary. A child may discover an intermediate fact that should
be routed to another child before it finishes.

#### 8.3 Dynamic workflow runner

Claude Code's dynamic workflow shape is useful: a generated script declares
metadata/phases, launches many agents, records `started` and `result` journal
entries, and returns a structured aggregate. A recent local Claude workflow in
`~/.claude/projects/-home-schwinns-pi-relay/.../workflows/scripts/` used this
pattern to audit many docs:

```js
export const meta = {
  name: 'doc-implementation-audit',
  phases: [{ title: 'Verify canonical docs' }, { title: 'Verify plans' }],
}

const findings = await parallel(items.map(item => () =>
  agent(promptFor(item), {
    label: `verify:${item.file}`,
    phase: 'Verify plans',
    schema: SCHEMA,
    agentType: 'Explore',
  })
))

return { count: findings.length, findings }
```

The pi-relay implementation does not need JavaScript first, but it should
support the same concepts:

```text
WorkflowRun
  meta: name, description, phases
  steps:
    spawn agent/session
    wait one/all
    write/read variable
    send templated context
    branch on variable/result status
    return aggregate
  journal:
    workflow.started
    agent.started
    variable.written
    agent.result
    workflow.result
```

This is what enables automatic back-to-back orchestration without the top-level
model intervening between each agent:

```text
fan out reviewers
  -> wait all
  -> collect structured findings
  -> spawn implementer with rendered aggregate
  -> wait implementer
  -> spawn verifier with implementer diff
  -> return final report
```

#### 8.4 First runner format

Start with a narrow declarative JSON/YAML workflow spec instead of a full Python
REPL:

```json
{
  "name": "review-implement-verify",
  "phases": [{ "title": "Review" }, { "title": "Implement" }],
  "steps": [
    {
      "id": "reviewers",
      "op": "spawn_all",
      "items": ["security", "correctness", "tests"],
      "role": "reviewer",
      "task_template": "Review for {item}.",
      "result_variable_template": "review_{item}"
    },
    {
      "id": "implement",
      "op": "spawn",
      "role": "implementer",
      "task_template": "Address these findings:\n{review_security}\n{review_correctness}\n{review_tests}",
      "result_variable": "implementation"
    }
  ]
}
```

This is less expressive than Python, but much easier to persist, replay, limit,
and test. It covers the first set of dynamic workflows: fanout, wait, result
collection, templated send, and sequential follow-up spawns.

#### 8.5 Optional Python adapter

Codex's Rust repo implements orchestration as JavaScript "code-mode", not
Python: one model-facing `exec` tool runs code, nested tool calls are forwarded
to a Rust delegate, long-running cells yield a cell id, and a `wait` tool
resumes/polls the cell.

If pi-relay later needs arbitrary computation beyond the declarative runner,
add Python as an adapter over the same bus:

```text
WorkflowExecPython(source)
  -> Python calls agents.spawn / vars.write / sessions.spawn_related
  -> Python host calls map to daemon workflow bus operations
  -> returns completed output or "running with cell_id"

WorkflowWait(cell_id)
  -> daemon waits for more output/completion/termination
```

Python variables are then convenience only. Durable workflow state remains in
Rust/Postgres, and Python `AgentHandle` objects contain ids that refresh from
the daemon.

These workflow tools need `AppState`, parent `session_id`, and active dispatch
context, so v1 should special-case them in `runtime/tool.rs` the way daemon-owned
tools are already special-cased. A later refactor can extend `ToolContext` with
a host delegate if several daemon-owned tools need the same pattern.

### 9. Merge-back

Filesystem CoW only makes child creation cheap; it does not merge work.

The merge model is:

```text
B = stable baseline snapshot at child spawn
P = parent current workspace
C = child current workspace
```

V1 merge behavior should be conservative:

1. `diff(child)` shows `B -> C`, including untracked/created/deleted files.
2. `merge_preview(child)` compares `B`, `P`, and `C` and reports:
   - cleanly applicable changes,
   - files changed by both parent and child,
   - binary files,
   - deletes/renames,
   - unsupported cases.
3. Parent decides whether to apply.

For the first shipped version, it is acceptable for `apply_merge` to support
only clean, non-conflicting text-file changes and otherwise return a patch plus
instructions for manual application by the parent. Auto-merge can come later.

Implementation options:

- Git workspaces:
  - Use the preserved baseline checkout to compute `B -> C`.
  - Prefer Git-aware diffs for tracked files.
  - Include untracked files explicitly.
  - Later, synthesize temporary Git trees for `B`, `P`, and `C` and use
    `merge-tree`/three-way merge mechanics.
- Local workspaces:
  - Use directory comparison between baseline and child.
  - Apply only clean additions/modifications/deletions where parent still
    matches baseline.

The key requirement is that merge uses the baseline snapshot, not the remote
base SHA. A child may start from a dirty parent state, so remote `base_sha` is
not enough.

## Implementation phases

### Phase 0: baseline in `main`

Already landed:

- daemon-owned workspace bases,
- Git fetch/reset/clean base refresh,
- local rsync base refresh,
- Btrfs subvolume snapshot instantiation,
- reflink/copy fallback,
- per-session Git branches for top-level sessions.

No new work needed here except reusing the primitives for parent-to-child forks.

### Phase 1: workspace fork primitives

- Add `WorkspaceManager::fork_session_from_parent`.
- Copy/snapshot parent `cwd` to child `cwd`.
- Copy/snapshot parent `cwd` to child baseline `cwd`.
- Validate child Git directories are isolated.
- Retarget child Git branches.
- Record fork mode and baseline path.
- Add unit tests for:
  - no project-base refresh during subagent fork,
  - dirty/untracked files appear in child and baseline,
  - parent and child Git dirs/common dirs are isolated,
  - child branch names differ from parent branch names,
  - copy fallback still works.

### Phase 2: durable session graph and internal spawn

- Add an `agent-store` table/migration for `session_relationships` rather than
  subagent-only edges.
- Add store methods to create/list/read/update relationships by source, target,
  kind, and control mode.
- Factor session start so subagent spawn can create a normal session from a
  supplied `SessionConfig` and first input.
- Add `SubagentManager::spawn`.
- Add `RelatedSessionManager::spawn` for same-project top-level handoff
  sessions.
- Drive the child through existing `SessionDriver`.
- Add harness/fake-provider integration tests so this works without real model
  calls.

### Phase 3: control operations

- Implement `send`, `tail`, `transcript`, `wait`, `interrupt`, and `stop`.
- Reuse existing input and interrupt paths where possible.
- Add parent/child status projection.
- Add limits: max depth, max children per parent, max concurrently running
  children.
- Add events for parent observability, for example:
  `relationship.created`, `subagent.updated`, `subagent.completed`,
  `workflow.variable_written`.

### Phase 4: workflow bus and variable/context tools

- Add durable workflow runs, workflow variables, and workflow journal events.
- Add daemon-owned tools:
  - `WorkflowVarsList`,
  - `WorkflowVarRead`,
  - `WorkflowVarWrite`,
  - `WorkflowContextSend`.
- Extend subagent spawn with `result_variable` and optional `result_schema`.
- Allow child agents to write intermediate structured results to variables.
- Add bounded template rendering for context forwarding:
  `{variable_name}` interpolation, max rendered size, and clear errors for
  missing variables.
- Ensure workflow operations have the parent session id and can call
  `SubagentManager` / `RelatedSessionManager`.

### Phase 5: dynamic workflow runner

- Add `WorkflowRun` / `WorkflowWait` for declarative JSON/YAML workflow specs.
- Support `spawn`, `spawn_all`, `wait`, `wait_all`, `var_write`, `context_send`,
  and `return` steps.
- Persist workflow `meta`, phases, steps, status, variables, and journal.
- Enforce limits: max spawned agents, max concurrent agents, max depth, max
  variable bytes, max rendered template bytes, max total workflow runtime.
- Add cancellation/termination.
- Keep output bounded and truncatable.

### Phase 6: optional Python adapter

- Only after the workflow bus and declarative runner are stable, consider
  `WorkflowExecPython` as an ergonomic adapter.
- The Python worker should call daemon bus operations; it must not be the durable
  source of truth.
- Implement cell lifecycle if/when Python lands:
  started, yielded, completed, failed, terminated.

### Phase 7: diff and merge preview

- Use child baseline snapshots to compute `B -> C`.
- Implement `agents.diff()` first.
- Implement `agents.merge_preview()` second.
- Implement clean apply only after preview is reliable.
- Do not attempt broad automatic conflict resolution in v1.

### Phase 8: prompt and docs

- Update `PI.md` tool guidance with workflow recipes:
  - when to spawn subagents,
  - when to spawn related same-project sessions,
  - how to choose roles,
  - how to write/read variables,
  - how to send templated context between agents,
  - how to use dynamic workflows,
  - how to wait/tail/diff,
  - how to avoid excessive fanout,
  - how to merge only after review.
- Keep tool descriptions self-contained enough that the model can use workflows
  without a huge prompt dump.
- Document examples in Rust docs; web UI docs can wait until a UI exists.

## Testing strategy

- Workspace fork unit tests in `agent-daemon` using temp Git repos and local
  workspaces.
- Btrfs-specific tests should skip when `btrfs` or a Btrfs-backed temp root is
  unavailable; copy/reflink fallback must always be tested.
- Store migration tests for session relationships, workflow runs, workflow
  variables, and workflow journal events.
- Subagent manager tests with harness model completions.
- Related session tests proving `same_project_fresh` reuses the origin
  `project_id` and creates a top-level visible session.
- Workflow bus tests with a fake delegate:
  - spawn returns handles,
  - wait/tail routes to delegate,
  - variable writes are durable,
  - templated context sends bounded rendered messages,
  - child result capture validates `result_schema`.
- Dynamic workflow runner tests:
  - `spawn_all` fans out and journals `agent.started`,
  - `wait_all` collects structured results,
  - follow-up steps can consume variables from earlier steps,
  - cancellation marks live children appropriately.
- Integration test for parent spawning a child from a dirty workspace and
  verifying:
  - child sees the dirty file,
  - parent does not see child edits until merge,
  - diff reports child changes against the spawn baseline.

## Risks and mitigations

- **Filesystem race during fork.** Add an exclusive fork barrier or reject spawn
  while sibling filesystem tools are active.
- **Context explosion.** Never auto-inject full child transcripts. Provide
  bounded `tail`, `summary`, and `transcript(max_tokens=...)`.
- **Variable/context leakage.** Variables can hold sensitive or massive content.
  Scope variables to a workflow/session, cap sizes, and require explicit
  template sends.
- **Workflow runaway.** Dynamic workflows can chain many agents automatically.
  Enforce max children, max concurrency, max runtime, and cancellation.
- **Python process loss.** If Python is added later, treat Python variables as
  convenience only; child sessions, workflow variables, and workflow journals
  live in Rust/Postgres.
- **Unbounded fanout.** Enforce depth and concurrency limits.
- **Git worktree leakage.** Validate copied `.git` state resolves inside the
  child workspace before the child starts.
- **Merge surprises.** Require baseline-based preview and explicit apply; v1
  supports only clean merges.
- **Tool special-case sprawl.** Start with daemon-owned special cases for
  workflow tools; revisit `ToolContext` host delegates if more daemon-owned
  tools accumulate.

## Open decisions

1. Should child sessions be hidden by default from `session.list`, or visible
   with `subagent: true` metadata?
2. Should role definitions remain `SKILL.md`, or should a future `.agents/roles`
   directory have separate semantics?
3. What should the v1 declarative workflow spec support beyond spawn/wait,
   variables, templated sends, and return aggregation?
4. Should Python workers be one persistent process per parent session, fresh
   cells with JSON `store/load` persistence like Codex code-mode, or deferred
   indefinitely?
5. How strict should the first filesystem barrier be: full tool classification
   or "spawn fails while any sibling action is active"?
6. What is the minimum acceptable merge apply surface for v1: diff-only,
   clean-text-file apply, or Git three-way for Git workspaces?
