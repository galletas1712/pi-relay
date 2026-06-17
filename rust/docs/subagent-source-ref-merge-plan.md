# Subagent source-ref merge plan

> **Superseded (2026-06-16).** The cross-task git source-ref / merge mechanism
> described here is removed by `plans/workflow-orchestration.md` ("Combining
> parallel work"). Code now crosses task boundaries only as a `changes` (diff)
> artifact, combined via the `select` / `integrate` / `reduce` patterns — there
> is no daemon-side merge, no `sources` workspace mode, and no `merger` role.
> This document is retained as design history for the *current* REPL
> `subagents.*` behavior only; do not extend it.

## Goal

Let parent sessions orchestrate implementation, testing, verification, and merge
workflows without serializing large file diffs into model context and without
exposing child workspace paths as the primary abstraction.

Subagents remain regular forked sessions. A completed child is an opaque source
handle with text/transcript metadata. When another child is spawned with that
source handle, the daemon makes the source child's git workspace state available
inside the new child's fork as local git refs.

## Non-goals

- Do not eagerly inject patch text into prompts.
- Do not maintain daemon-specific hard-exclude lists such as `node_modules`.
- Do not attempt to retain or merge changes from local-folder workspaces.
- Do not auto-apply child changes back into the parent workspace.
- Do not expose arbitrary session workspace paths through the REPL.

## Workflow

```python
implementers = [
    subagents.spawn(role="implementer", message="Try approach A"),
    subagents.spawn(role="implementer", message="Try approach B"),
]
implementations = subagents.wait(implementers)

merge = subagents.call(
    role="merger",
    message="Combine the best implementation pieces.",
    sources=implementations,
)

verify = subagents.call(
    role="verifier",
    message="Verify the merged result.",
    sources=[merge],
)
```

Each `sources` item is a known child session of the current parent. The spawned
child gets a compact source section in its initial message:

```text
# Source child sessions

The following child session outputs are available as local git refs in your
workspace. Inspect or merge them with git commands as needed; do not assume they
are already applied.

## source-1-implementer-a1b2c3d4

- Session: `session_...`
- Git refs:
  - workspace `repo`: `refs/pi-relay/sources/source-1-implementer-a1b2c3d4`
```

The model decides what to inspect and merge:

```bash
git diff HEAD..refs/pi-relay/sources/source-1-implementer-a1b2c3d4
git checkout refs/pi-relay/sources/source-1-implementer-a1b2c3d4 -- src/file.py
git merge --no-commit refs/pi-relay/sources/source-1-implementer-a1b2c3d4
```

## Git workspace semantics

For each git workspace common to the spawned child and each source child:

1. Ensure the source child is idle.
2. Commit the source child's current worktree to a local synthetic commit, using
   normal git semantics:
   - tracked changes are included
   - tracked deletions are included
   - untracked non-ignored files are included via `git add -A`
   - ignored files stay ignored
3. Fetch that synthetic commit into the spawned child's corresponding workspace
   as:

   ```text
   refs/pi-relay/sources/source-N-role-session
   ```

Local-folder workspaces are skipped. If durable changes matter, the workspace
should be a git workspace.

## Why refs instead of diffs?

Refs are the minimum useful information for a merger:

- The prompt stays small.
- Git remains the source of truth for ignore handling, renames, conflicts, and
  merge operations.
- The merger can inspect only what it needs.
- Multiple child proposals from repeated `spawn(...)` calls plus `wait(...)`
  become multiple local refs.
- The parent remains the authority: merged changes are still just another child
  proposal until explicitly accepted later.

## Future work

- Add a parent-side accept/apply primitive once the proposal model is stable.
- Add optional lazy diff/UI inspection endpoints for human review.
- Add richer source labels if user-facing names become useful.
