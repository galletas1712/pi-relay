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
# The runtime's workspace_root: the state dir whose sessions/ btrfs subvolume
# already holds every session cwd, so <root>/sessions/<workspace_id>/cwd
# resolves to the existing working dirs referenced throughout each transcript.
PI_RUNTIME_ROOT="${PI_RUNTIME_ROOT:-"${XDG_STATE_HOME:-"$HOME/.local/state"}/pi-relay"}"
mkdir -p "$PI_RUNTIME_ROOT"

bun install

# Control plane + Postgres in Docker. The runtime is a host process (below);
# --remove-orphans clears a previously-dockerized runtime container.
docker compose -f infra/docker-compose.yml up -d --build --wait --remove-orphans

# Build + launch pi-runtime as a host process. Root is required for btrfs
# subvolume operations; HOME and PATH are preserved so session tools resolve
# your real environment (binaries, venvs, ~/.agents/skills, ~/.config).
( cd rust && cargo build --release -p agent-runtime )
RUNTIME_BIN="$REPO_ROOT/rust/target/release/pi-runtime"
RUNTIME_CFG="$REPO_ROOT/infra/config/runtime.local.toml"
cat > "$RUNTIME_CFG" <<EOF
runtime_id = "runtime-local"
name = "Local runtime"
control_addr = "127.0.0.1:8786"
workspace_root = "$PI_RUNTIME_ROOT"
EOF
sudo -n env HOME="$HOME" PATH="$PATH" "$RUNTIME_BIN" "$RUNTIME_CFG" &

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
