#!/usr/bin/env bash
# Run Postgres + control + pi-runtime (compose) and web preview.
#
# Local mode  (TAILNET_HOST unset): browse http://127.0.0.1:8788/.
# Tailnet mode (TAILNET_HOST set):  pair with infra/serve.sh and browse
#   https://${TAILNET_HOST}/ — the bundle has that origin baked in.
#
# Static by design: no HMR, no daemon auto-restart. The agent-daemon edits
# this repo, so an in-flight bad edit must not tear down running services.
# Apply changes by Ctrl-C and re-running.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

WEB_PORT="${WEB_PORT:-8788}"
TAILNET_HOST="${TAILNET_HOST:-}"
# Default to the pre-split state dir whose sessions/ btrfs subvolume already
# holds every migrated session cwd, so <root>/sessions/<workspace_id>/cwd
# resolves to the existing working dirs (not an empty pi-runtime dir).
PI_RUNTIME_ROOT="${PI_RUNTIME_ROOT:-"${XDG_STATE_HOME:-"$HOME/.local/state"}/pi-relay"}"
mkdir -p "$PI_RUNTIME_ROOT"
export PI_RUNTIME_ROOT

bun install
docker compose -f infra/docker-compose.yml up -d --build --wait

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
  wait 2>/dev/null || true
}
trap shutdown EXIT INT TERM

wait "$WEB_PID"
