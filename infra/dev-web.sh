#!/usr/bin/env bash
# Run Postgres + agent-daemon + pi-web API + Vite HMR in foreground.
#
# Vite owns :8788 and proxies /api and /healthz to loopback pi-web on :8789.
# Tailscale still sends /ws directly to pi-agentd via infra/serve.sh.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

DAEMON_BIND="${DAEMON_BIND:-127.0.0.1:8787}"
API_PORT="${API_PORT:-8789}"
DATABASE_URL="${DATABASE_URL:-postgres://postgres:postgres@127.0.0.1:55432/pi_relay}"
TAILNET_HOST="${TAILNET_HOST:-}"

bun install
docker compose -f infra/docker-compose.yml up -d --wait

cargo run --manifest-path rust/Cargo.toml -p agent-daemon -- \
  --database-url "$DATABASE_URL" \
  --bind "$DAEMON_BIND" &
DAEMON_PID=$!

wait_for_daemon() {
  local host="${DAEMON_BIND%:*}"
  local port="${DAEMON_BIND##*:}"
  host="${host#[}"
  host="${host%]}"
  for _ in $(seq 1 120); do
    if (: >/dev/tcp/"$host"/"$port") 2>/dev/null; then
      return
    fi
    if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
      echo "pi-agentd exited before completing schema migration" >&2
      return 1
    fi
    sleep 0.5
  done
  echo "timed out waiting for pi-agentd schema readiness" >&2
  return 1
}

shutdown() {
  trap - EXIT INT TERM
  if [[ -n "${VITE_PID:-}" ]]; then
    kill "$VITE_PID" 2>/dev/null || true
  fi
  if [[ -n "${WEB_PID:-}" ]]; then
    kill "$WEB_PID" 2>/dev/null || true
  fi
  kill "$DAEMON_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap shutdown EXIT INT TERM

# pi-agentd owns migrations and binds only after migration/startup recovery.
# Never start the SELECT-only web process until that readiness boundary.
wait_for_daemon

PI_WEB_ALLOWED_HOSTS="$TAILNET_HOST" \
cargo run --manifest-path rust/Cargo.toml -p pi-web -- \
  --database-url "$DATABASE_URL" \
  --bind "127.0.0.1:${API_PORT}" \
  --web-root "$REPO_ROOT/packages/web/dist" &
WEB_PID=$!

( cd packages/web && \
    PI_WEB_DEV_TARGET="http://127.0.0.1:${API_PORT}" \
    VITE_PI_AGENT_WS="${TAILNET_HOST:+wss://${TAILNET_HOST}/ws}" \
    VITE_PI_ALLOWED_HOSTS="$TAILNET_HOST" \
    bun run dev ) &
VITE_PID=$!

for _ in $(seq 1 60); do
  if curl --fail --silent --show-error "http://127.0.0.1:${API_PORT}/healthz" >/dev/null; then
    break
  fi
  if ! kill -0 "$DAEMON_PID" "$WEB_PID" "$VITE_PID" 2>/dev/null; then
    echo "pi-relay development service exited during startup" >&2
    exit 1
  fi
  sleep 0.5
done
curl --fail --silent --show-error "http://127.0.0.1:${API_PORT}/healthz" >/dev/null
echo "pi-relay Vite development UI ready at http://127.0.0.1:8788/"

wait -n "$DAEMON_PID" "$WEB_PID" "$VITE_PID"
