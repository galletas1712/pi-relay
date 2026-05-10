# Rust Agent Stack

Personal-use Rust rewrite of the core pi-style agent runtime. The design keeps
the good local semantics around resume, rewind, fork, and compaction while
removing the hierarchical subagent machinery from the TypeScript fork.

See [`docs/architecture.md`](docs/architecture.md) for the detailed design.

## Crate Layout

| Crate | What it owns |
| --- | --- |
| `agent-vocab` | Shared serializable ids, message blocks, images, assistant items, tool calls/results, and transcript items. |
| `agent-core` | Pure deterministic FSM for one agent turn loop. No I/O. |
| `agent-session` | Durable transcript forest, model context materialization, resume/rewind/fork/compaction, runner, and storage snapshots. |
| `agent-store` | Backend-neutral `SessionStore`, `StoredSession`, in-memory store, and JSONL store. |
| `agent-provider` | `ModelProvider` plus OpenAI and Anthropic adapters. |
| `agent-tools` | `AgentTool`, `ToolRegistry`, and builtin `read`/`write`/`edit`/`bash` tools. |
| `pi-cli` | Minimal `pi-rs` driver for one local session. |

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

## CLI Smoke Test

```sh
ANTHROPIC_API_KEY=... cargo run --manifest-path rust/Cargo.toml -p pi-cli -- claude claude-sonnet-4-5 "hello"
OPENAI_API_KEY=... cargo run --manifest-path rust/Cargo.toml -p pi-cli -- openai gpt-4.1 "hello"
```
