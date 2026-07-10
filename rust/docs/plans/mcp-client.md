# MCP client: session-scoped design and implementation plan

Status: the session-scoped MCP client, generic Streamable HTTP transport, and
generic OAuth Stages 1-4 are implemented. Public sanitized RPCs and the React
New Session login/logout UI use the pinned rmcp OAuth state machine and the
daemon-owned loopback/manual-completion boundary.
Daemon-owned file credentials, restart restoration, refresh, bounded
authenticated transport, sanitized public auth RPCs, local logout, and the web
New Session OAuth UI are also implemented. Windows and macOS validation remain
follow-up work. This is the live checklist and durable design record.

**Implementation ceiling:** Codex is the OAuth complexity ceiling. Follow
`codex-rs/rmcp-client/src/perform_oauth_login.rs`,
`codex-rs/rmcp-client/src/oauth_http_client.rs`, and rmcp 1.8
`AuthorizationManager`/`OAuthState`. Do not add a stricter parallel OAuth
protocol implementation, exhaustive secret-buffer framework, or bespoke
provider behavior in pi-relay.

rmcp owns RFC 9728/RFC 8414/OIDC discovery, wire types, path fallbacks, Dynamic
Client Registration, endpoint validation, scope selection, PKCE/state, and
token exchange. Authorization endpoints may retain provider query parameters,
as in Codex/rmcp. `oauth2` is also a direct default-features-disabled dependency
used only to reconstruct rmcp token responses from durable records, while
remaining transitive through rmcp auth. There is no parallel pi-relay OAuth
protocol implementation.

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

Transports are operator-configured local stdio or generic remote Streamable
HTTP using the Rust `rmcp` SDK. The existing process cleanup, environment
boundary, protocol bounds, result normalization, and exact-contract route
verification remain in scope. There is no built-in or provider-specific server
catalog.

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

### Generic remote stage 1: Streamable HTTP prerequisite

- [x] Replace the stdio-only config fields with an explicit typed transport:
  `stdio` or `streamable_http`. Keep timeouts, concurrency, and tool allowlists
  common to both.
- [x] Add generic Streamable HTTP discovery and calls through `rmcp` 1.8,
  including JSON/SSE responses, stateless servers, stateful session IDs, empty
  successful notification responses, and bounded SSE reconnection.
- [x] Add an optional tagged HTTP auth policy with a `bearer_env` prerequisite.
  Resolve its token from a named environment variable at connection time. Keep
  the value out of config identity, manifests, Debug, logs, RPC, and model
  context; the environment variable name remains semantic route config. This
  does not satisfy the interactive OAuth objective.
- [x] Disable SDK stale-session request replay. A stale session or failed/timed
  out POST closes the route; a later operation may establish a fresh client,
  but the possibly-sent call is never replayed.
- [x] Bound bytes before JSON deserialization and SSE line/event accumulation,
  cap events and reconnect attempts, and apply connect/header and body-idle
  timeouts. Timeout/task cancellation aborts only the in-flight POST before a
  separately bounded `notifications/cancelled` and stateful DELETE cleanup.
- [x] Scrub the exact resolved bearer from every inbound JSON value, SSE field,
  session ID, result, and error before it reaches rmcp logging or catalog/result
  processing. Retain downstream fixed-size catalog/result/error bounds.
- [x] Restrict cleartext HTTP to loopback hosts for local development and
  bounded integration tests; require HTTPS for remote hosts.
- [x] Add bounded fake Streamable HTTP coverage for initialize, initialized,
  tools/list, calls, bearer reflection, stateless/stateful JSON and SSE,
  ignored server instructions, adversarial chunking/stalls, reconnect
  exhaustion, cancellation/DELETE observations, `tools/list_changed`, and no
  replay.

### Generic OAuth accepted plan

OAuth is generic configuration and daemon behavior, never a vendor catalog.
The `oauth` and `bearer_env` variants are mutually exclusive by construction.
No server name, issuer, endpoint, client ID, or scope is built into pi-relay.

#### OAuth Stages 1 and 2: rmcp discovery and login (implemented)

- [x] Use a minimal Codex-like OAuth config: optional `client_id` (omission
  means DCR), optional `scopes`, optional RFC 8707 `resource`, and operational
  callback port/timeout. There is no client secret, registration enum, issuer
  pin, trusted-origin list, scope ceiling, or custom auth method.
- [x] Apply the existing Streamable HTTP URL rule unchanged: HTTPS remotely,
  HTTP on loopback, and query-bearing MCP URLs allowed and preserved. Delegate
  discovery URL/path behavior and endpoint validation to rmcp.
- [x] Keep OAuth routes immediately unavailable/login-required at startup
  instead of running an independent discovery gate. The Stage 4 status check
  calls the thin `AuthorizationManager::discover_metadata` wrapper.
- [x] Follow Codex `start_authorization`: use
  `OAuthState::start_authorization` for DCR and
  `discover_metadata`/`set_metadata`/`configure_client`/
  `get_authorization_url` plus `AuthorizationSession::for_scope_upgrade` for a
  configured public client. rmcp owns scope precedence, DCR, PKCE/state, and
  token exchange. Preserve provider authorization-endpoint query parameters.
- [x] Use a bounded direct/no-proxy OAuth adapter with separate redirect-follow
  and redirect-stop reqwest clients. Discovery and DCR follow redirects; token
  exchange stops.
- [x] Include OAuth mode, the full MCP URL, client ID, scopes, and resource in
  semantic route identity. Exclude callback port/timeout, metadata, DCR output,
  tokens, state, and PKCE. Preserve fixed legacy stdio and bearer fingerprints.
- [x] Allow only the generated loopback redirect URI for the transaction: one
  listener, one unique per-login callback path, one bounded lifetime, one
  completion, exact state, exact method/path, bounded query, and fail-closed
  OAuth error handling. The authorization URL is returned for explicit opening;
  browser launch remains a caller concern.
- [x] The loopback listener runs on the daemon host. Therefore automatic
  browser callback works only when the browser can reach loopback on that same
  host. For a remote/headless daemon, the user completes authorization in
  their browser, then pastes the **entire callback URL** (not only `code`) into
  the web dialog, which submits it to the bounded public completion RPC and
  existing manager completion boundary.
- [x] Route listener and manual callback submissions through one per-login owner
  task. Dropped waiters do not cancel it; timeout, cancellation, and shutdown
  clean it up. Success/cancel acknowledgement occurs only after credential
  handoff, reservation/flow removal, and listener release. Retain only rmcp
  `OAuthState` in memory. Public errors are local categories and authorization
  URLs are redacted from Debug.

#### OAuth Stage 3: daemon credentials, refresh, and transport injection (implemented)

- [x] Make the daemon the sole owner of DCR and token records. The versioned,
  bounded aggregate JSON file is keyed by configured server ID plus exact MCP
  URL, with static-client/resource compatibility checks, restrictive
  permissions, sibling temporary writes, and atomic replacement. It lives
  directly under the daemon state root, never in Postgres or a session
  workspace. Missing means empty; empty, corrupt, oversized, and I/O failures
  are sanitized explicit store errors. An unreadable store is preserved and
  fail-closed for OAuth status/login/logout while unrelated routes and the
  daemon continue; the file backend has no repair, migration, cross-process
  locking, keyring, or database fallback.
- [x] Persist the public client ID, access token, optional refresh token,
  absolute expiry, granted scopes, and minimal configured resource/client
  identity. Save only after callback/listener cleanup and before acknowledging
  browser/manual completion. The store is plaintext protected by OS file
  permissions; optional keyring wrapping is future work.
- [x] Rediscover through rmcp and reconstruct `OAuthState`/
  `AuthorizationManager` on restart. Refresh at authenticated route acquisition
  with Codex's 30-second skew under one per-server mutex. Concurrent demand
  shares the manager; access/refresh/expiry/scope rotation is atomically
  persisted directly from rmcp; if a provider omits a replacement refresh
  token, the stored rmcp credentials omit it too. Refresh contacts the token
  endpoint before atomic replacement; transient failure, cancellation, or
  replacement-write failure preserves the previous durable record for a later
  retry.
- [x] Inject the resulting bearer only into requests to the exact configured
  MCP resource through the existing bounded client. Discovery/registration
  endpoints never receive it. POST, common GET/SSE, and DELETE retain shared
  attachment/scrubbing and bounded behavior. A 401 closes the route without
  replaying the current request; only a later operation may refresh/reconnect.
- [x] Keep credentials and OAuth transaction material out of Debug, logs,
  errors, status payloads, fingerprints, manifests, PI.md, provider/model
  context, traces, and metrics. Internal status exposes only unsupported,
  unknown, login-required, reauthentication-required, OAuth-ready, bearer, or
  non-OAuth distinctions. Local logout cancels a pending login, removes
  in-memory/file credentials, closes the route, and leaves frozen manifests
  unchanged; it does not perform remote revocation, matching Codex.

#### OAuth Stage 4: sanitized RPC and New Session UX

- [x] Add sanitized `status`, `login`, `complete`, `cancel`, and `logout`
  daemon RPCs with bounded server IDs and transaction IDs. Responses expose
  only stable states and bounded operator-safe categories; never tokens,
  client secrets, verifier/state, codes, raw response bodies, full callback
  URLs, or server-provided instructions. `login` may return the generated
  authorization URL exactly once to the requesting trusted client; ordinary
  status never does.
- [x] Make completion/cancellation transaction-owned, expiring, and
  idempotently fail closed. Logout deletes daemon credentials and cancels
  outstanding login state, but does not rewrite existing session manifests.
- [x] Add New Session OAuth status and login controls before MCP tool
  selection. Sanitized status is independent from inventory, so
  login-required/pending/unsupported servers remain visible without tools.
  Successful login refreshes inventory but never silently selects tools or
  changes an existing frozen session.
- [x] Add public RPC lifecycle coverage for generic static-client login,
  pending polling, manual full-callback completion, ready inventory, logout,
  sanitized errors, and manifest invariance. Existing `agent-mcp` integration
  coverage retains DCR, automatic loopback completion, PKCE/state/resource/
  scope validation, cancellation/expiry, refresh/restart/logout, and
  authenticated transport without exposing credentials.

#### Cross-stage credential and session invariants

1. OAuth config may affect semantic route identity. Credentials, DCR output,
   and ephemeral authorization state never do.
2. MCP session manifests remain credential-free. They contain only the frozen
   selected declarations and semantic route fingerprints already described
   below; no OAuth config object, endpoint, scope, client information, token,
   or credential-store key is added.
3. PI.md and provider/model context remain credential- and OAuth-policy-free.
   They continue to contain only selected server IDs and exposed tool names.
4. Full/read-only subagents inherit the exact parent MCP manifest, not a copy
   of credentials. Calls resolve through the daemon-owned route and credential
   repository under the same no-replay and exact-contract checks.
5. Discovery and login are bounded control-plane operations. They cannot
   mutate a frozen manifest, replay an MCP operation, or inject server-provided
   text into model context.

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
   newer route or automatically replay a possibly side-effecting call. This
   also disables `rmcp` transparent stale-session reinitialization because it
   replays the in-flight request.
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
Refresh attempts are single-flight per route. A waiter consumes the completed
attempt generation instead of immediately starting another attempt; unrelated
healthy route selection does not wait for a coherent unavailable route.
Streamable HTTP may reconnect its auxiliary server-event SSE stream, but never
retries a POSTed tool call. A stale `Mcp-Session-Id` fails the current operation
and lets the existing manager reconnect only for a later operation.

Existing calls continue to return:

- `mcp_server_unavailable`;
- `mcp_tool_contract_changed`;
- `mcp_tool_revoked`;
- bounded timeout/protocol errors.

No result is automatically replayed.

## Explicit non-goals

- Agent-facing MCP search, discovery, enable/disable, or generic call brokers.
- Built-in, provider-specific, or curated remote server catalogs.
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
- 2026-07-10: Generic remote stage 1 adds only operator-configured Streamable
  HTTP routes and optional tagged `bearer_env` authentication as an OAuth
  prerequisite. Remote initialization `instructions` are deliberately ignored;
  resources and prompts remain out of scope. Provider-neutral OAuth is the next
  active stage and no OAuth checklist item is implemented here.
- 2026-07-11: The transient custom Stage 1 parser/discovery stack and bespoke
  registration/scope/trust policy were retired before landing. OAuth now uses
  Codex as its strict complexity ceiling: a minimal config, pinned rmcp
  discovery/DCR/PKCE/token behavior, and Codex redirect semantics. pi-relay
  adds only its daemon-specific owner task, internal manual callback path, and
  finalization ordering. Credentials/refresh, authenticated transport, public
  RPC, status/UI, and cross-platform validation remain unchecked.
