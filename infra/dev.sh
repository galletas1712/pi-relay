#!/usr/bin/env bash
# Run Postgres + control + web (Docker) plus pi-runtime (host process).
#
# The runtime is deliberately NOT dockerized: it executes each session's tools
# in your real host environment (arbitrary local-workspace source_paths, your
# toolchain/venvs, PATH). It runs as your login user (not root) and needs the
# workspace btrfs mount(s) to allow unprivileged deletion of owned subvolumes
# (`user_subvol_rm_allowed`). It dials the control runtime listener published on
# 127.0.0.1:8786. Control, Postgres, and the static web UI stay in Docker.
#
# Local access: browse http://127.0.0.1:8788/.
# Tailnet access: pair with infra/serve.sh (Tailscale → web; nginx proxies /ws).
# The browser derives the websocket endpoint from the page location.
#
# Static by design: no HMR, no daemon auto-restart. The agent-daemon edits this
# repo, so an in-flight bad edit must not tear down running services.
#
# Refresh / lifecycle:
#   Full stack (this script): rebuilds compose services (including web) and
#     restarts host pi-runtime. Ctrl-C stops only the host runtime; Docker
#     services keep running (restart: unless-stopped).
#   Frontend only (sessions stay up):
#     docker compose -f infra/docker-compose.yml up -d --build web
#   Stop Docker services:
#     docker compose -f infra/docker-compose.yml down
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

WEB_PORT="${WEB_PORT:-8788}"
export WEB_PORT

PI_AGENTD_CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}/pi-relay/agentd"
if [ ! -d "$PI_AGENTD_CONFIG_HOME" ]; then
  echo "missing agentd configuration: $PI_AGENTD_CONFIG_HOME" >&2
  exit 1
fi
export PI_AGENTD_CONFIG_HOME

# Build pi-runtime before replacing either side of the control protocol. Its
# required policy lives at
# $XDG_CONFIG_HOME/pi-relay/runtime/config.toml (or
# ~/.config/pi-relay/runtime/config.toml)
# and optional MCP policy is the sibling mcp.toml. HOME, PATH, and
# XDG_CONFIG_HOME are used so the runtime resolves the host's policy,
# instructions, role/workflow catalogs, binaries, venvs, and ~/.agents
# global/project skills.
( cd rust && cargo build --release -p agent-runtime )
RUNTIME_BIN="$REPO_ROOT/rust/target/release/pi-runtime"
RUNTIME_CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}/pi-relay/runtime"
if [ ! -f "$RUNTIME_CONFIG_HOME/config.toml" ]; then
  echo "missing runtime configuration: $RUNTIME_CONFIG_HOME/config.toml" >&2
  exit 1
fi

stop_runtime() {
  # Prefer the PID we launched; also clear any leftover user-owned pi-runtime.
  if [ -n "${RUNTIME_PID:-}" ] && kill -0 "$RUNTIME_PID" 2>/dev/null; then
    kill "$RUNTIME_PID" 2>/dev/null || true
  fi
  pkill -x -u "$USER" pi-runtime 2>/dev/null || true
  for _ in {1..50}; do
    if ! pgrep -x -u "$USER" pi-runtime >/dev/null; then
      return 0
    fi
    sleep 0.1
  done
  if pgrep -x -u "$USER" pi-runtime >/dev/null; then
    echo "old pi-runtime did not stop" >&2
    return 1
  fi
}

# Stop the old runtime before deploying the new control plane.
stop_runtime

# Deploy the matching control plane only after the runtime build succeeds.
docker compose -f infra/docker-compose.yml up -d --build --wait --remove-orphans

if [ -n "${XDG_CONFIG_HOME:-}" ]; then
  env HOME="$HOME" PATH="$PATH" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" "$RUNTIME_BIN" &
else
  env HOME="$HOME" PATH="$PATH" "$RUNTIME_BIN" &
fi
RUNTIME_PID=$!
sleep 0.5
if ! kill -0 "$RUNTIME_PID" 2>/dev/null \
  || ! pgrep -x -u "$USER" pi-runtime >/dev/null; then
  stop_runtime || true
  wait "$RUNTIME_PID" 2>/dev/null || true
  echo "pi-runtime exited during startup" >&2
  exit 1
fi

shutdown() {
  trap - EXIT INT TERM
  # Leave Docker services running so Ctrl-C does not drop the stack.
  stop_runtime || true
  wait 2>/dev/null || true
  exit 0
}
trap shutdown EXIT INT TERM

echo "pi-relay stack up:"
echo "  web UI:  http://127.0.0.1:${WEB_PORT}/"
echo "  agentd:  ws://127.0.0.1:8787"
echo "  Ctrl-C stops host pi-runtime only; Docker services keep running."
echo "  Frontend-only refresh: docker compose -f infra/docker-compose.yml up -d --build web"

# Keep the script attached while runtime runs. Docker web can be rebuilt
# independently without this process exiting.
while true; do
  sleep 3600
done
