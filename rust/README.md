# Rust Agent Stack

Personal-use Rust agent runtime and control plane. It provides durable
PostgreSQL-backed sessions, resume/switch/compaction, host-side workspace
tools and MCP routes, bounded delegation, and the React web client.

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
| `agent-runtime` | `pi-runtime` host worker for managed workspaces, local tools, runtime skills, and MCP. | — |
| `agent-prompt` | Renders the repo-level `PI.md` system prompt. | [docs/modules/agent-prompt.md](docs/modules/agent-prompt.md) |

## Build And Test

The standard workspace commands use Cargo. PostgreSQL-backed tests are
explicitly ignored unless `PI_RELAY_TEST_DATABASE_URL` is supplied, so an
ordinary test run reports them in its ignored count rather than presenting
them as successful database coverage. If ignored tests are explicitly
included without the variable, their bodies print
`SKIPPED PostgreSQL test` and return without database coverage.

```sh
cargo check --manifest-path rust/Cargo.toml --all
cargo test  --manifest-path rust/Cargo.toml --all -- --nocapture
cargo fmt   --manifest-path rust/Cargo.toml --all --check
```

The `--nocapture` flag is intentional: it makes each missing-database
`SKIPPED PostgreSQL test` report visible if an ignored test is selected
directly. Use `--include-ignored` only with a configured PostgreSQL URL.

For the complete test suite, start a PostgreSQL 16 instance with a role that
can create and drop databases, then run:

```sh
docker compose -f infra/docker-compose.yml up -d postgres
PI_RELAY_TEST_DATABASE_URL=postgres://postgres:postgres@127.0.0.1:55432/postgres \
  cargo test --manifest-path rust/Cargo.toml --workspace -- --include-ignored --nocapture
```

The tests create uniquely named databases and remove them afterward. Do not
point `PI_RELAY_TEST_DATABASE_URL` at a production database.

The frontend's checked-in `package-lock.json` is the canonical reproducible
install for npm-based development:

```sh
npm ci
npm test --workspaces --if-present
npm run build --workspace @pi-relay/web
```

The repository uses npm and `package-lock.json` for the local Docker/host
stack. The obsolete Bun lockfile is not part of the supported workflow.

## Prerequisites and runtime requirements

- Rust stable with Cargo and `rustfmt`, Node.js 20+, and npm.
- Docker Engine with Compose v2 and PostgreSQL 16. The integration tests need
  a PostgreSQL role allowed to `CREATE DATABASE` and `DROP DATABASE`.
- A Linux host with `btrfs-progs`, `git`, `rsync`, and passwordless `sudo -n`
  for `pi-runtime` when using `infra/dev.sh`; the runtime is intentionally a
  host process and is not dockerized.
- Provider credentials at model-call time. OpenAI/Codex accepts
  `CODEX_ACCESS_TOKEN` or `$HOME/.codex/auth.json`; Anthropic accepts
  `ANTHROPIC_API_KEY` or Claude Code's
  `$HOME/.claude/config.json`/`$HOME/.claude.json`.

The compose control service mounts `$HOME/.codex`,
`$HOME/.claude/config.json`, and `$HOME/.claude.json` read-only into the
container. Ensure the credential paths used by the selected provider exist
and are readable by Docker, or run the binaries directly on the host instead
of using the compose mounts. MCP configuration and OAuth credential state are
host-runtime files; compose deliberately does not mount them into the control
container.

## Run The Services

Start Postgres (any Postgres 16 works):

```sh
docker run -d --name pi-relay-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_USER=postgres \
  -e POSTGRES_DB=pi_relay \
  -p 55432:5432 postgres:16-alpine
```

Create the required daemon policy at
`$XDG_CONFIG_HOME/pi-relay/agentd/config.toml` (or
`$HOME/.config/pi-relay/agentd/config.toml` when `XDG_CONFIG_HOME` is unset):

```sh
CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}/pi-relay/agentd"
mkdir -p "$CONFIG_HOME"
cat >"$CONFIG_HOME/config.toml" <<'EOF'
database_url = "postgres://postgres:postgres@127.0.0.1:55432/pi_relay"
bind = "127.0.0.1:8787"
EOF

cargo run --manifest-path rust/Cargo.toml -p agent-daemon
```

`pi-agentd` accepts no configuration arguments. The websocket endpoint is
`ws://127.0.0.1:8787` unless `bind` changes it in `config.toml`.

Each runtime host has independent policy at
`$XDG_CONFIG_HOME/pi-relay/runtime/config.toml` (or
`$HOME/.config/pi-relay/runtime/config.toml`):

```toml
runtime_id = "runtime-local"
name = "Local runtime"
control_addr = "127.0.0.1:8786"
workspace_root = "/home/me/.local/state/pi-runtime"
```

`pi-runtime` accepts no configuration arguments. It reads optional MCP policy
from the sibling `pi-relay/runtime/mcp.toml`; when that file is absent, MCP is
disabled. `workspace_root` is explicit and may point at existing managed
workspace state.

```sh
cargo run --manifest-path rust/Cargo.toml -p agent-runtime
```

The service boundary is explicit:

- `pi-agentd` owns database/frontend/runtime-listener settings, provider policy,
  prompt construction, and delegation execution.
- `pi-runtime` owns its identity, control address, workspace root, MCP routes,
  MCP OAuth state, instructions, and skill/role files it publishes to the
  control plane.

### Database initialization and migrations

`PostgresAgentStore::migrate()` runs the embedded, idempotent current schema at
every daemon startup. On a fresh database this creates the complete schema.
On an existing current database it reapplies the current `create ... if not
exists`/additive schema statements safely. This is not a general historical
data-migration runner: startup does not rewrite old session or transcript
rows. Back up databases from older product/schema revisions and follow any
release-specific migration instructions before using them; do not assume a
fresh startup upgrades old data layouts.

For the repository's local stack, `infra/dev.sh` mounts
`infra/config/control.toml` into the control container's `pi-relay/agentd`
configuration root and launches the host runtime with the caller's
`pi-relay/runtime` XDG configuration. Workflows and subagent roles live under
the runtime catalog (`$XDG_CONFIG_HOME/pi-relay/runtime/{skills,subagent-roles}`);
home and project overlays live under `$HOME/.agents/{skills,projects}/`.
Ordinary home/workspace skills are not subagent roles; put roles under
`runtime/subagent-roles/<global-name>/` and reference them by that unprefixed
global name.

### Daemon and runtime configuration

General daemon configuration is read from
`$XDG_CONFIG_HOME/pi-relay/agentd/config.toml`; when `XDG_CONFIG_HOME` is unset
or empty, that is `$HOME/.config/pi-relay/agentd/config.toml`. In particular, a
nonempty `XDG_CONFIG_HOME` is used directly and never gains an extra `.config`
component. `XDG_CONFIG_HOME` and `HOME` must be absolute paths; relative values
and parent-directory components are rejected rather than being resolved
against the daemon's working directory. The file is required, and its required
root `database_url` must not be blank. The optional root `bind` defaults to
`127.0.0.1:8787`. A legacy `config.json` is not read as a daemon-policy
fallback. Invalid TOML, unknown fields (including provider fields), blank
database URLs, blank binds, and blank model names fail daemon startup rather
than being deferred to a session or subagent.

```toml
database_url = "postgres://postgres:postgres@127.0.0.1:55432/pi_relay"
bind = "127.0.0.1:8787" # optional; this is the default
runtime_bind = "127.0.0.1:8786" # optional; this is the default

[default_parent_model]
kind = "openai"
model = "gpt-5.6-sol"
reasoning_effort = "high"
max_tokens = 32768
prompt_cache = { key = "my-parent-cache" }
```

The root schema is exactly `database_url`, optional `bind`, optional
`runtime_bind`, and optional `default_parent_model`. The provider object keeps
the normal `kind`, `model`, `reasoning_effort`, optional `max_tokens`, and
optional `prompt_cache` fields. If `default_parent_model` is omitted, the
built-in parent policy is OpenAI `gpt-5.6-sol` with `high` reasoning. A new
parent session uses an explicit `session.start.provider`, otherwise
`default_parent_model`, otherwise that built-in policy.
Existing or replayed sessions retain their persisted provider and are never
retargeted by changed defaults.

Runtime-owned instructions and catalogs use the runtime host's XDG and home
directories:

```text
${XDG_CONFIG_HOME:-$HOME/.config}/pi-relay/runtime/
├── AGENTS.md
├── skills/<workflow>/SKILL.md
└── subagent-roles/<role>/SKILL.md

$HOME/.agents/
├── skills/<skill>/SKILL.md
└── projects/<workspace>/skills/<skill>/SKILL.md

<workspace>/.agents/skills/<skill>/SKILL.md
```

The top-level `AGENTS.md` applies on that runtime before selected workspaces'
own `AGENTS.md` files. Home skills are reusable global capabilities. Runtime
`skills/` contains workflow skills; workflows remain ordinary loadable skills.
Personal project skills override same-named repository project skills.

A role-local provider policy and global skill preloads use frontmatter:

```yaml
---
name: reviewer
description: Review artifacts and handoffs against the objective.
kind: claude
model: claude-opus-4-8
reasoning_effort: high
skills:
  - swe
---
```

Each immediate role directory must contain a valid `SKILL.md` whose frontmatter
name exactly matches the directory name. `kind` and `model` must appear
together; `reasoning_effort` and `max_tokens` are optional. `skills` may name
only global packages from `$HOME/.agents/skills`; project and workflow packages
cannot be role preloads. A child uses an explicit spawn override, then its role
provider, then the parent provider. An unavailable role provider retains the
existing stable-provider fallback.

Roles stay hidden from ordinary `LoadSkill` discovery. `LoadSkill` returns only
the absolute runtime-host path to the selected `SKILL.md`; the agent reads that
file and resolves relative links from its enclosing directory.

Optional MCP configuration is read only from an already-existing
`$XDG_CONFIG_HOME/pi-relay/runtime/mcp.toml` on each runtime host; when it is
absent, MCP is disabled on that runtime. The runtime never creates, edits,
merges, renames, or chmods that file. It is parsed as strict TOML. Its typed
TOML shape and trust model are documented in
[`docs/plans/mcp-client.md`](docs/plans/mcp-client.md).
When that configuration contains OAuth routes, the runtime stores their
credentials in `mcp-oauth-credentials.json` directly beneath that runtime's
configured `workspace_root`. This OAuth credential state remains JSON and is
distinct from TOML-only XDG configuration. It is a plaintext file protected by
restrictive OS directory/file permissions. A corrupt, empty, oversized, or
unreadable file is preserved and makes only OAuth credential operations
unavailable; unrelated stdio/bearer routes and runtime startup continue. The
backend intentionally has no repair, cross-process locking, keyring, or
credential-database fallback.
For example:

```toml
# $XDG_CONFIG_HOME/pi-relay/runtime/mcp.toml
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
and DCR login is persisted after callback cleanup, restored across runtime
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
Automatic callback works when the browser can reach loopback on the runtime
host. For a remote runtime, complete authorization in the browser, copy its
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
data migrations automatically. See
[Database initialization and migrations](#database-initialization-and-migrations)
for the fresh-versus-existing database contract.

## Run The Web UI

For day-to-day UI edits with HMR:

```sh
npm run dev:web
```

The full local stack (`infra/dev.sh`) serves the built UI from the Compose
`web` service at `http://127.0.0.1:8788` (websocket still `ws://127.0.0.1:8787`).
Rebuild only the frontend without restarting host `pi-runtime`:

```sh
docker compose -f infra/docker-compose.yml up -d --build web
```

The client uses `ws://127.0.0.1:8787` when opened on loopback and same-origin
`/ws` when served through `infra/serve.sh` (Tailscale → nginx → agentd). The
same build therefore works through local TCP forwarding and Tailnet access. See
[`../packages/web/docs/web-ui.md`](../packages/web/docs/web-ui.md) for the
client design.


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
