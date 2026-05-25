#!/usr/bin/env bash
# Run Postgres (compose) + agent-daemon + web (vite preview) in foreground.
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

DAEMON_BIND="${DAEMON_BIND:-127.0.0.1:8787}"
WEB_PORT="${WEB_PORT:-8788}"
DATABASE_URL="${DATABASE_URL:-postgres://postgres:postgres@127.0.0.1:55432/pi_relay}"
TAILNET_HOST="${TAILNET_HOST:-}"

bun install
docker compose -f infra/docker-compose.yml up -d --wait

# ${VAR:+...} expands to empty when VAR is unset/empty, so local mode passes
# through the rpc.ts default (ws://127.0.0.1:8787) and skips allowed-host gating.
( cd packages/web && \
    VITE_PI_AGENT_WS="${TAILNET_HOST:+wss://${TAILNET_HOST}/ws}" \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run build )

cargo run --manifest-path rust/Cargo.toml -p agent-daemon -- \
  --database-url "$DATABASE_URL" \
  --bind "$DAEMON_BIND" &
DAEMON_PID=$!

( cd packages/web && \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run preview -- --port "$WEB_PORT" ) &
WEB_PID=$!

shutdown() {
  trap - EXIT INT TERM
  kill "$DAEMON_PID" "$WEB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap shutdown EXIT INT TERM

wait -n "$DAEMON_PID" "$WEB_PID"
