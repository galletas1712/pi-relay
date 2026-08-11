# M8 — workspace-lib (btrfs) + MCP/OAuth parity + product gaps

Status: **COMPLETE** (2026-08-10). Final feature milestone before M9 (REPL)
and M10 (migrator/soak/cutover). Everything below runs against the TEST rig
only (PG 127.0.0.1:56432, WSS 127.0.0.1:8730, GLM via m1-demo SSE shim 8571).
The live pi-relay deployment was never touched; `~/.config/pi-relay/runtime/mcp.toml`
was used read-only as the config-parity fixture.

## What shipped

### Phase 1 — workspace semantics (real btrfs)

- `packages/workspace-lib`: TypeScript port of `rust/crates/agent-workspace`
  semantics. btrfs probe + `validate_root`, session materialize/ensure/fork/
  destroy, browse + git status/diff, path safety. Shells to `btrfs`,
  `cp --reflink`, `rsync`, `git`. Subvolume detection via inode 256 (no root
  needed). Non-btrfs fallback: plain dirs + loud log; `BRIDGE_WORKSPACE_REQUIRE_BTRFS=1`
  fails boot instead.
- Bridge wiring: `session.create` accepts `workspaces` / `projectId` /
  `mcpSelection` (all inside idempotency hashing); managed sessions run their
  host with cwd = `<workspaceStateRoot>/sessions/<id>/cwd` subvolume.
  Real `workspace.*` contract methods (list/status/diff/browse/file ops),
  `project.*` CRUD, `session.delete` (host stop + workspace destroy + PG
  cascade, idempotent), `session.rename` (PG + `session.renamed` + best-effort
  host `set_session_name`).
- **Live-base guard**: `assertWorkspaceStateRootNotLiveBase()` refuses
  `BRIDGE_WORKSPACE_STATE_ROOT` resolving at/under `~/.local/state/pi-relay`
  (owner's live base) at boot — probe-and-refuse per CONFIG-PATHS.md.
- SPA: M6 mock seam swapped for `createBridgeWorkspaceBackend(client, sessionId)`
  (mock stays exported for tests). prime-harness `context-files.ts` reads
  AGENTS.md from every workspace dir via `PI_RELAY_WORKSPACE_DIRS`.

### Phase 2 — MCP control plane + kernel-mediated model exposure

- `packages/bridge/src/mcp/`: full port of the Rust MCP stack —
  `config.ts` (config.rs/oauth_config.rs bounds, `McpConfigError`, sha256
  fingerprints, loopback validation incl. `::1` fix, tagged-sub-table
  transports; proven against the live mcp.toml), `credentials.ts`
  (serde_json-compatible credential keys, 0600 atomic writes, cross-process
  mkdir lock, 25 ms spin / 10 s timeout / 60 s stale-break), `oauth.ts`
  (SDK-owned RFC 9728/8414/OIDC discovery, DCR with 0600 sidecar, PKCE S256,
  loopback callback, refresh), `manager.ts` (fingerprint-keyed pool,
  bearer_env/oauth/stdio, inventory, selection validation, tool calls).
- Contract methods: `mcp.inventory` (real), `mcp.status`, `mcp.reload`,
  `mcp.select`, `mcp.call`, `mcp.login`/`complete`/`cancel`/`logout`, plus
  `mcp.authChanged` broadcast. Selection locked while a host runs.
- Model exposure is kernel-mediated, per the milestone decision: hosts receive
  `PI_RELAY_MCP_CATALOG` + `PI_RELAY_MCP_CREDENTIALS` +
  `PRIME_HARNESS_EXTRA_SKILLS_DIRS`; the bridge writes per-session generated
  skills (`mcp-<server>/`) that subclass `CatalogMcpIntegration`
  (prime-rlm runtime, `mcp_base.py`) and call through it. OAuth refresh is
  implemented in BOTH the bridge (connect time) and the kernel Python (call
  time) with the same file lock, credential-key derivation, 30 s expiry skew,
  and 0600 atomic replace — re-read inside the lock co-operates with
  concurrent refreshers. Tool names with dots (`mock.echo`) bind via
  sanitized aliases (`mock_echo`) through a PEP 562 `__getattr__`.
- Secrets discipline: bearer tokens via env only; OAuth credentials only in
  the 0600 store at `<workspaceStateRoot>/mcp-oauth-credentials.json`; never
  in PG, never in a session workspace, never in the catalog json.

### Product gaps (G1/G2)

- **Projects** (G1): `project.*` CRUD on the contract (already); session
  grouping in the bridge SPA sidebar (project.list + per-project headers,
  "No project" group last); session.create adopts the project's default
  workspaces when none are passed; project delete NULLs `sessions.project_id`.
- **Session titles** (G1): sidecar — first settle to idle with `name IS NULL`
  → non-streaming GLM call via the shim (`titleShimBaseUrl`/`titleModel`/
  `titleApiKeyEnv` config) on the first user message from the session JSONL →
  normal rename path (PG + `session.renamed` + host `set_session_name`).
  One retry; user rename always wins; titles never block the session.
  `max_tokens: 300` because GLM reasoning burns tokens before content.
- **Transcript rebuild** (G2): `src/transcript.ts` walks the pi session JSONL
  v3 tree (leaf = last id, parents to root) into the web projection's
  `TranscriptBlock[]` (messages + tool execs, results merged by toolCallId).
  `session.getState` gains `transcript.blocks`; `eventStore.rebuildFromState`
  adopts them. Also fixed: attach against a FULLY TRIMMED spool now raises
  typed `event_gap` (was: silent "replay 0 events"), and the spool ring-buffer
  trim SQL casts (`$2::bigint - $3::int`, PG 42725).

### M9 prep (from parent audit)

- server.ts's 33-method dispatch literal split into `src/routes/{common,mcp,
  workspace,project}.ts` with `mergeMethodTables` (duplicate method = boot
  failure). M9's `repl.*` = one new module + one spread; the broadcast hook
  stays server-owned and is injected.

## Verification (all PASS)

| Gate | What | Trace / suite |
|---|---|---|
| W1 | workspace-lib unit: 8/8 on real btrfs (materialize/ensure/fork/destroy/browse/git) | workspace-lib tests |
| W2 | workspace.* + project.* contract end-to-end incl. SPA seam | `m8-w2-contract.jsonl`, `m8-w2-spa.jsonl` |
| W3 | AGENTS.md from ALL workspace dirs reaches the model | `m8-w3-agents.jsonl` |
| M1 | mock-MCP kernel round trip: session.create(mcpSelection) → catalog+skills on disk (no token in catalog) → kernel pre-imports `mcp_bearer` → GLM echo round trip → selection gate rejects `mock.time` → session.delete removes artifacts | `m8-m1-kernel.jsonl` |
| M2 | OAuth state machine vs mock AS: DCR exactly once, PKCE S256, loopback callback, 0600 token persist, inventory healthy, tool call, 1 s TTL → `reauthentication_required` → forced reconnect → refresh exactly once (rotation verified); denial → `access_denied`; cancel/logout | `m8-mcp-oauth.jsonl`, `m8-mcp.test.mjs` 3/3 |
| M3 | real `mcp.inventory` across bearer/oauth/stdio mocks; live mcp.toml parse parity | `m8-mcp-inventory.jsonl`, `m8-mcp-config.test.mjs` 4/4 |
| G1 | project CRUD + sidebar grouping; title sidecar: `session.renamed` "G2 Transcript Marker 91309" after first settle; user rename wins | `m8-gaps.jsonl` |
| G2 | trimmed-spool rebuild: attach(0) → typed `event_gap` → getState transcript.blocks carry user+assistant markers; web eventStore adopts blocks (18/18) | `m8-gaps.jsonl`, eventStore tests |

Regression: bridge b1–b8 PASS (b8 assertions updated — workspace.*/mcp.inventory
are real now), web bridge suite 38/38, `npx tsc --noEmit` clean in bridge + web.

## Not in M8 (owner checklist / follow-ups)

- **Real OAuth logins** (Slack/Linear/Outlook/NVCarPs): the parity matrix W5
  against real providers needs owner browser logins; mock-AS coverage stands
  in. Callback port 14555 matches live config.
- **Workspace search** (product gap): deferred; browse + git cover the demo.
- **M10 migrator**: restore `.pi/backups/20260809-112741/` into the test
  container and verify the migrator there (never live until cutover).
- **Provider watch-item**: NVIDIA SSE streaming still broken endpoint-wide;
  shim is demo-only. Decide before cutover.

## Key paths

- `packages/workspace-lib/` (new), `packages/bridge/src/mcp/` (new),
  `packages/bridge/src/routes/` (new), `packages/bridge/src/{titles,transcript}.ts`
- `extensions/prime-rlm/python/prime-rlm-runtime/src/prime_rlm_runtime/mcp_base.py`
  (CatalogMcpIntegration; venv auto-reprovisions via content-hash stamp)
- `extensions/prime-harness/src/skills.ts` (`PRIME_HARNESS_EXTRA_SKILLS_DIRS`)
- Tests: `packages/bridge/test/{m8-w2,m8-w3,m8-mcp,m8-mcp-config,m8-m1-kernel,m8-gaps}*`,
  mocks `test/mock-oauth-mcp.mjs`, `test/mock-stdio-mcp.cjs`
- Traces: `.pi/m1-demo/traces/m8-*.jsonl`; progress: `.pi/m1-demo/M8-PROGRESS.md`
