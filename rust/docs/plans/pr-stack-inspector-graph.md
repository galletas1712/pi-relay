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
   sourced from the `gh` sidecar.

## Topology: a read-only `gh` sidecar, GitHub kept out of the daemon

This functionality is implemented **mostly outside the daemon**. GitHub access
lives in a small, standalone, **read-only `gh` sidecar**; session/clone state
comes from the daemon; and the browser composes the two. Three components, each
dumb about the others:

```
browser ── websocket ──▶ daemon      (sessions + session.workspace_heads; no GitHub)
   │
   └── /gh/* (same-origin proxy) ──▶ gh-sidecar   (read-only gh; host auth; no sessions)
   │
   └── matches sessions↔PRs + builds stacks   (pure browser modules)
```

- **gh sidecar** (chosen): a tiny single-file bun/TS service (`packages/gh-sidecar`)
  that shells out to **read-only** `gh` (`gh pr list --author @me`, `gh pr view`,
  `gh api` GETs). It uses the host's existing `gh` auth (`gh auth login` /
  `GH_TOKEN`), so **no token ever reaches the browser**. Bound to localhost,
  reached **same-origin** via a `/gh/*` reverse-proxy route (Vite `server.proxy`
  in dev, the serve layer in prod). Never exposed publicly.
- **daemon**: gains **no GitHub knowledge** — only the one tiny
  `session.workspace_heads` RPC (clone git state the browser can't see).

> This supersedes the earlier "shell out to `gh` inside the daemon" idea: putting
> GitHub logic in the daemon contradicts the isolation goal.

Topologies considered:

- **C (chosen) — standalone read-only `gh` sidecar**, browser talks to it directly
  via a same-origin proxy. Maximal isolation, server-side auth, no browser token,
  zero daemon GitHub code. Cost: one tiny new deployable.
- A — browser → GitHub API directly. Rejected: puts a PAT in browser storage.
- B — `gh` inside the daemon. Rejected: GitHub logic in the daemon.
- **Daemon plugin/RPC-forwarding to the sidecar — rejected (YAGNI).** A generic
  plugin/forwarding layer is a framework for exactly one integration; it
  re-introduces GitHub-shaped traffic through the daemon and couples the daemon to
  the sidecar lifecycle, partially undoing the isolation. Its only benefit (single
  browser origin) is achieved more cheaply by the same-origin proxy route. Revisit
  a plugin layer only if several real integrations appear.

## Non-goals (explicitly cut to avoid cruft)

- No GitHub logic in the daemon (no `gh`, no token, no owner/repo parsing, no PR
  RPC). The daemon exposes only git state of clones it already owns.
- No daemon plugin/RPC-forwarding framework (see topology rejection above).
- No GitHub token in the browser — the sidecar holds host auth.
- No iframe embed of github.com — it sends `X-Frame-Options: DENY`, so a literal
  page embed is impossible. The "embedded PR view" is a detail **panel** rendered
  from sidecar-proxied GitHub data (title/body/state/checks/reviewers) plus an
  "open on GitHub" link.
- No GitLab/Forgejo support, no cross-project graph, no subagents in either
  surface, no persisted view toggle.
- No session delegation (parent/child) edges in the graph. The graph shows **PR
  base→head stacks** — the lineage that was asked for. Delegation overlay can be
  added later; it is out of scope here.
- No caching/TTL layer beyond react-query `staleTime`.
- The sidecar is **read-only**: it only ever runs `gh` GET-equivalent commands;
  no PR creation/merge/edit.

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

The isolated module fetches your open PRs for a repo (via the sidecar:
`/gh/pulls?...`) with their `head.ref` and `head.sha`, then links a session
workspace to a PR by:

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

## Components & layout

### gh sidecar (`packages/gh-sidecar`)

A single-file bun/TS HTTP service, read-only, localhost-bound:

```
gh-sidecar/
  src/server.ts   // tiny HTTP server; routes -> read-only `gh` invocations
  package.json
```

- Routes are a thin, allow-listed mapping to `gh` (e.g. `GET /pulls` →
  `gh pr list --author @me --state open --json number,title,url,headRefName,baseRefName,headRefOid,mergeStateStatus`;
  `GET /pulls/:n` → `gh pr view :n --json ...`). Repo is passed as a query param
  and forwarded via `gh --repo owner/name` (derived in the browser from the
  workspace `remote_url`).
- No write commands are reachable. No session knowledge. Auth is the host's `gh`.
- Reached **same-origin** from the web app through a `/gh/*` proxy (Vite
  `server.proxy` in dev; the serve layer in prod), so the browser never holds a
  token and there is no CORS surface.

### Frontend feature module (`packages/web/src/pr-graph/`)

Self-contained:

```
pr-graph/
  github.ts          // calls the sidecar via /gh/* (plain fetch); no token handling
  stack.ts           // build PR stacks + lineage paths from a PR list (pure, unit-tested)
  match.ts           // link sessions↔PRs from workspace_heads + PR list (pure, unit-tested)
  usePrGraph.ts      // react-query hooks: workspace_heads (daemon) + PRs (/gh/*)
  GraphPane.tsx      // interactive graph (React Flow) + click-to-highlight lineage
  PrDetailPanel.tsx  // PR detail panel (sidecar-sourced) + open-on-GitHub
  SessionsForPr.tsx  // sessions whose workspace is checked out at the PR's branch
  inspectorSection.tsx // the inspector "Workspaces" section (consumed by panels.tsx)
```

`stack.ts` and `match.ts` are pure functions over plain data, so the graph logic
is testable without the sidecar, daemon, or network.

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
    sidecar + "open on GitHub". (Separate panel, not an iframe — see non-goals.)

### Integration seams (the only edits to existing files)

- `App.tsx`: a `topView: "chat" | "graph"` state + a sidebar/topbar toggle that
  swaps the center grid region for `<GraphPane/>`. Graph scope = idle + unarchived
  top-level sessions of the selected project.
- `panels.tsx`: render `<InspectorWorkspacesSection/>` from `pr-graph/`, gated on
  `snapshot.metadata.subagent !== true`.
- `queryKeys.ts` / `agentApi.ts`: the one `session.workspace_heads` method + key.
- Dev/serve proxy config: the `/gh/*` → sidecar route.

Everything else stays inside `pr-graph/` and `packages/gh-sidecar/`.

## Build order

1. `session.workspace_heads` RPC (the only daemon change; GitHub-free).
2. `gh` sidecar (`packages/gh-sidecar`) + the `/gh/*` proxy route.
3. `pr-graph/` core: `github.ts`, `stack.ts`, `match.ts` + unit tests for the two
   pure modules.
4. Inspector Workspaces section.
5. Interactive `GraphPane` + `PrDetailPanel` + `SessionsForPr`.

## Open risks

- **Match precision.** Branch-name/exact-sha matching can miss when a session
  commits past its pushed tip (deferred ancestry refinement).
- **`gh` availability.** If `gh` is absent/unauthenticated on the host, the
  sidecar returns empty and both surfaces degrade gracefully (no PR data), rather
  than erroring.
- **Sidecar exposure.** Must stay localhost-bound and read-only; the `/gh/*`
  proxy is the only path the browser uses.
- **GitHub rate limits.** Authenticated `gh` limits (5k/hr) are ample for a
  personal tool; react-query `staleTime` avoids redundant calls.
