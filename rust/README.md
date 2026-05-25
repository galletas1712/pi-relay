# Rust Agent Stack

Personal-use Rust rewrite of the core pi-style agent runtime. The design keeps
the good local semantics around resume, switch, and compaction while
removing the hierarchical subagent machinery from the TypeScript fork.

See [`docs/architecture.md`](docs/architecture.md) for the detailed design.
See [`docs/websocket-rpc.md`](docs/websocket-rpc.md) for the implemented
Postgres-first websocket RPC contract and manual exercise plan.
See [`docs/design-decisions.md`](docs/design-decisions.md) for the visible UI
choices and invisible runtime/storage decisions.

## Crate Layout

| Crate | What it owns |
| --- | --- |
| `agent-vocab` | Shared serializable ids, message blocks, images, assistant items, tool calls/results, and transcript items. |
| `agent-core` | Pure deterministic FSM for one agent turn loop. No I/O. |
| `agent-session` | Durable transcript forest, model context materialization, resume, compaction, and storage snapshots. |
| `agent-store` | Postgres-only session/event/action/input persistence for the daemon. |
| `agent-provider` | `ModelProvider` plus OpenAI and Anthropic adapters. |
| `agent-tools` | `AgentTool`, `ToolRegistry`, and builtin `read`/`write`/`edit`/`bash` tools. |
| `agent-daemon` | `pi-agentd` websocket RPC server with runtime/provider/tool dispatch. |

## Running

```sh
cargo check --manifest-path rust/Cargo.toml --all
cargo test --manifest-path rust/Cargo.toml -p agent-core
cargo fmt --manifest-path rust/Cargo.toml --all --check
```

Full workspace test linking currently depends on the local macOS toolchain
finding the Apple SDK and `libiconv`. On this machine the passing full command
is:

```sh
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/clang' \
cargo test --manifest-path rust/Cargo.toml --all
```

## Websocket Daemon

Start Postgres, for example with OrbStack/Docker:

```sh
DOCKER_HOST=unix:///Users/schwinns/.orbstack/run/docker.sock \
docker run -d --name pi-relay-pg \
  -e POSTGRES_PASSWORD=postgres \
  -e POSTGRES_USER=postgres \
  -e POSTGRES_DB=pi_relay \
  -p 55432:5432 postgres:16-alpine
```

Run the daemon:

```sh
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk \
RUSTFLAGS='-C linker=/Library/Developer/CommandLineTools/usr/bin/clang' \
cargo run --manifest-path rust/Cargo.toml -p agent-daemon -- \
  --database-url postgres://postgres:postgres@127.0.0.1:55432/pi_relay \
  --bind 127.0.0.1:8787
```

The websocket endpoint is `ws://127.0.0.1:8787`.

Provider credential loading:

- `provider.kind = "codex"` uses `CODEX_ACCESS_TOKEN` or
  `~/.codex/auth.json`, including `tokens.account_id` when present.
- `provider.kind = "openai"` uses the same ChatGPT/Codex subscription auth
  path. pi-relay does not support plain OpenAI API-key auth for OpenAI models.
- `provider.kind = "anthropic"` or `"claude"` uses `ANTHROPIC_API_KEY` or
  Claude Code's `primaryApiKey` from `~/.claude/config.json` / `~/.claude.json`.

Session provider config supports `reasoning_effort`, an optional explicit
`max_tokens` cap, and `prompt_cache: { "key": "..." }`. OpenAI accepts
`none`, `minimal`, `low`, `medium`, `high`, and `xhigh`; Claude accepts `low`,
`medium`, `high`, `xhigh`, and `max`. The daemon does not add a default OpenAI output
cap. Claude Opus 4.7 uses adaptive thinking with `output_config.effort` and a
64k default `max_tokens` value because the Messages API requires that field.
The model system prompt is rendered from the repo-level `PI.md` template. Project-specific instructions enter through `AGENTS.md` files in the session's workspace checkouts, which `PI.md` composes explicitly. The daemon does not inject date, time, or cwd unless the template asks for it.

## Web UI

```sh
npm run dev:web
```

The web UI runs at `http://127.0.0.1:8788` and connects to
`ws://127.0.0.1:8787` by default. Override the daemon URL with
`VITE_PI_AGENT_WS`.

The composer sends regular text as `input.follow_up`. The top bar exposes the
model picker and provider-specific reasoning effort picker. The model is locked
once the session has transcript history; reasoning effort can still be changed
during or between turns and applies to subsequently created provider requests.
Slash commands expose operations that do not already have dedicated controls:
`/switch`, `/compact`, `/system`, and `/export`. Active turns use the
stop button; new, rename, archive, and unarchive use sidebar controls; queued
follow-ups can be promoted to steer from the queue pane above the composer.
Crashed or interrupted terminal model turns can be retried/continued directly
from the transcript row.
