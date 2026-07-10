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

Run `pi-agentd` (the websocket endpoint is `ws://127.0.0.1:8787`):

```sh
cargo run --manifest-path rust/Cargo.toml -p agent-daemon -- \
  --database-url postgres://postgres:postgres@127.0.0.1:55432/pi_relay \
  --bind 127.0.0.1:8787
```

`--database-url`/`DATABASE_URL` is required; `--bind`/`PI_AGENTD_BIND` defaults
to `127.0.0.1:8787`. Optional MCP configuration is selected with
`--mcp-config PATH` or `PI_AGENTD_MCP_CONFIG`; its typed JSON shape and trust
model are documented in [`docs/plans/mcp-client.md`](docs/plans/mcp-client.md).
For example:

```json
{
  "servers": {
    "workspace": {
      "transport": {
        "type": "stdio",
        "command": "npx",
        "args": ["-y", "@example/workspace-mcp"],
        "cwd": "/trusted/workspace",
        "inherit_env": ["EXAMPLE_TOKEN"]
      },
      "enabled_tools": ["read_file", "search"],
      "call_timeout_ms": 30000,
      "parallel_calls": 1
    }
  }
}
```

Generic remote servers use Streamable HTTP. HTTPS is required except for
loopback development/test endpoints. An optional bearer token is read at
connection time from the configured environment variable; only its name is
configuration identity:

```json
{
  "servers": {
    "remote": {
      "transport": {
        "type": "streamable_http",
        "url": "https://mcp.example.com/mcp",
        "auth": {
          "type": "bearer_env",
          "env": "EXAMPLE_MCP_TOKEN"
        }
      },
      "enabled_tools": ["search"]
    }
  }
}
```

The tagged `transport` object is preferred. Earlier flat stdio fields remain
accepted with the same route fingerprint so existing frozen manifests and
configuration files remain compatible.
Bearer environment authentication is only a transport prerequisite, not the
interactive authentication objective. Browser OAuth login is not implemented
in this stage. The next active generic stage is protected-resource and
authorization-server discovery, DCR by default or static client IDs, PKCE,
resource indicators, loopback/browser callbacks, secure credential
persistence/refresh, explicit scope allowlists, and login/status/logout in New
Session before tool selection. There is no built-in or provider-specific server
catalog.

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
