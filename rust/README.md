# Rust Agent Stack

Personal-use Rust rewrite of the core pi-style agent runtime. It keeps the good
local semantics around resume, switch, and compaction while removing the
hierarchical subagent machinery from the TypeScript fork.

## Documentation

- [`docs/architecture.md`](docs/architecture.md) - overview and crate map.
- [`docs/design-decisions.md`](docs/design-decisions.md) - the visible and
  invisible engineering choices and why they were made.
- [`docs/websocket-rpc.md`](docs/websocket-rpc.md) - the frontend websocket RPC
  contract and manual exercise plan.
- [`docs/modules/`](docs/modules) - one reference per crate (linked below).
- [`docs/plans/`](docs/plans) - in-flight future work only.
- [`../packages/web/docs/web-ui.md`](../packages/web/docs/web-ui.md) - the React
  web client.

## Crate Layout

| Crate | What it owns | Doc |
| --- | --- | --- |
| `agent-vocab` | Shared serializable ids, message blocks, images, assistant items, tool calls/results, transcript items, provider config. | [docs/modules/agent-vocab.md](docs/modules/agent-vocab.md) |
| `agent-core` | Pure deterministic FSM for one agent turn loop. No I/O. | [docs/modules/agent-core.md](docs/modules/agent-core.md) |
| `agent-session` | Durable transcript forest, model-context materialization, resume, switch, compaction. | [docs/modules/agent-session.md](docs/modules/agent-session.md) |
| `agent-store` | Postgres-only session/transcript/queue/action/event persistence and recovery. | [docs/modules/agent-store.md](docs/modules/agent-store.md) |
| `agent-provider` | `ModelProvider` plus OpenAI/Codex and Anthropic adapters. | [docs/modules/agent-provider.md](docs/modules/agent-provider.md) |
| `agent-tools` | `AgentTool`, `ToolRegistry`, and builtins: `apply_patch` / `str_replace_based_edit_tool`, `Bash`, `web_search`, `web_fetch`, `LoadSkill`, and delegation tools (`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`, `steer_subagent`, `interrupt_subagent`). | [docs/modules/agent-tools.md](docs/modules/agent-tools.md) |
| `agent-mcp` | Session-scoped stdio and generic Streamable HTTP MCP clients, New Session inventory/selection, deterministic frozen manifests, and result normalization. | [docs/plans/mcp-client.md](docs/plans/mcp-client.md) |
| `agent-daemon` | `pi-agentd` websocket RPC server with runtime/provider/tool dispatch. | [docs/modules/agent-daemon.md](docs/modules/agent-daemon.md) |
| `agent-prompt` | Renders the repo-level `PI.md` system prompt. | [docs/modules/agent-prompt.md](docs/modules/agent-prompt.md) |

## Build And Test

```sh
cargo check --manifest-path rust/Cargo.toml --all
cargo test  --manifest-path rust/Cargo.toml --all
cargo fmt   --manifest-path rust/Cargo.toml --all --check
```

## Run The Daemon

Start Postgres (any Postgres 16 works):

```sh
docker run -d --name pi-relay-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_USER=postgres \
  -e POSTGRES_DB=pi_relay \
  -p 55432:5432 postgres:16-alpine
```

Create the required daemon policy at
`$XDG_CONFIG_HOME/pi-relay/config.toml` (or
`$HOME/.config/pi-relay/config.toml` when `XDG_CONFIG_HOME` is unset):

```sh
CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}/pi-relay"
mkdir -p "$CONFIG_HOME"
cat >"$CONFIG_HOME/config.toml" <<'EOF'
database_url = "postgres://postgres:postgres@127.0.0.1:55432/pi_relay"
bind = "127.0.0.1:8787"
EOF

cargo run --manifest-path rust/Cargo.toml -p agent-daemon
```

`pi-agentd` accepts no configuration arguments. The websocket endpoint is
`ws://127.0.0.1:8787` unless `bind` changes it in `config.toml`.

For the repository’s local stack, `infra/dev.sh` uses the caller's normal XDG
config and state directories. It therefore starts the daemon with the same
`config.toml`, optional `mcp.toml`, catalog overlay, managed workspaces, and
OAuth state used outside the script. Configure `database_url` to the compose
database shown above when using this local stack.

### Daemon configuration and packaged catalogs

General daemon configuration is read from
`$XDG_CONFIG_HOME/pi-relay/config.toml`; when `XDG_CONFIG_HOME` is unset or
empty, that is `$HOME/.config/pi-relay/config.toml`. In particular, a nonempty
`XDG_CONFIG_HOME` is used directly and never gains an extra `.config`
component. `XDG_CONFIG_HOME` and `HOME` must be absolute paths; relative
values and parent-directory components are rejected rather than being resolved
against the daemon's working directory. The file is required, and its required
root `database_url` must not be blank. The optional root `bind` defaults to
`127.0.0.1:8787`. A legacy `config.json` is not read as a daemon-policy
fallback. Invalid TOML, unknown fields (including provider fields), blank
database URLs, blank binds, and blank model names fail daemon startup rather
than being deferred to a session or subagent.

This is a breaking TOML-only migration for user-authored XDG daemon
configuration: legacy `$XDG_CONFIG_HOME/pi-relay/config.json` and
`mcp.json` are ignored, never converted, and never read.

```toml
database_url = "postgres://postgres:postgres@127.0.0.1:55432/pi_relay"
bind = "127.0.0.1:8787" # optional; this is the default

[default_parent_model]
kind = "openai"
model = "gpt-5.6-sol"
reasoning_effort = "high"
max_tokens = 32768
prompt_cache = { key = "my-parent-cache" }

[subagent_models.reviewer]
kind = "claude"
model = "claude-opus-4-8"
reasoning_effort = "high"

[subagent_models."repo/reviewer"]
kind = "openai"
model = "gpt-5.6-sol"
reasoning_effort = "high"
```

The root schema is exactly `database_url`, optional `bind`, optional
`default_parent_model`, and optional `subagent_models`. Every provider object
keeps the normal `kind`, `model`, `reasoning_effort`, optional `max_tokens`, and
optional `prompt_cache` fields. If `default_parent_model` is omitted, the
built-in parent policy is OpenAI `gpt-5.6-sol` with `high` reasoning. A new
parent session uses an explicit `session.start.provider`, otherwise
`default_parent_model`, otherwise that built-in policy. A child uses its
explicit override, then the matching resolved exposed role name in
`subagent_models` (for example
`reviewer` or `repo/reviewer`), then its persisted parent provider. Existing
or replayed sessions retain their persisted provider and are never retargeted
by changed defaults.

On first startup, the daemon bootstrap-copies its packaged
`subagent-roles/*/SKILL.md` and `workflows/*/SKILL.md` into this configuration
directory. It creates only absent files, never changes permissions or contents
of existing files, and writes a completion marker so deliberately deleted
catalog entries stay deleted on future starts. Workspace/home explicit skills
and roles retain their current precedence. For parent workflow skills and
subagent-role catalog entries, configured catalog files override same-named
packaged files, while missing configured entries fall back to the package.
Roles remain out of ordinary `LoadSkill` discovery; subagents cannot load
workflow skills. Bootstrap opens each owned configuration component through
no-follow directory handles, rejecting symlinked roots, catalogs, entries, and
leaves rather than writing outside the configured configuration home. Each new
leaf is fully written and synced to a unique hidden staging leaf in the
already-open configuration root, then capability-published with a no-replace
hard link. The hidden staging leaves are intentionally retained: after a
failure their names cannot safely be deleted without risking a concurrently
created user file.

Optional MCP configuration is read only from an already-existing
`<config-root>/mcp.toml`; when it is absent, MCP is disabled. The daemon never
creates, edits, merges, renames, or chmods that file; it is intentionally
separate from `config.toml`. It is parsed as strict TOML. Its typed TOML shape
and trust model are documented in [`docs/plans/mcp-client.md`](docs/plans/mcp-client.md).
Legacy `mcp.json` is ignored and never modified.
When that configuration contains OAuth routes, the daemon stores their
credentials in `mcp-oauth-credentials.json` directly beneath its existing
`$XDG_STATE_HOME/pi-relay` state root (or `~/.local/state/pi-relay` when
`XDG_STATE_HOME` is unset). This OAuth credential state remains JSON and is
distinct from TOML-only XDG configuration. It is a plaintext file protected by
restrictive OS directory/file permissions. A corrupt, empty, oversized, or
unreadable file is preserved and makes only OAuth credential operations
unavailable; unrelated stdio/bearer routes and daemon startup continue. The
backend intentionally has no repair/migration, cross-process locking, keyring,
or credential-database fallback.
For example:

```toml
# $XDG_CONFIG_HOME/pi-relay/mcp.toml
[servers.workspace]
enabled_tools = ["read_file", "search"]
call_timeout_ms = 30000
parallel_calls = 1

[servers.workspace.transport]
type = "stdio"
command = "npx"
args = ["-y", "@example/workspace-mcp"]
cwd = "/trusted/workspace"
inherit_env = ["EXAMPLE_TOKEN"]
```

Generic remote servers use Streamable HTTP. HTTPS is required except for
loopback development/test endpoints. An optional bearer token is read at
connection time from the configured environment variable; only its name is
configuration identity. Generic OAuth delegates discovery, public-client
dynamic registration, S256 PKCE, state validation, and token exchange to the
pinned rmcp 1.8 state machine:

```toml
[servers.remote]
enabled_tools = ["search"]

[servers.remote.transport]
type = "streamable_http"
url = "https://mcp.example.com/mcp"

[servers.remote.transport.auth]
type = "bearer_env"
env = "EXAMPLE_MCP_TOKEN"
```

```toml
[servers.oauth_remote]
enabled_tools = ["search"]

[servers.oauth_remote.transport]
type = "streamable_http"
url = "https://mcp.example.com/mcp"

[servers.oauth_remote.transport.auth]
type = "oauth"
client_id = "operator-configured-public-client"
scopes = ["read", "search"]
resource = "https://api.example.com/audience"
callback_port = 8765
callback_timeout_ms = 300000
```

Omit `client_id` to let rmcp perform Dynamic Client Registration. `scopes` and
the RFC 8707 `resource` authorization parameter are optional. A fixed
`callback_port` may be configured; otherwise pi-relay binds an ephemeral port
on `127.0.0.1`. `callback_timeout_ms` defaults to five minutes. Both callback
settings are operational and do not change route identity. Streamable HTTP
keeps its existing URL rule for OAuth: HTTPS is required remotely, while HTTP
is permitted for loopback; MCP resource URLs may include a query. The redirect
path is `/callback/<stable-id>`, where the 12-character ID is derived from the
configured MCP URL using Codex-compatible URL canonicalization.

The transient, unlanded Stage 1 keys (`registration`, `client_secret_env`,
`allowed_scopes`, `initial_scopes`, issuer pins, and trusted origins) are not
accepted. Replace them with optional `client_id`, `scopes`, and `resource`.

The tagged `transport` object is preferred. Earlier flat stdio TOML fields
remain accepted with the same route fingerprint so existing frozen manifests
and configuration remain compatible.
Bearer environment authentication remains supported. Successful static-client
and DCR login is persisted after callback cleanup, restored across daemon
restart, and refreshed through rmcp with a 30-second skew. Status is
observational:
expired credentials with a refresh token remain OAuth-ready without consuming
that token; refresh occurs only on route acquisition/reconnect. A transient
refresh or atomic-save failure preserves the old durable record for a later
attempt. The current access token is injected only into pi-relay's
bounded/no-replay Streamable HTTP client, whose
POST, common GET/SSE, and DELETE behavior and secret scrubbing remain shared
with `bearer_env`. An MCP 401 closes the route without replaying the current
operation; a later inventory or call may refresh and reconnect. Internal
sanitized status and local credential-only logout are implemented. The bounded
`mcp.status`, `mcp.login`, `mcp.complete`, `mcp.cancel`, and
`mcp.logout` websocket RPCs expose only sanitized status/categories plus the
one authorization URL returned by an explicit login action. The New Session
MCP picker shows OAuth state separately from inventory, disables tools until a
route is ready, and opens an accessible login dialog with an explicit
authorization link, URL-copy fallback, and full-callback-URL completion.
Automatic callback works when the browser can reach loopback on the daemon
host. For a remote daemon, complete authorization in the browser, copy its
entire failed/unreachable loopback callback URL from the address bar, and paste
that URL into the dialog. Login IDs and authorization URLs remain in memory
only and are never written to browser storage. Logout removes only local
credentials; it does not remotely revoke access or rewrite existing frozen
session manifests. There is no built-in or provider-specific server catalog.

All configured servers appear as New Session inventory sources. Operators use
`enabled_tools` (or explicit `allow_all_tools: true`) as the hard allowlist;
there are no required/optional or subagent-exposure settings. Users then choose
the exact session subset in the New Session UI. Existing sessions and all their
full/read-only children keep that frozen subset.

The daemon creates its schema on startup but does not run old-session
migrations automatically.

## Run The Web UI

```sh
npm run dev:web
```

The web UI serves at `http://127.0.0.1:8788` and connects to
`ws://127.0.0.1:8787` by default; override with `VITE_PI_AGENT_WS`. See
[`../packages/web/docs/web-ui.md`](../packages/web/docs/web-ui.md) for the client
design.

## Provider Credentials

Credentials are loaded at model-call time, not stored on the session:

- `provider.kind = "openai"` uses the ChatGPT/Codex subscription transport
  (`CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`, including `tokens.account_id`).
  pi-relay does not support plain OpenAI API-key auth for OpenAI models.
- `provider.kind = "claude"` uses `ANTHROPIC_API_KEY` or Claude
  Code's `primaryApiKey` from `~/.claude/config.json` / `~/.claude.json`.

Provider/model tuning (`reasoning_effort`, optional `max_tokens`,
`prompt_cache.key`) and the exact accepted values are documented in
[`docs/websocket-rpc.md`](docs/websocket-rpc.md) and
[`docs/modules/agent-provider.md`](docs/modules/agent-provider.md).
