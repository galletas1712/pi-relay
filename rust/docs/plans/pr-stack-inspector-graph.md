# PR stack: inspector status + interactive graph view

Status: planned. Last reviewed 2026-06-21.

## Motivation

Sessions check out git workspaces on per-session local branches, and the
intended workflow (see `PI.md`) is for the agent to push a descriptive branch and
open a PR. When you run many sessions against large open-source repos, it is hard
to keep track of which session owns which branch/PR, how those PRs stack on each
other, and whether any have fallen behind their base.

Two additions close that gap:

1. **Inspector PR status** — for a top-level session, show each git workspace's
   branch, the PR it corresponds to (one you own), whether that PR is out of date
   (behind its base), and the unmerged stack it sits in.
2. **Interactive PR graph view** — a new top-level view (alongside chat) where
   each node is one of your unmerged PRs and edges are base→head stacking. Click
   a node to highlight its full lineage path down to `main`, see every session
   whose workspace is checked out at that PR's branch, and open a PR detail panel
   sourced from the GitHub API.

## Topology: GitHub access lives in the frontend, not the daemon

This functionality is implemented **mostly outside the daemon**, in an isolated
frontend feature module. The daemon's only role is the part that *must* be
server-side: reporting git state of the session clones it owns (which the browser
cannot see). It gains **no GitHub knowledge** — no `gh`, no token, no PR types.

> Note: this supersedes the earlier "shell out to `gh` in the daemon" idea. `gh`
> cannot run in the browser, and putting GitHub logic in the daemon contradicts
> the isolation goal. We use the GitHub **REST/GraphQL API from the browser**
> instead.

Three topologies were considered; **A is chosen** (pending confirmation of the
browser-token tradeoff):

- **A (chosen) — Browser → GitHub API directly.** A user-supplied PAT (stored in
  the browser) is used by the isolated module to call GitHub. Daemon stays
  GitHub-free; the whole feature is one frontend folder + one tiny daemon RPC.
  Tradeoff: a fine-grained read-only PAT lives in browser storage.
- B — `gh` inside the daemon (one PR RPC). Simplest auth, but puts GitHub logic
  in the daemon, which we are explicitly avoiding.
- C — a small sidecar process runs `gh` server-side. Honors isolation + keeps
  auth server-side, but adds a new deployable. Defer unless the browser-token
  tradeoff in A is unacceptable.

## Non-goals (explicitly cut to avoid cruft)

- No GitHub logic in the daemon (no `gh`, no token, no owner/repo parsing, no PR
  RPC). The daemon exposes only git state of clones it already owns.
- No iframe embed of github.com — it sends `X-Frame-Options: DENY`, so a literal
  page embed is impossible. The "embedded PR view" is a detail **panel** rendered
  from the GitHub API (title/body/state/checks/reviewers) plus an "open on
  GitHub" link.
- No GitLab/Forgejo support, no cross-project graph, no subagents in either
  surface, no persisted view toggle.
- No session delegation (parent/child) edges in the graph. The graph shows **PR
  base→head stacks** — the lineage that was asked for. Delegation overlay can be
  added later; it is out of scope here.
- No caching/TTL layer beyond react-query `staleTime`.

## Key decisions

### The daemon's one job: git state of clones

New tiny RPC `session.workspace_heads` — params `{ session_id }`, refused/empty
for `metadata.subagent == true`. For each **git** workspace, using the clone as
cwd via the existing `git_output` helper (`rust/crates/agent-daemon/src/
workspaces/`):

```jsonc
{
  "workspace_dir": "repo",
  "local_branch": "pi/session/.../repo",
  "base_branch": "main",                 // remote_branch
  "head_sha": "<git rev-parse HEAD>",
  "pushed_branch": "feat-b"              // best-effort: rev-parse --abbrev-ref @{u}, else null
}
```

That is the entire backend change. No GitHub anything. Wiring mirrors
`subagent.list`: `RpcMethod` variant in `types.rs`, dispatch in `main.rs`, view
in `rpc_views.rs`.

### Branch→PR matching happens in the browser

The isolated module fetches your open PRs for a repo (`search` / `pulls`) with
their `head.ref` and `head.sha`, then links a session workspace to a PR by:

1. `pushed_branch == pr.head.ref` (preferred, when the daemon could resolve an
   upstream), else
2. `head_sha == pr.head.sha` (exact tip match).

Ancestry matching (session committed past the pushed tip) is a known limitation
deferred to a later refinement; it would need a daemon round-trip with candidate
SHAs and is intentionally not built now.

### The unmerged stack and "out of date"

From your open PRs in the repo, PR `B` is stacked on PR `A` when
`B.base.ref == A.head.ref`. Walking those edges yields the unmerged stack, all in
the browser. "Out of date" = the GitHub `mergeable_state == "behind"` (head
behind base). The bottom of every stack points at the repo default branch
(`main`).

### Top-level sessions only

Subagents carry `metadata.subagent == true` (and `metadata.hidden == true`, so
they are already excluded from `session.list`/the graph). The inspector PR
section is additionally gated on `metadata.subagent !== true`. Full subagents
inherit the parent's workspace fork; read-only subagents never persist changes to
the parent — so per-subagent PR status is noise.

## Frontend: one isolated feature module

Everything lives in `packages/web/src/pr-graph/` and is self-contained:

```
pr-graph/
  github.ts          // browser GitHub API client (plain fetch; token from settings)
  token.ts           // PAT storage (localStorage) + a small settings input
  stack.ts           // build PR stacks + lineage paths from a PR list (pure, unit-tested)
  match.ts           // link sessions↔PRs from workspace_heads + PR list (pure, unit-tested)
  usePrGraph.ts      // react-query hooks: workspace_heads (daemon) + PRs (GitHub)
  GraphPane.tsx      // interactive graph (React Flow) + click-to-highlight lineage
  PrDetailPanel.tsx  // GitHub-API-sourced PR detail ("embedded" view) + open-on-GitHub
  SessionsForPr.tsx  // sessions whose workspace is checked out at the PR's branch
  inspectorSection.tsx // the inspector "Workspaces" section (consumed by panels.tsx)
```

`stack.ts` and `match.ts` are pure functions over plain data, so the graph logic
is testable without a daemon or network.

### Interactive graph (`GraphPane.tsx`)

- Library: **React Flow** (`@xyflow/react`) for an actual interactive DAG, with a
  small auto-layout pass (dagre/elk) bottom (`main`) → top (leaf PRs). Mermaid is
  **not** used here (it can't do click-to-highlight); it stays only for
  transcript diagrams.
- Each node = one unmerged PR (number, title, owning session(s), out-of-date
  badge). A synthetic `main` node anchors each stack's base.
- **Click a node** → highlight the lineage path from that PR down to `main`
  (ancestors via base→head edges), dim the rest, and open the selection panel:
  - **Stacked-on**: the PR(s) it sits on top of, down to `main`.
  - **Sessions**: every session whose workspace is checked out at that PR's
    branch (from `workspace_heads` ↔ PR matching), each linking back to its chat.
  - **PR detail** (`PrDetailPanel`): title/body/state/checks/reviewers from the
    GitHub API + "open on GitHub". (Separate panel, not an iframe — see
    non-goals.)

### Integration seams (the only edits to existing files)

- `App.tsx`: a `topView: "chat" | "graph"` state + a sidebar/topbar toggle that
  swaps the center grid region for `<GraphPane/>`. Graph scope = idle + unarchived
  top-level sessions of the selected project.
- `panels.tsx`: render `<InspectorWorkspacesSection/>` from `pr-graph/`, gated on
  `snapshot.metadata.subagent !== true`.
- `queryKeys.ts` / `agentApi.ts`: the one `session.workspace_heads` method + key.

Everything else stays inside `pr-graph/`.

## Build order

1. `session.workspace_heads` RPC (the only backend change; GitHub-free).
2. `pr-graph/` core: `github.ts`, `token.ts`, `stack.ts`, `match.ts` + unit
   tests for the two pure modules.
3. Inspector Workspaces section.
4. Interactive `GraphPane` + `PrDetailPanel` + `SessionsForPr`.

## Open risks

- **Browser-held PAT** (topology A). Mitigation: fine-grained read-only token,
  clearly scoped, with an option to fall back to topology C (server-side `gh`
  sidecar) if unacceptable.
- **GitHub rate limits / CORS.** The REST/GraphQL APIs are CORS-enabled for
  token auth; authenticated limits (5k/hr) are ample for a personal tool.
- **Match precision.** Branch-name/exact-sha matching can miss when a session
  commits past its pushed tip (deferred ancestry refinement).
