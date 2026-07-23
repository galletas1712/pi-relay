#!/usr/bin/env bash
# Run Postgres + control + web (Docker) plus pi-runtime (host process).
#
# The runtime is deliberately NOT dockerized: it executes each session's tools
# in your real host environment (arbitrary local-workspace source_paths, your
# toolchain/venvs, PATH) and needs root for btrfs subvolumes. It runs here as a
# host process (sudo) dialing the control runtime listener published on
# 127.0.0.1:8786. Control, Postgres, and the static web UI stay in Docker.
#
# Local access: browse http://127.0.0.1:8788/.
# Tailnet access: pair with infra/serve.sh. The browser derives the websocket
# endpoint from the page location, so both access paths use the same bundle.
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

# Control plane + Postgres + web in Docker. The runtime is a host process
# (below); --remove-orphans clears a previously-dockerized runtime container.
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

# Re-running this script should replace any prior host runtime from a previous
# launch without touching Docker (sessions survive a web-only rebuild; a full
# re-run still restarts runtime intentionally).
sudo -n pkill -f "$RUNTIME_BIN" 2>/dev/null || true
sleep 0.2

if [ -n "${XDG_CONFIG_HOME:-}" ]; then
  sudo -n env HOME="$HOME" PATH="$PATH" XDG_CONFIG_HOME="$XDG_CONFIG_HOME" "$RUNTIME_BIN" &
else
  sudo -n env HOME="$HOME" PATH="$PATH" "$RUNTIME_BIN" &
fi

shutdown() {
  trap - EXIT INT TERM
  # pi-runtime runs as root; stop it by binary path (its sudo child outlives a
  # kill of the backgrounded sudo pid). Leave Docker services running so a
  # Ctrl-C does not drop control/postgres/web or force a full stack rebuild.
  sudo -n pkill -f "$RUNTIME_BIN" 2>/dev/null || true
  wait 2>/dev/null || true
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
