# PR stack: inspector status + interactive graph view

Status: planned (complete spec, ready to implement). Last reviewed 2026-06-21.

## 1. Motivation

Sessions check out git workspaces on per-session local branches, and the
intended workflow (see `PI.md`) is for the agent to push a descriptive branch and
open a PR. When you run many sessions against large open-source repos, it is hard
to track which session owns which branch/PR, how those PRs stack on one another,
and whether any have fallen behind their base.

Two additions close that gap:

1. **Inspector PR status** — for a top-level session, each git workspace shows its
   branch, the PR it corresponds to (one you own), whether that PR is behind its
   base, and the unmerged stack it sits in.
2. **Interactive PR graph view** — a new top-level view (alongside chat) where
   each node is one of your unmerged PRs and edges are base→head stacking.
   Clicking a node highlights its full lineage path down to its base branch, lists
   every session whose workspace is checked out at that PR's branch, and opens a
   PR detail panel sourced from the `gh` sidecar.

## 2. Goals (precise user-facing behavior)

- **G1.** In the inspector, for a **top-level** session only, show a "Workspaces"
  section: per git workspace → local branch, base branch, matched PR (number +
  title + link), an out-of-date indicator, and the unmerged stack the PR is in.
- **G2.** A toggle switches the center pane between **Chat** and **Graph**.
- **G3.** The graph shows, for the selected project's **idle + unarchived +
  top-level** sessions, the unmerged PRs you own (matched to those sessions),
  drawn as base→head stacks per repo, with a synthetic base-branch node anchoring
  each stack.
- **G4.** Clicking a PR node:
  - highlights the lineage path from that PR down to its base branch (ancestors),
    dimming unrelated nodes;
  - lists the sessions whose workspace is checked out at that PR's branch, each
    linking back to its chat;
  - opens a PR detail panel (title/body/state/checks/reviewers) with an "open on
    GitHub" link.
- **G5.** All of GitHub is read-only; nothing in this feature can create/merge/edit
  PRs.

## 3. Topology: a read-only `gh` sidecar, GitHub kept out of the daemon

GitHub access lives in a small, standalone, **read-only `gh` sidecar**; session
and clone state come from the daemon; the browser composes the two. Three
components, each ignorant of the others:

```
browser ── websocket ──▶ daemon       (sessions + session.workspace_heads; no GitHub)
   │
   └── /gh/* (same-origin) ──▶ gh-sidecar   (read-only gh; host auth; no sessions)
   │
   └── matches sessions↔PRs + builds stacks  (pure browser modules)
```

- **gh sidecar (chosen):** a tiny single-file bun/TS service (`packages/gh-sidecar`)
  that shells out to **read-only** `gh`. It uses the host's existing `gh` auth, so
  **no token ever reaches the browser**. Bound to localhost, reached **same-origin**
  via a `/gh/*` route. Never exposed directly.
- **daemon:** gains **no GitHub knowledge** — only the one new
  `session.workspace_heads` RPC (clone git state the browser can't see).

> Supersedes the earlier "shell out to `gh` inside the daemon" idea: putting
> GitHub logic in the daemon contradicts the isolation goal.

### Topologies considered

- **C (chosen) — standalone read-only `gh` sidecar**, browser talks to it via a
  same-origin route. Maximal isolation, server-side auth, no browser token, zero
  daemon GitHub code. Cost: one tiny new deployable.
- A — browser → GitHub API directly. Rejected: PAT in browser storage.
- B — `gh` inside the daemon. Rejected: GitHub logic in the daemon.
- **Daemon plugin/RPC-forwarding to the sidecar — rejected (YAGNI).** A generic
  plugin/forwarding layer is a framework for exactly one integration; it
  re-introduces GitHub-shaped traffic through the daemon and couples the daemon to
  the sidecar lifecycle, partially undoing the isolation. Its only benefit (single
  browser origin) is achieved more cheaply by the same-origin route. Revisit only
  if several real integrations appear.

## 4. Non-goals (explicitly cut to avoid cruft)

- No GitHub logic in the daemon (no `gh`, no token, no owner/repo parsing, no PR
  RPC). The daemon exposes only git state of clones it already owns.
- No daemon plugin/RPC-forwarding framework (see §3).
- No GitHub token in the browser — the sidecar holds host auth.
- No iframe embed of github.com — it sends `X-Frame-Options: DENY`. The "embedded
  PR view" is a detail **panel** from sidecar-proxied data + an "open on GitHub"
  link.
- No GitLab/Forgejo support, no cross-project graph, no subagents in either
  surface, no persisted view toggle.
- No session delegation (parent/child) edges in the graph. The graph shows **PR
  base→head stacks** only. Delegation overlay is out of scope.
- No server-side caching/TTL layer beyond react-query `staleTime`.
- The sidecar is **read-only** (GET-equivalent `gh` commands only).

## 5. Architecture & data contracts

### 5.1 Daemon RPC: `session.workspace_heads` (the only backend change)

**Why it must be in the daemon:** matching a session to a PR needs the clone's
current HEAD sha and (best-effort) the pushed/upstream branch. `local_branch`
(`pi/session/...`) is never pushed and `base_sha` is checkout-time, so neither
identifies the PR head; only the daemon can read the live clone.

**Params (batch, to avoid N+1 from the graph):**

```jsonc
{ "session_ids": ["session_…", "session_…"] }
```

**Response:**

```jsonc
{
  "sessions": {
    "session_abc": [
      {
        "workspace_dir": "repo",
        "kind": "git",
        "remote_url": "https://github.com/owner/repo.git",
        "local_branch": "pi/session/session_abc/repo",
        "base_branch": "main",          // = SessionWorkspace.remote_branch
        "head_sha": "<git rev-parse HEAD>",
        "pushed_branch": "feat-b"       // best-effort upstream short name, else null
      }
    ]
  }
}
```

- Sessions with `metadata.subagent == true` are **omitted** from the response
  (server-side guard; the frontend also gates display).
- Local (non-git) workspaces are omitted from a session's array.
- Per git workspace, with `cwd = <snapshot.outer_cwd>/<workspace_dir>`:
  - `head_sha` = `git rev-parse HEAD`.
  - `pushed_branch` = `git rev-parse --abbrev-ref --symbolic-full-name @{u}`
    stripped of the `origin/` prefix; `null` if there is no upstream (the common
    case unless the agent pushed with `-u`).
- A workspace whose git read fails (missing clone, detached, etc.) is returned
  with `head_sha: null` rather than failing the whole call.

**Implementation placement (keeps git in one place, mirrors `subagent.list`):**
- Add `pub(crate) async fn workspace_heads(outer_cwd, &[SessionWorkspace])` to
  `rust/crates/agent-daemon/src/workspaces/` (new `heads.rs` or in `mod.rs`),
  reusing the module-private `git_output` / `git_output_dynamic` helpers.
- Handler `session_workspace_heads` in `main.rs` loads each snapshot via
  `state.repo.session_snapshot(id)` (no `SessionDriver::acquire` needed — these
  are read-only `rev-parse`s), reads `outer_cwd` + `workspaces` + `metadata`, and
  calls the workspaces helper.
- Wire `RpcMethod::SessionWorkspaceHeads` in `types.rs` (enum + `parse()` + a
  `parse` unit test) and dispatch in `main.rs`; serialize via a small
  `rpc_views::workspace_heads(...)` for symmetry.

### 5.2 gh sidecar (`packages/gh-sidecar`)

A single-file bun/TS HTTP service, read-only, localhost-bound:

```
gh-sidecar/
  src/server.ts    // tiny HTTP server; allow-listed routes -> read-only gh
  package.json     // { "bin"/"scripts": { "start": "bun run src/server.ts" } }
  README.md
```

**Routes (allow-listed; repo passed as `?repo=owner/name`, forwarded as
`gh --repo owner/name`):**

| Route | `gh` invocation | Returns |
|---|---|---|
| `GET /gh/health` | `gh auth status` (exit code only) | `{ ok, authed }` |
| `GET /gh/pulls?repo=o/r` | `gh pr list --repo o/r --author @me --state open --json number,title,url,state,isDraft,headRefName,headRefOid,baseRefName,headRepositoryOwner` | `{ pulls: Pull[], degraded: false }` |
| `GET /gh/pull?repo=o/r&number=N` | `gh pr view N --repo o/r --json number,title,url,state,isDraft,body,mergeStateStatus,reviewDecision,statusCheckRollup,headRefName,baseRefName` | `{ pull: PullDetail }` |

- **Degraded mode:** if `gh` is missing/unauthenticated, every route returns
  HTTP 200 with `{ degraded: true, ... empty ... }` so the UI shows "GitHub not
  connected" rather than erroring.
- **Out-of-date signal** lives on the *detail* call: `mergeStateStatus == "BEHIND"`
  ⇒ behind base. `list` does not reliably populate `mergeStateStatus`, so the
  inspector/graph fetch detail for *matched* PRs only (few). `UNKNOWN` ⇒ neutral
  "checking" chip (GitHub may still be computing).
- **No write subcommands are reachable.** No session knowledge. `repo` is
  validated against `^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$` before being passed to
  `gh`. The PR `number` is validated as an integer. `gh` is invoked with an
  argv array (never a shell string).
- Binds `127.0.0.1:${GH_SIDECAR_PORT:-8789}`.

### 5.3 Same-origin `/gh` route + process wiring

The web app is served by `vite preview` (static build) and, on the tailnet, by
`tailscale serve` path routing — there is no dev server in the running stack.
Mirror how `/ws` is exposed today:

- **Local mode** (`http://127.0.0.1:8788`): add `preview: { proxy: { "/gh": {
  target: "http://127.0.0.1:8789", changeOrigin: true } } }` to
  `packages/web/vite.config.ts`. *(Build step note: confirm `vite preview`
  honors `preview.proxy`; if not, fall back to a tailscale-serve-style route or a
  `VITE_PI_GH_BASE` env pointing straight at the sidecar with CORS enabled on the
  sidecar.)*
- **Tailnet mode**: add `tailscale serve --bg --set-path=/gh
  "http://127.0.0.1:${GH_SIDECAR_PORT}"` to `infra/serve.sh` (alongside `/ws` and
  `/`). The browser uses same-origin `/gh/*` in both modes; the sidecar stays
  localhost-only.
- **`infra/dev.sh`**: start the sidecar (`( cd packages/gh-sidecar && bun run
  start ) &`) with its own PID and add it to the `shutdown`/`wait` set, next to
  the daemon and web preview.

The browser base for the sidecar defaults to `/gh` (overridable via
`VITE_PI_GH_BASE`).

### 5.4 Frontend feature module (`packages/web/src/pr-graph/`)

Self-contained; only the pure modules and hooks are imported by the integration
seams.

```
pr-graph/
  ghClient.ts        // fetch wrapper over /gh/* (base from VITE_PI_GH_BASE); types Pull, PullDetail
  types.ts           // PrNode, PrEdge, StackGraph, SessionRef, MatchResult
  stack.ts           // buildStacks(pulls) -> StackGraph; lineageOf(graph, prNumber) (pure)
  match.ts           // matchSessionsToPulls(heads, pulls) -> Map<prNumber, SessionRef[]> + Map<sessionWorkspace, prNumber> (pure)
  usePrGraph.ts      // react-query: useWorkspaceHeads(sessionIds) [daemon] + usePulls(repo) [/gh]
  GraphPane.tsx      // interactive graph (React Flow) + click-to-highlight lineage
  PrDetailPanel.tsx  // PR detail panel (sidecar-sourced) + open-on-GitHub
  SessionsForPr.tsx  // sessions whose workspace is checked out at the PR's branch
  inspectorSection.tsx // the inspector "Workspaces" section (consumed by panels.tsx)
  *.test.ts          // unit tests for stack.ts and match.ts (pure, no network)
```

**Matching algorithm (`match.ts`, pure):** for each `(session, workspace)` whose
`remote_url` maps to repo R, against R's pulls:
1. if `pushed_branch` set and `pushed_branch === pull.headRefName` → match;
2. else if `head_sha === pull.headRefOid` → match (disambiguates fork PRs that
   share a branch name);
3. else no match.
Produce both directions: `prNumber → SessionRef[]` (for G4) and
`(sessionId, workspaceDir) → prNumber` (for G1). At most one PR per workspace; if
several match a sha, prefer the open, most-recently-updated.

**Stack algorithm (`stack.ts`, pure):** nodes = a repo's open pulls. Edge `A→B`
("B stacked on A") iff `B.baseRefName === A.headRefName`. A pull whose
`baseRefName` is **not** any pull's `headRefName` is a stack root; add a synthetic
**base-branch node** labeled with that `baseRefName` (`main`/`master`/`develop`/…
— never hardcoded). `lineageOf(prNumber)` walks `baseRefName` links to the
terminal base node.

`owner/repo` is derived **in the browser** from `remote_url` (handles
`https://…/o/r(.git)` and `git@host:o/r(.git)`); this is the only URL parsing and
it lives in `ghClient.ts`.

### 5.5 Interactive graph (`GraphPane.tsx`)

- **Library: React Flow (`@xyflow/react`)** + a layout pass (**dagre** via
  `@dagrejs/dagre`) laying base node → leaf PRs. Mermaid is **not** used here (it
  can't do click-to-highlight); it stays only for transcript diagrams.
- One subgraph per repo present among the in-scope sessions (multi-repo projects
  render multiple stacks; see open questions).
- Node = one open PR: number, title, owning session count, out-of-date badge
  (filled once detail is fetched), draft styling. Synthetic base node per stack.
- **Click a node** (G4): compute `lineageOf` → highlight that path, dim the rest;
  open the selection panel with `SessionsForPr` (from `match.ts`) and
  `PrDetailPanel` (lazy `usePulls`→detail fetch).
- Read-only canvas (pan/zoom/select). No editing affordances.

### 5.6 Inspector "Workspaces" section (`inspectorSection.tsx`)

- Rendered by `panels.tsx` **only when** `snapshot.metadata.subagent !== true`.
- Reuses the existing `.kv` row pattern under a new `.inspect-section`.
- Per git workspace: local branch, base branch, matched PR (`#N title`, linked),
  out-of-date chip ("behind base" / "up to date" / "checking"), and the unmerged
  stack listed compactly beneath, highlighting this workspace's PR.
- Branch/base render immediately from the snapshot + `useWorkspaceHeads([id])`;
  PR/status fill in async from `usePulls(repo)` + matched-PR detail.

### 5.7 Integration seams (the only edits to existing files)

- `packages/web/src/App.tsx`: `topView: "chat" | "graph"` state + a sidebar/topbar
  toggle that swaps the center grid region for `<GraphPane/>`. Graph scope = idle
  + unarchived + top-level sessions of the selected project (`activity === "idle"
  && !isArchivedSession(s)`; subagents already absent from `session.list`).
- `packages/web/src/panels.tsx`: render `<InspectorWorkspacesSection/>`, gated on
  `metadata.subagent !== true`.
- `packages/web/src/agentApi.ts` + `queryKeys.ts`: the one
  `session.workspace_heads` method + query key.
- `packages/web/vite.config.ts`, `infra/serve.sh`, `infra/dev.sh`: the `/gh` route
  + sidecar process.

Everything else stays inside `pr-graph/` and `packages/gh-sidecar/`.

## 6. Build order (each milestone independently shippable)

1. **Daemon `session.workspace_heads`** — workspaces helper + handler + enum/parse
   + dispatch + view; Rust unit test for `parse` and a handler test over a seeded
   git workspace. *(GitHub-free; shippable alone.)*
2. **gh sidecar + `/gh` route** — `packages/gh-sidecar`, routes, degraded mode,
   `vite.config` proxy, `dev.sh`/`serve.sh` wiring. Smoke-testable with `gh`
   stubbed.
3. **`pr-graph/` core** — `ghClient.ts`, `types.ts`, `stack.ts`, `match.ts`,
   `usePrGraph.ts`, with vitest unit tests for `stack.ts` and `match.ts`.
4. **Inspector Workspaces section** — `inspectorSection.tsx` + the `panels.tsx`
   seam. First user-visible PR data.
5. **Interactive graph** — `GraphPane.tsx`, `PrDetailPanel.tsx`,
   `SessionsForPr.tsx` + the `App.tsx` toggle and React Flow/dagre deps.

## 7. Testing strategy

- **Rust:** `RpcMethod::parse` test; a `session.workspace_heads` handler test that
  seeds a git workspace (the `workspaces/tests.rs` fixtures already create local
  remotes and push) and asserts `head_sha`/`base_branch`, plus the subagent-omit
  guard.
- **Pure TS (`stack.ts`, `match.ts`):** table-driven vitest — linear stack,
  branched stack, root-at-non-main base, fork PRs sharing a branch name
  (sha-disambiguated), no-match, multi-PR-per-sha tie-break.
- **Sidecar:** unit-test the route→argv mapping and `repo`/`number` validation
  with `gh` stubbed; assert no non-`--json`/write subcommands are constructible.
- **Manual/e2e:** documented checklist (idle+unarchived filtering, subagent
  hidden from inspector, degraded GitHub state, click-to-highlight).

## 8. Open questions / loose ends

1. **Multi-repo projects.** A project/session can have several git workspaces
   (different repos). Proposed: one stack subgraph per repo in the graph, repo
   shown as a node group/lane. Confirm vs. a single combined canvas.
2. **Cross-project scope.** v1 scopes the graph to the selected project (matches
   the sidebar). You mentioned "all idle sessions" generally — confirm per-project
   is acceptable for v1, or we add a project-less session list later.
3. **Partially-merged stacks.** v1 fetches only **open** PRs. When a lower PR
   merges, GitHub auto-retargets the upper PR's base to the merged base, so the
   upper correctly bottoms out at the base branch — but we won't *show* the merged
   ancestor. Acceptable, or do we also fetch recently-merged PRs to render them as
   dimmed/checked nodes?
4. **`mergeStateStatus == UNKNOWN`.** GitHub computes mergeability lazily; first
   read can be `UNKNOWN`. Proposed: show a neutral "checking" chip and let
   react-query refetch. Confirm we don't want an explicit poll.
5. **Match precision / ancestry.** Branch-name/exact-sha matching misses when a
   session commits *past* its pushed tip (HEAD ≠ PR head). A later refinement
   could send candidate PR head shas to the daemon to test ancestry
   (`merge-base --is-ancestor`) in the clone. Out of scope for v1 — confirm.
6. **Enterprise / multiple GitHub hosts.** The sidecar uses the host's default
   `gh` auth. Multi-account/GHE (`GH_HOST`) is out of scope; document the single
   assumed auth.
7. **`vite preview` proxy.** Need to confirm `preview.proxy` is honored by the
   pinned Vite version; otherwise use the tailscale-serve route + a direct
   `VITE_PI_GH_BASE` with sidecar CORS (build step 2 check).
8. **React Flow + dagre dependencies.** Two new frontend deps (`@xyflow/react`,
   `@dagrejs/dagre`), justified by the interactive requirement. Confirm acceptable
   vs. a lighter hand-rolled SVG DAG.
9. **Live-git cost.** `workspace_heads` shells `git` per workspace per in-scope
   session. Fine for a handful of idle sessions; if it ever bites, the mitigation
   is batching/short server cache — deliberately deferred per non-goals.
10. **"Out of date" definition.** Locked to "PR head is behind its base"
    (`mergeStateStatus == BEHIND`). Not surfacing local dirty/unpushed signals —
    confirm that's the only signal wanted.

## 9. Security considerations

- The sidecar is **localhost-bound and read-only**, but the `/gh` route exposes it
  through the same origin as the web UI. Anyone who can reach the UI can read your
  PRs via the host's `gh` auth — the **same trust boundary** as the UI, which can
  already drive your sessions. Acceptable for a personal/self-hosted tool; do not
  expose the UI publicly without auth.
- `repo`/`number` inputs are strictly validated; `gh` is always invoked with an
  argv array (no shell), and only `--json` read subcommands are constructible.
- No GitHub token is ever sent to or stored by the browser.
