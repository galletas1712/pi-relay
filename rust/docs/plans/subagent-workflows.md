# Subagent workflows

Status: implementation in progress. Last reviewed 2026-06-09.

## Bird's-eye view

pi-relay supports recursive workflow orchestration with normal durable sessions:

1. a root session owns the user goal;
2. the root may spawn child subagent sessions for true subtasks;
3. each child gets a CoW/copy snapshot of the parent's current session cwd;
4. the parent can read bounded child session state/transcript and send follow-ups;
5. workflow variables provide compact handoff/checkpoint storage.

There is no adjacent-session workflow primitive. Session forks are only for
subagents. Unrelated or future top-level work should be started by the user as a
normal session, not spawned by another agent.

## Goals

- Spawn child sessions with role instructions and task prompts.
- Fork a child's filesystem from the parent session cwd without refreshing bases
  or pulling remotes.
- Keep child transcript history append-only from the parent perspective.
- Let parents steer children using the same queued-input primitives users rely
  on.
- Provide a compact model-facing workflow vocabulary (`WorkSpawn`, `WorkAwait`,
  `WorkRead`, `WorkSend`, `WorkWrite`).
- Keep merge-back explicit: child filesystem edits are isolated until the parent
  accepts/materializes artifacts.
- Keep the root agent responsible for deciding workflow shape; users should not
  have to specify subagent topology.

## Non-goals

- Do not port the legacy TypeScript subagent implementation.
- Do not add a separate adjacent/top-level session spawning feature.
- Do not auto-merge child changes into the parent workspace.
- Do not let a parent rewrite existing child transcript entries.
- Do not require Python; Python scripts are only editable adapters over daemon
  primitives.

## Storage model

Subagent ownership uses the existing session parent pointer:

```text
sessions
  id                  primary key
  parent_session_id   references sessions(id)
```

This answers the only ownership question the daemon needs for v1: "who owns this
child session?" Rich status, control, visibility, filesystem mode, and role
metadata are not modeled as first-class relationship records. Child session
metadata can record role/task/workflow hints for UI and debugging, but
authorization and cleanup use only `sessions.parent_session_id`.

## Runtime model

### Spawning

`WorkSpawn` / `subagent.spawn`:

- validates the parent is a project session;
- resolves the requested role from built-ins (`worker`, `reviewer`, `tester`) or
  a project/user `SKILL.md`;
- forks the child workspace from the parent's current cwd;
- creates the child session with hidden subagent metadata and a durable
  `parent_session_id`;
- dispatches the child's initial turn.

If initial dispatch fails, the daemon cleans up the child session/workspace so
hidden orphans are not left behind.

### Steering and reading

- `WorkSend` appends follow-up/steer input to a direct child subagent.
- `WorkRead view=sessions` lists direct child subagents.
- `WorkRead view=overview|recent|turns|turn` reads bounded session state. Parents
  may read their subagents; visible same-project sessions are read-only.
- Direct `subagent.*` RPCs remain available for daemon/UI callers, while models
  see the compact `Work*` tools.

### Variables

Workflow variables are scoped to the root owner session of the current subagent
tree. Children and parents in the same tree can exchange compact JSON/text
handoffs by `workflow_id` and variable name. Producers are daemon-owned: model
calls cannot spoof producer session/action ids.

Variables are upserts. Scripts should use deterministic `workflow_id` and child
ids for reruns, plus iteration-specific variable names when history matters.

## Prompt model

The system prompt should stay concise and token-efficient:

- root chooses workflow shape;
- subagents are for true current-task subtasks only;
- default roles are `worker`, `reviewer`, `tester`;
- variables are the normal handoff path;
- transcript reads are for debugging/evidence, not every step;
- child filesystem edits are isolated and never merged automatically.

Avoid presenting many overlapping choices. In particular, do not expose adjacent
sessions or multiple spawn modes to the model.

## UI model

The inspector can navigate the root session plus direct child subagents. The same
transcript preview/steering UI can be reused for subagents:

- root is always selectable;
- child rows come from `WorkRead view=sessions`;
- selecting a child reads bounded transcript turns;
- steering uses `WorkSend`.

Later UI work can enrich child labels from session metadata, but it should not
require a separate relationship schema.

## Editable workflow scripts

Python workflow files are examples, not required runtime machinery. They should
wrap the same daemon RPCs:

- `spawn_subagent(role, task, result_var, child_session_id, ...)`;
- `await_vars` / `await_sessions`;
- `read_var` / `write_var`;
- optional transcript reads and follow-up sends.

Templates should cover common shapes without becoming mandatory:

- review loop;
- metric hillclimb;
- parallel race.

Agents can create new workflow scripts when the task calls for a custom loop.
