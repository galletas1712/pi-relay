# PR stack: inspector status + graph view

Status: planned. Last reviewed 2026-06-21.

## Motivation

Sessions check out git workspaces on per-session local branches, and the
intended workflow (see `PI.md`) is for the agent to push a descriptive branch
and open a PR. When you run many sessions against large open-source repos, it is
hard to keep track of which session owns which branch/PR, how those PRs stack on
each other, and whether any of them have fallen behind their base.

Two additions close that gap:

1. **Inspector PR status** — for a top-level session, show each git workspace's
   branch, the PR it corresponds to (one you own), and whether that PR is out of
   date (behind its base). The unmerged PR *stack* the PR sits in is shown as
   context, since a PR is rarely meaningful in isolation.
2. **Graph view** — a new top-level view (alongside chat) that visualizes the
   unmerged PR stacks across idle, unarchived, top-level sessions, so the
   lineage of your in-flight PRs is visible at a glance.

This doc tracks only this work. The session/workspace data model and RPC surface
it builds on are documented in [architecture](../architecture.md) and
[websocket-rpc](../websocket-rpc.md); do not re-litigate those here.

## Non-goals (explicitly cut to avoid cruft)

- No bespoke GitHub HTTP client, no `remote_url`→owner/repo parser, no
  `GITHUB_TOKEN` wiring. We shell out to the **`gh` CLI** inside the workspace
  clone, which auto-detects the repo from `origin` and carries its own auth.
- No separate git-only freshness RPC, no `sessions.lineage` RPC. One RPC serves
  both the inspector and the graph.
- No caching/TTL/fetch-serialization layer. React-query `staleTime` on the
  frontend is sufficient; `gh`'s authenticated rate limit is not a constraint
  for a personal tool.
- No GitLab/Forgejo support, no cross-project graph, no subagents in either
  surface, no persisted view toggle, no interactive mermaid nodes.
- No session delegation (parent/child) edges in the graph. The graph shows **PR
  base→head stacks**, which is the lineage that was asked for. (Delegation
  overlay can be added later if it proves useful; it is not part of this work.)

## Key decisions

### Branch→PR matching is by commit SHA, not branch name

The session's pushed branch name is whatever descriptive name the agent chose,
and is not recorded anywhere. Rather than introduce machinery to record it, we
match by commit identity: `gh pr list --author @me --state open --json
number,title,url,headRefName,baseRefName,headRefOid` gives our open PRs with
their head SHAs; the session's clone already contains the commits it pushed, so

```
git cat-file -e <pr_head_oid>^{commit}     # is this PR's head present in this clone?
```

(or `git merge-base --is-ancestor <pr_head_oid> HEAD`) identifies which PR this
workspace produced, independent of branch naming, forks, or rebases.

`gh pr list` and the `git` queries run with the workspace clone as cwd, reusing
the daemon's existing `git_output`/`run_git` helpers in
`rust/crates/agent-daemon/src/workspaces/` (a sibling `gh_output` helper that
shells out the same way). No new HTTP stack, no token plumbing.

### "Out of date" comes straight from `gh`

`gh` reports merge state directly, so no local `rev-list`/`compare` math is
needed:

```
gh pr view <number> --json mergeStateStatus,mergeable
```

`out_of_date = mergeStateStatus == "BEHIND"` (PR head is behind its base). We do
not compute dirty/unpushed signals — out of date means "the PR needs to catch up
to its base."

### The unmerged stack

From the same `gh pr list` result (all our open PRs in the repo), a PR `B` is
stacked on PR `A` when `B.baseRefName == A.headRefName`. Walking these edges from
the session's matched PR yields the unmerged stack it belongs to. The stack is
attached to the workspace status (inspector context) and is the edge set the
graph renders.

### Top-level sessions only

Subagents carry `metadata.subagent == true` (and `metadata.hidden == true`, so
they are already excluded from `session.list` and the graph). The inspector PR
section is additionally gated on `metadata.subagent !== true`. Rationale: full
subagents inherit the parent's workspace fork (their branch/PR state is the
parent's), and read-only subagents never persist changes to the parent's
filesystem — so per-subagent PR status is noise.

## Backend: one RPC

`session.pull_requests` — params `{ session_id }`. Refuses (or returns empty)
for sessions with `metadata.subagent == true`.

For each **git** workspace of the session, run, with the workspace clone as cwd:

1. `gh pr list --author @me --state open --json number,title,url,headRefName,baseRefName,headRefOid`
   (repo auto-detected from `origin`).
2. Match the workspace to a PR by head-SHA presence in the clone (above).
3. For the matched PR, `gh pr view --json mergeStateStatus` → `out_of_date`.
4. Build the unmerged stack via `baseRefName`/`headRefName` edges among the
   listed PRs.

Returns, per workspace:

```jsonc
{
  "workspace_dir": "repo",
  "local_branch": "pi/session/.../repo",
  "base_branch": "main",                 // remote_branch
  "pr": {                                 // null if no match
    "number": 123, "title": "...", "url": "...",
    "state": "open", "out_of_date": false
  },
  "stack": [                              // unmerged PRs base→head, [] if none
    { "number": 120, "title": "...", "url": "...", "base_ref": "main",    "head_ref": "feat-a" },
    { "number": 123, "title": "...", "url": "...", "base_ref": "feat-a",  "head_ref": "feat-b" }
  ]
}
```

Wiring mirrors `subagent.list`: add the `RpcMethod` variant in
`rust/crates/agent-daemon/src/types.rs` (parse table), dispatch in `main.rs`,
serialize the view in `rpc_views.rs`. `gh` absence or non-git workspaces degrade
to `pr: null, stack: []` (no hard error).

## Frontend

One react-query hook over `session.pull_requests` (key in `queryKeys.ts`, method
in `agentApi.ts`), consumed by both surfaces.

### Inspector "Workspaces" section (`panels.tsx`)

Rendered only when `snapshot.metadata.subagent !== true`. Reuses the existing
`.kv` row pattern under a new `inspect-section`:

- `workspace_dir` → `local_branch`
- `base` → `base_branch`
- `PR` → `#123 title` (link to `url`) or "none"
- `status` → "up to date" / "behind base" chip
- the unmerged stack listed compactly beneath (each PR linked), highlighting the
  current workspace's PR

PR/status fill in async with a "checking…" state; branch/base render
immediately from the snapshot.

### Graph view (`App.tsx` + new `GraphPane`)

- `topView: "chat" | "graph"` state in `App.tsx`; a toggle in the sidebar header
  (and mobile topbar) swaps the center grid region. Not persisted.
- Scope: idle + unarchived + top-level sessions of the selected project
  (`activity === "idle" && !isArchivedSession(s)`; subagents already absent from
  `session.list`).
- Build a mermaid `flowchart` string: nodes = unmerged PRs (labeled with PR
  number/title and the owning session), edges = base→head stack relationships.
  Render via the already-bundled `MermaidBlock` (`mermaidBlock.tsx`) — no new
  dependency. Read-only; a side legend lists sessions for navigation.

## Build order

1. `session.pull_requests` RPC (`gh` + git SHA match + stack assembly).
2. Inspector Workspaces section.
3. Graph view.

Each step is independently shippable; step 1 is the only backend change.

## Open risk

`gh` must be available and authenticated in the daemon's environment. If it is
absent, both surfaces simply show no PR data (graceful degradation), which is the
intended behavior rather than an error.
