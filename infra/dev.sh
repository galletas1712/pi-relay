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

WEB_PORT="${WEB_PORT:-8788}"
TAILNET_HOST="${TAILNET_HOST:-}"
DEV_XDG_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/pi-relay-dev.XXXXXX")"
export XDG_CONFIG_HOME="$DEV_XDG_ROOT/config"
export XDG_STATE_HOME="$DEV_XDG_ROOT/state"
mkdir -p "$XDG_CONFIG_HOME/pi-relay"
cat >"$XDG_CONFIG_HOME/pi-relay/config.toml" <<'EOF'
database_url = "postgres://postgres:postgres@127.0.0.1:55432/pi_relay"
bind = "127.0.0.1:8787"
EOF

cleanup_dev_xdg() {
  rm -rf "$DEV_XDG_ROOT"
}
trap cleanup_dev_xdg EXIT

bun install
docker compose -f infra/docker-compose.yml up -d --wait

# ${VAR:+...} expands to empty when VAR is unset/empty, so local mode passes
# through the rpc.ts default (ws://127.0.0.1:8787) and skips allowed-host gating.
( cd packages/web && \
    VITE_PI_AGENT_WS="${TAILNET_HOST:+wss://${TAILNET_HOST}/ws}" \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run build )

cargo run --manifest-path rust/Cargo.toml -p agent-daemon &
DAEMON_PID=$!

( cd packages/web && \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run preview -- --port "$WEB_PORT" ) &
WEB_PID=$!

shutdown() {
  trap - EXIT INT TERM
  kill "$DAEMON_PID" "$WEB_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  cleanup_dev_xdg
}
trap shutdown EXIT INT TERM

wait -n "$DAEMON_PID" "$WEB_PID"
