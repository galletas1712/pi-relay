# M6 — Frontend rewiring to the bridge (contract v0), bridge profile

Date: 2026-08-09. Author: m6-frontend (subagent). Status: **complete, F1–F6 green.**

The pi-relay SPA (`packages/web`) now has a second, self-contained data layer
under `src/bridge/` that speaks **slim contract v0** to the M5 bridge
(`packages/bridge`, WS `127.0.0.1:8730`). It is gated behind a backend profile
flag; the legacy data layer is untouched and remains the default.

## Enabling the bridge profile

`resolveBackendProfile()` (`src/bridge/profile.ts`), precedence:

1. `?backend=bridge` query param (persisted to localStorage `pi-relay:backend`)
2. localStorage `pi-relay:backend`
3. `VITE_BACKEND=bridge` build-time env
4. default: `legacy`

`main.tsx` lazy-imports `bridge/BridgeApp.tsx` only in the bridge profile, so
the legacy bundle never carries it.

## Browser auth: the vite WS proxy

Browsers cannot set `Authorization`/`Origin` headers on WebSocket upgrades, but
the bridge requires both (exact-match Origin allowlist + bearer token). The
bridge-profile app therefore connects **same-origin** to `/__bridge-ws`; the
vite dev server proxies that path to `ws://127.0.0.1:8730` with the upgrade
headers injected (`vite.config.ts`). The token is read **server-side only**
from `.pi/m1-demo/.bridge-auth-token` (override: `BRIDGE_TOKEN_FILE`,
`BRIDGE_PORT`, `BRIDGE_PROXY_ORIGIN` env vars) and never enters the bundle.
Node-side tests inject a `ws` WebSocket via the client's `webSocketFactory`.

## Data layer (`src/bridge/`, ~2950 LOC + 586 vendored)

| file | role |
|---|---|
| `types.ts` | contract v0 wire types: `{id,method,params}` → `{id,result}`/`{id,error{code,message,data?}}`; event envelopes `{event,sessionId,seq,data,at,replayed?}`; all method params/results; typed error codes |
| `client.ts` | `BridgeClient`: injectable WS factory, request/response correlation, per-request timeouts, reconnect with backoff, `BridgeRequestError` (typed code+data) vs `BridgeTransportError` (uncertain outcome), full method facade, `newIdempotencyKey()` |
| `eventStore.ts` | pure reducer: `SessionProjection` — watermark dedupe, gap detection, transcript blocks (assistant/user messages, tool.exec cards incl. ipython cells), subagent map, optimistic local echo for user prompts, `rebuildFromState`, `seqContiguity` assertion helper |
| `sessionStore.ts` | `BridgeSessionStore`: attach/resume lifecycle (`fromSeq`=watermark), **optimistic attach** (bridge delivers replay frames *before* the attach response; replay holds the per-session event lock server-side), serialized `reattachAll` on reconnect, `event_gap` → `session.getState` rebuild → re-attach at head |
| `useBridge.tsx` | React bindings: `BridgeProvider`, TanStack Query `useSessionList` (2 s refetch — same discipline as legacy), `useSessionProjection` via `useSyncExternalStore`, `useSubagentTree` invalidated by lifecycle events |
| `components/` | `SessionList`, `TranscriptView`, `Composer` (idle→prompt.send / running→steer+followUp+abort, typed errors inline), `SubagentTreeView` (live projection merged with durable `subagent.tree`), `DiffPanel` |
| `BridgeApp.tsx` | profile shell: sessions · transcript+composer · agents/diff rail; `?s=<sessionId>` and `?rail=diff` deep links |
| `profile.ts` | backend profile resolution |
| `workspace.ts` | `WorkspaceBackend` seam + **local mock** (`workspace.*` is a reserved namespace in v0 — bridge returns typed `not_implemented`; real RPCs land in M7). Mock serves a 3-file fixture (modified planner.ts, deleted ipython.ts, new docs) |
| `bridge.integration.test.ts` | F1–F4 against the real bridge, env-gated (`BRIDGE_INTEGRATION=1`), writes traces |

## Diff rendering: pierre + vendored T3 glue

Installed at the T3 pins: `@pierre/diffs@1.3.0-beta.10`,
`@pierre/trees@1.0.0-beta.4`. pierre's exports map blocks T3's deep imports, so
`parsePatchFiles`/`FileDiffMetadata` are imported from the package root instead.

Vendored into `src/bridge/vendored/` (MIT, provenance headers, T3 commit
6f69b44): `diffRendering.ts` (248 LOC, imports adapted), `turnDiffTree.ts`
(183, local `TurnDiffFileChange` struct, `toSorted`→`[...].sort` for the app's
tsconfig lib), `baseRefChoices.ts` (70, local `VcsRef`), `diffCollapse.ts` (17,
verbatim). The **AnnotatableCodeView cluster was deferred** — it drags in T3's
own stores/router; the plain `FileDiff` renderer + stats + collapse-all +
diff→tree grouping cover the milestone. `DiffPanel` renders the mock
`workspace.git_diff` through `getRenderablePatch` → `FileDiff`
(`disableWorkerPool`, `unsafeCSS: DIFF_SURFACE_THEME_UNSAFE_CSS`, theme from
`resolveDiffThemeName`); `bridge.css` maps pierre's `--code-background` /
`--code-foreground` tokens onto the app palette.

## Contract-v0 semantics the UI honors

- **Idempotency**: every `session.create`/`prompt.send` carries
  `newIdempotencyKey(...)`; replay responses (`replay:true`) skip the
  optimistic echo; `idempotency_conflict` surfaces as a typed error.
- **Resume**: attach at the applied watermark; dupes (`seq <= watermark`, incl.
  `replayed:true` frames) are dropped with unchanged state identity; a
  discontinuity flags `projection.gap` in a banner but still applies.
- **event_gap**: typed `{minAvailable, headSeq}` → rebuild from
  `session.getState` (which carries **no transcript bodies** — the trimmed
  range is unrecoverable in v0; prior blocks are kept and a "rebuilt" banner is
  shown) → re-attach at head.
- **Typed errors**: `session_busy`, `session_not_found`, `host_unavailable`,
  `workspace.*`/`mcp.*` → `not_implemented`, unknown method →
  `method_not_found` — all rendered distinctly; transport errors carry an
  "outcome uncertain, reconcile via getState" notice (bridge journals every
  command).
- **User messages**: stream envelopes carry no text → the store keeps an
  optimistic local echo and matches the stream's `role:"user"` start to it.
- **Subagents**: `subagent.lifecycle` (phase per `rlm_child_id`, terminal
  statuses sticky against duplicate/out-of-order phases) + `subagent.tree`
  (durable) merged in the tree view.

## Verification (all traces in `.pi/m1-demo/traces/`)

| gate | result | evidence |
|---|---|---|
| **F1** happy path | PASS | `m6-f1.jsonl`: 21 contiguous events — idle→running, user start/end, thinking deltas, toolcall, ipython tool.exec start/update/update/end (marker in args+result), text, idle; watermark=headSeq=21, gap=null |
| **F2** mid-stream resume | PASS | `m6-f2.jsonl`: socket dropped mid-ipython at watermark 13 with a 4 s reconnect hold; reattach fromSeq=13 replayed exactly seqs 14–16 (`replayed:true`); stream continued to 27, contiguous, no dupes, watermark=headSeq |
| **F3** subagent fanout | PASS | `m6-f3.jsonl`: b7-style prompt spawned rlm children `m6f3a`/`m6f3b`; lifecycle admitted→completed observed live; `subagent.tree` lists both with depth/status |
| **F4** idempotency + typed errors | PASS | `m6-f4.jsonl`: double-create with same key replays (`replay:true`, same sessionId); different params → `idempotency_conflict`; bogus attach → `session_not_found`; `workspace.list/read/git_diff` + `mcp.inventory` → `not_implemented`; unknown method → `method_not_found`; second prompt mid-turn → `session_busy` |
| **F5** legacy untouched | PASS | `npm run build` (tsc + vite + CSP check) green; full web suite: **688 passed, 4 skipped** (the 4 integration tests skip without `BRIDGE_INTEGRATION=1`); 32 new unit tests included (reducer ordering/dedupe/gap, client wire semantics, store attach/resume/rebuild, vendored glue) |
| **F6** visual smoke | PASS | `m6-f6-diff.png` (headless Chrome via vite dev server): DOM assertions confirm the bridge profile shell, WS status **open through the `/__bridge-ws` proxy** (proves header injection works browser-side), live session list from the bridge, and the pierre-rendered 3-file mock diff |

Integration run: `BRIDGE_INTEGRATION=1 npx vitest run packages/web/src/bridge/bridge.integration.test.ts`
→ 4/4 passed in 27 s against the real bridge.

## Known limitations / M7 handoff

- `workspace.ts` mock → swap `createMockWorkspaceBackend()` for a bridge-backed
  adapter at the single construction site in `BridgeApp.tsx` once the bridge
  implements the reserved `workspace.*` RPCs.
- `session.getState` carries no transcript bodies (v0); a trimmed spool range
  is unrecoverable — surfaced via the rebuild banner, prior blocks retained.
- AnnotatableCodeView (comment-on-diff) deferred; needs a local annotation
  store to replace T3's.
- The vite proxy is dev-only; a production deployment needs the same header
  injection at whatever serves the SPA (or a bridge-side auth mechanism that
  browsers can satisfy, e.g. ticket/cookie).
- `rlm-heartbeat`-driven sessions and goal events are not yet projected
  (contract v0 exposes them only via `session.*`).
