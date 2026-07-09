# Local MCP client: session-scoped design and implementation plan

Status: session-scoped refactor rebased onto the provider-route and compact web
workspace architecture. The combined New Session setup, public RPC, provider
routing, compaction recovery, tool claim, and PostgreSQL migration paths are
implemented and locally validated. Windows and macOS validation remain
follow-up work. This is the live checklist and durable design record.

## Objective

MCP selection happens only while creating a new session. The client chooses a
bounded subset of operator-allowed tools from a dedicated inventory, and the
daemon validates that selection against a semantic revision before creating the
session. The resulting MCP-only manifest is content addressed, persisted
transactionally with the session, and frozen for that session and every child.

`ToolRegistry` remains first-party and static. Provider requests combine the
session profile's first-party declarations with the exact persisted MCP
declarations. No later inventory refresh, reconnect, configuration change, or
`tools/list_changed` notification can mutate an existing session's prompt,
declarations, routes, or selected names.

The initial transport remains local stdio using the Rust `rmcp` SDK. The
existing process cleanup, environment boundary, protocol bounds, result
normalization, and exact-contract route verification remain in scope.

## Live checklist

### Approved design

- [x] Replace the turn-scoped invariant with one immutable MCP-only manifest per
  durable session.
- [x] Make omitted or empty selection explicitly MCP-free; legacy sessions with
  no binding never fall back to current inventory.
- [x] Specify dedicated `mcp.inventory` and revision-fenced `session.start.mcp`
  contracts using raw server/tool identities only.
- [x] Remove required/optional admission, subagent exposure/audience, and future
  search/call broker concepts from the design.
- [x] Specify exact MCP inheritance for full and read-only children while
  retaining child-specific first-party profiles.
- [x] Specify conditional PI.md output containing server IDs and exposed names
  only.

### MCP inventory and frozen manifests

- [x] Refactor `McpCatalogManifest`/`McpTurnSnapshot` into MCP-only
  `McpSessionManifest`/`McpSessionSnapshot`.
- [x] Add a deterministic health-independent inventory revision that changes
  for semantic config, allowlist, or contract changes.
- [x] Add bounded provider-specific declaration token estimates to inventory.
- [x] Validate sorted/deduplicated raw selections atomically and return
  `mcp_inventory_changed`, `mcp_selection_invalid`, or `mcp_unavailable`.
- [x] Remove `McpSnapshotAudience`, `SubagentExposure`, `required`, optional/LKG
  candidate admission, and related tests/config documentation.
- [x] Preserve exact-route revocation/change/unavailable checks and no replay.

### Persistence and runtime

- [x] Persist one content-addressed manifest binding in the session creation
  transaction.
- [x] Treat a missing binding as an explicit empty snapshot, including sessions
  created before this schema.
- [x] Delete turn bindings, legacy-zero materialization, action MCP columns, and
  MCP-specific claim/bind machinery; restore ordinary action claiming/events.
- [x] Load the frozen session snapshot for model requests, retries, token
  counting, title generation, compaction, recovery, `tools.list`, and tool calls.
- [x] Copy the exact parent's MCP manifest binding into every full/read-only
  child creation transaction.
- [x] Render a bounded conditional PI.md MCP section without schemas,
  descriptions, health, revisions, or fingerprints.

### Public RPC and web New Session UI

- [x] Add typed `mcp.inventory` request/response and a distinct web query key.
- [x] Add typed `session.start.mcp` serialization; omission means no MCP.
- [x] Make `tools.list(session_id)` use only the frozen session manifest and
  make no-session results first-party-only.
- [x] Add pure selection grouping, tri-state, toggling, deterministic payload,
  revision reconciliation, and estimate-total functions.
- [x] Add an accessible compact New Session setup above the composer:
  Workspaces first and MCP tools second for projects, MCP only for host
  sessions, with one coordinated disclosure, bounded lists, tri-state choices,
  health, selected count, and clearly labelled estimated MCP context added.
- [x] Gate inventory reads on connection, valid route reads, and no selected
  session; keep loading/error/retry local and fail closed for stale nonempty
  selection.
- [x] Prevent deselecting the final project workspace so `0 of N` can never
  serialize as omission/all.
- [x] Preserve selection across uncertain start failure; clear after definite
  creation success/new-session reset. Clear on draft provider-kind changes and
  retain on effort-only changes.
- [x] Fence uncertain New Session retry IDs by deliberate workspace
  inclusion/branch, MCP, model, and effort edits while keeping automatic
  inventory reconciliation and internal reset/success cleanup generation-neutral.
- [x] Keep retained inventory removal-only and fail closed for a nonempty
  selection while fetching or errored; disable duplicate Retry and reconcile a
  successful refresh before submission.
- [x] Treat healthy zero-tool servers as non-selectable and omit phantom
  selection warnings/readiness, while keeping both setup disclosures
  viewport-bounded and rederiving workspace controls when kind changes.

### Tests, documentation, and validation

- [x] Adapt MCP catalog/manager/transport tests to inventory and session
  selection semantics.
- [x] Add store tests for atomic binding, reconnect, reference release,
  legacy-null behavior,
  and exact child inheritance.
- [x] Add daemon public-RPC tests for inventory/start fencing, prompt omission
  and names, frozen later work, session `tools.list`, and child inheritance.
- [x] Add web pure-state, API serialization, and static
  markup/accessibility/omission tests.
- [x] Update websocket RPC, architecture/store/web UI docs, README, and config
  examples.
- [x] Run Rust formatting, workspace check/tests, affected-crate Clippy, and web
  typecheck/tests/build. Record environmental skips without claiming unexecuted
  PostgreSQL or Windows coverage.
- [x] Execute the PostgreSQL-backed store/daemon test bodies, including the
  consolidated selected-session public-RPC integration scenario, with
  `PI_RELAY_TEST_DATABASE_URL`.
- [ ] Validate on Windows and macOS.

## Exact invariants

1. **Session selection only.** The agent has no discovery or mutation tools.
   Selection is accepted only by `session.start`. An omitted or empty MCP field
   creates an MCP-free session.
2. **Immutable session manifest.** A selected session references exactly one
   content-addressed MCP-only manifest for its whole lifetime. Existing
   sessions never adopt inventory refreshes or `tools/list_changed`.
3. **Provider declarations are authoritative.** The manifest stores exact
   ordered OpenAI and Anthropic MCP declarations. Every request prepends the
   appropriate stable first-party profile. PI.md contains only a compact list
   of selected server IDs and exposed names.
4. **Revision fencing.** Inventory revision is a durable semantic hash of
   configured server identities, semantic config/allowlists, and complete
   operator-allowed contracts. Health is excluded. A stale revision or invalid
   identity rejects the entire selection before session insertion.
5. **Required-on-selection.** Every selected server must initialize and validate
   its complete allowed catalog at creation. An unavailable unselected server
   does not gate creation. Later outages preserve declarations and produce
   ordinary bounded call errors.
6. **No unsafe substitution/replay.** Calls resolve by server config
   fingerprint, raw name, and contract fingerprint. Revocation, contract
   change, unavailability, timeout, and protocol failures never substitute a
   newer route or automatically replay a possibly side-effecting call.
7. **Exact inheritance.** Full and read-only children reuse the parent's exact
   MCP manifest. Their first-party profile still removes parent-only delegation
   tools. Read-only describes filesystem access only and does not constrain
   remote MCP side effects.
8. **Byte stability.** Retries clone a built request. Later turns, count calls,
   title sidecars, compaction, recovery, and resumed work rebuild only from the
   persisted system prompt and manifest. Compaction never rewrites the prompt.
9. **Bounds.** Config, inventory, selection, declarations, schemas, arguments,
   results, and prompt summary all have hard limits. Large catalogs are managed
   through operator allowlists, per-tool selection, and context caps—not model
   discovery.
10. **Static registry.** MCP names never enter `ToolRegistry`; dispatch checks
    the frozen MCP snapshot before first-party execution.

## Wire contracts

### `mcp.inventory`

Provider-neutral request:

```json
{ "provider": "openai" }
```

Bounded response:

```json
{
  "revision": "sha256...",
  "servers": [{
    "server": "workspace",
    "revision": "sha256...",
    "health": "healthy",
    "tools": [{
      "raw_name": "read_file",
      "description": "Read a file",
      "context_token_estimate": 94
    }]
  }]
}
```

The estimate is computed from the exact declaration JSON for the selected
provider and is labelled by the UI as approximate MCP context added, never as
total context. Inventory can report unavailable servers, but only a healthy,
fully refreshed server can be selected for a new binding.

### `session.start.mcp`

```json
{
  "mcp": {
    "inventory_revision": "sha256...",
    "servers": [
      { "server": "workspace", "tools": ["read_file", "search"] }
    ]
  }
}
```

Servers and raw names are sorted, unique, nonempty, and bounded. No schemas,
descriptions, config, command lines, environment names/values, credentials, or
fingerprints are sent by the client. The daemon checks a replayed session ID
before resolving current inventory. Empty servers are normalized to no MCP.

Errors:

- `mcp_inventory_changed`: the semantic inventory revision is stale; the UI
  refreshes and reconciles without silently selecting new or changed tools.
- `mcp_selection_invalid`: duplicate, unknown, disallowed, or over-limit
  identities.
- `mcp_unavailable`: a selected server cannot currently validate its full
  catalog.

## Manifest and persistence

`McpSessionManifest` is canonical JSON hashed with SHA-256 and contains:

- format version and semantic inventory/server revisions;
- ordered selected tools with raw server/tool identity, stable exposed name,
  server config fingerprint, canonical schema, and contract fingerprint;
- exact ordered MCP-only OpenAI and Anthropic declarations;
- the content-addressed manifest fingerprint.

It contains no health, process IDs, connection epochs, secrets, first-party
tools, or subagent audience. Selection filtering preserves exposed names from
the complete inventory so removing a collision participant cannot rename a
selected tool.

PostgreSQL uses:

```sql
create table mcp_session_manifests (
    fingerprint text primary key,
    manifest jsonb not null,
    created_at timestamptz not null default now(),
    last_used_at timestamptz not null default now()
);

alter table sessions
    add column mcp_manifest_fingerprint text null
        references mcp_session_manifests(fingerprint);
```

The manifest row, nullable session reference, initial transcript, events, and
actions commit in one transaction. A null reference always means explicit empty
MCP; it is never a request to inspect the manager. Children copy the same
fingerprint transactionally. Content-address collisions are validated by
comparing canonical JSON.

The feature branch's turn-binding tables, legacy-zero table, action
fingerprints, and special action claim transaction are removed rather than
migrated. They have not shipped. Ordinary action attempt/lease ownership and
tool-start events remain authoritative.

## Runtime and prompt

`SessionConfig` carries an opaque persisted manifest value/reference so
`agent-store` stays independent of `agent-mcp`. Loading config validates and
rehydrates `McpSessionSnapshot`; missing manifests produce the explicit empty
snapshot. `DispatchAction` should carry a nonoptional snapshot when practical.

Provider arrays are:

```text
stable first-party declarations for this session profile
+ frozen MCP declarations from McpSessionSnapshot
```

The persisted PI.md includes a section only for selected sessions:

```text
### Selected MCP tools

- workspace: `mcp__workspace__read_file`, `mcp__workspace__search`
```

It does not duplicate declarations, descriptions, schemas, health, revisions,
or fingerprints. The summary is fully rendered or selection is rejected for
exceeding its bound; it is never silently truncated.

`tools.list(session_id)` combines the session profile's first-party tools with
the frozen manifest. Without a session it returns first-party tools only;
`mcp.inventory` exclusively owns new-session discovery.

## Subagents

Both model-facing and RPC delegation load/copy the parent's exact manifest
binding. Full and every read-only sibling therefore have byte-identical MCP
declarations and routes, while their provider arrays use the subagent
first-party profile. A parent with no binding creates MCP-free children.

There is no `subagents` config field, audience enum, or exposure filtering.
Read-only subagents can invoke remote tools with side effects because read-only
status applies to local filesystem permissions, and the user explicitly chose
the inherited MCP set at parent session creation.

## Inventory refresh and route degradation

`tools/list_changed` receipt still marks a client uncertain before call
admission. Reconnect/refresh updates only the global New Session inventory,
revision, and health. Publication happens after bounded full-catalog
validation. Existing manifests and provider-visible bytes do not change.

Existing calls continue to return:

- `mcp_server_unavailable`;
- `mcp_tool_contract_changed`;
- `mcp_tool_revoked`;
- bounded timeout/protocol errors.

No result is automatically replayed.

## Explicit non-goals

- Agent-facing MCP search, discovery, enable/disable, or generic call brokers.
- Streamable HTTP/SSE MCP transport, OAuth, or remote credential flows.
- MCP resources/prompts as independently exposed agent capabilities.
- MCP sampling, elicitation, or server-initiated model work.
- Dynamic `ToolRegistry` mutation or prompt rewriting.
- Automatic replay after timeout/disconnect.
- Cross-daemon distributed MCP connection ownership.

## Implementation notes

- 2026-04-16: The pushed implementation was inspected and confirmed to bind
  complete first-party+MCP snapshots per turn, adopt later candidates, use
  audience filtering, and persist action/turn fences. Those semantics are being
  removed rather than adapted.
- 2026-04-16: The approved replacement separates MCP-only declarations from
  first-party profiles, uses one transactional session reference, makes legacy
  null explicit-empty, and gives all children exact inheritance.
- 2026-04-16: Existing stdio transport, environment security, canonicalization,
  declaration ordering, provider accounting, cancellation, cleanup, exact-route
  validation, and result normalization remain reusable.
- 2026-04-16: Local validation completed: `cargo fmt --all`,
  `cargo check --workspace`, `cargo test --workspace`, focused MCP/prompt/store/
  daemon suites, affected-crate Clippy with warnings denied (allowing only the
  workspace's pre-existing `too_many_arguments` warning), and web tests/build.
  Full workspace Clippy still stops on the pre-existing
  `agent-store::switch_active_leaf` argument-count lint. The web package has no
  separate formatting script; its build runs `tsc -b`. PostgreSQL-backed test
  bodies self-skipped because `PI_RELAY_TEST_DATABASE_URL` was unset. Windows
  was not tested.
- 2026-04-16: Review follow-up restored the durable ordinary tool
  pending-to-running CAS and `tool.started` ordering, restored exact ordered
  first-party+MCP provider toolset fingerprints for usage anchors, and enforced
  exact child/parent MCP binding equality inside the session creation
  transaction. Failed-refresh global catalog coherence and picker collapse
  remained under review.
- 2026-04-16: Review follow-up also made provider switches and unhealthy
  selections fail safe in the web picker, added the visible inherited remote
  side-effect warning and bounded internal scrolling, and added consolidated
  daemon public-RPC coverage. PostgreSQL-backed bodies still require
  `PI_RELAY_TEST_DATABASE_URL`; do not treat a self-skipped local run as body
  execution.
- 2026-07-08: Final review follow-up made aggregate inventory coherence honor a
  stale participant after its client is dropped, withheld successful inventory
  responses and all bindings until full-catalog recovery, and retained the
  intended non-gating behavior for an ordinary unselected outage with a known
  coherent catalog. The MCP picker conditionally omits closed controls.
  Regression coverage exercises both two-server outage states,
  failed-refresh old/incomplete revision fencing and recovery, and closed
  picker markup.
- 2026-07-08: Validation passed with `cargo fmt --all -- --check`,
  `cargo check --workspace`, `cargo test --workspace`, warning-denied
  all-target `agent-mcp` Clippy, all 30 `agent-mcp` tests, all 218 web tests,
  the web production build, and `git diff --check`.
  `PI_RELAY_TEST_DATABASE_URL` was unset, so PostgreSQL-backed bodies
  self-skipped. Windows was not tested.
- 2026-07-08: Follow-up stress testing exposed a fixture synchronization race:
  its exit marker preceded transport EOF observation, so inventory could still
  report the old healthy state. The outage fixture now exits on an explicit
  request after the initial healthy assertion, and the test waits on a lossless,
  test-only client-liveness notification before refreshing. The regression
  passed 100 consecutive runs, all 30 `agent-mcp` tests, the full Rust
  workspace, warning-denied workspace Clippy, formatting, and
  `git diff --check`.
- 2026-07-09: Rebasing onto the provider-route/compact-UI main line retained
  route snapshots, no-op persistence, empty-dispatch short-circuiting, hot
  queue indexes, replayed-compaction visibility, request retry construction,
  and reasoning-effort hooks. `NewSessionSetup` now composes Workspaces before
  MCP with one disclosure and host-only MCP behavior. PostgreSQL-backed
  workspace tests execute the combined provider-route plus MCP dispatch,
  selected-MCP compaction recovery, tool CAS, exact hot indexes, and additive
  MCP schema migration. The web integration asserts the exact sibling
  `workspaces`/`mcp` start payload and canonical route navigation.
- 2026-07-09: Post-rebase review fixes preserve the pending status through the
  proactive compaction gate, fence uncertain new-session retries by deliberate
  workspace/MCP/provider setup edits, and treat retained MCP inventory as
  removal-only and not submission-ready while fetching or errored. Workspace
  picker identity now includes ordered directory and effective-kind pairs,
  both setup lists are viewport-bounded, and zero-tool servers cannot create a
  phantom selection. Focused web integration/pure/layout tests passed. A
  Docker-backed PostgreSQL daemon regression executed the real selected-MCP
  pending-to-blocked proactive transition without a model error; the existing
  reactive running-action regression also passed.
- 2026-07-09: Final follow-up validation passed `cargo fmt --all -- --check`,
  `cargo check --workspace`, the full Rust workspace tests with a temporary
  PostgreSQL 16 database (including 211 daemon and 68 store tests),
  warning-denied all-target `agent-daemon` Clippy while allowing only the
  documented pre-existing `agent-store::switch_active_leaf`
  `too_many_arguments` lint, all 454 web tests, the web production build, the
  unused TypeScript check, and `git diff --check`. Windows and macOS remain
  untested.
