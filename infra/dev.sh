#!/usr/bin/env bash
# Run Postgres + control (Docker) plus pi-runtime (host process) and web preview.
#
# The runtime is deliberately NOT dockerized: it executes each session's tools
# in your real host environment (arbitrary local-workspace source_paths, your
# toolchain/venvs, PATH) and needs root for btrfs subvolumes. It runs here as a
# host process (sudo) dialing the control runtime listener published on
# 127.0.0.1:8786. Control + Postgres stay in Docker.
#
# Local mode  (TAILNET_HOST unset): browse http://127.0.0.1:8788/.
# Tailnet mode (TAILNET_HOST set):  pair with infra/serve.sh and browse
#   https://${TAILNET_HOST}/ — the bundle has that origin baked in.
#
# Static by design: no HMR, no daemon auto-restart. The agent-daemon edits this
# repo, so an in-flight bad edit must not tear down running services. Apply
# changes by Ctrl-C and re-running.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

WEB_PORT="${WEB_PORT:-8788}"
TAILNET_HOST="${TAILNET_HOST:-}"
PI_AGENTD_CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}/pi-relay/agentd"
if [ ! -d "$PI_AGENTD_CONFIG_HOME" ]; then
  echo "missing agentd configuration: $PI_AGENTD_CONFIG_HOME" >&2
  exit 1
fi
export PI_AGENTD_CONFIG_HOME

bun install

# Control plane + Postgres in Docker. The runtime is a host process (below);
# --remove-orphans clears a previously-dockerized runtime container.
docker compose -f infra/docker-compose.yml up -d --build --wait --remove-orphans

# Build + launch pi-runtime as a host process. Its required policy lives at
# $XDG_CONFIG_HOME/pi-relay/runtime/config.toml (or
# ~/.config/pi-relay/runtime/config.toml)
# and optional MCP policy is the sibling mcp.toml. Root is required for btrfs
# subvolume operations; HOME, PATH, and XDG_CONFIG_HOME are preserved so the
# runtime resolves the host's policy, binaries, venvs, and ~/.agents/skills.
( cd rust && cargo build --release -p agent-runtime )
RUNTIME_BIN="$REPO_ROOT/rust/target/release/pi-runtime"
RUNTIME_CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}/pi-relay/runtime"
if [ ! -f "$RUNTIME_CONFIG_HOME/config.toml" ]; then
  echo "missing runtime configuration: $RUNTIME_CONFIG_HOME/config.toml" >&2
  exit 1
fi
if [ -n "${XDG_CONFIG_HOME:-}" ]; then
  sudo -n env HOME="$HOME" PATH="$PATH" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" "$RUNTIME_BIN" &
else
  sudo -n env HOME="$HOME" PATH="$PATH" "$RUNTIME_BIN" &
fi

# ${VAR:+...} expands to empty when VAR is unset/empty, so local mode passes
# through the rpc.ts default (ws://127.0.0.1:8787) and skips allowed-host gating.
( cd packages/web && \
    VITE_PI_AGENT_WS="${TAILNET_HOST:+wss://${TAILNET_HOST}/ws}" \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run build )

( cd packages/web && \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run preview -- --port "$WEB_PORT" ) &
WEB_PID=$!

shutdown() {
  trap - EXIT INT TERM
  kill "$WEB_PID" 2>/dev/null || true
  # pi-runtime runs as root; stop it by binary path (its sudo child outlives a
  # kill of the backgrounded sudo pid).
  sudo -n pkill -f "$RUNTIME_BIN" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap shutdown EXIT INT TERM

wait "$WEB_PID"
