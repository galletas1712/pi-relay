#!/usr/bin/env bash
# Run Postgres (compose) + agent-daemon + pi-web in foreground.
#
# Local mode  (TAILNET_HOST unset): browse http://127.0.0.1:8788/.
# Tailnet mode (TAILNET_HOST set):  pair with infra/serve.sh and browse
#   https://${TAILNET_HOST}/ — the bundle has that origin baked in.
#
# Static by design: no HMR or daemon auto-restart. pi-web serves the production
# frontend bundle and the same-origin /api. For Vite HMR, use infra/dev-web.sh.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

DAEMON_BIND="${DAEMON_BIND:-127.0.0.1:8787}"
WEB_PORT="${WEB_PORT:-8788}"
WEB_BIND="${WEB_BIND:-127.0.0.1:${WEB_PORT}}"
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

readiness_url_for_bind() {
  local bind="$1"
  local host port
  if [[ "$bind" =~ ^\[([0-9A-Fa-f:.%]+)\]:([0-9]+)$ ]]; then
    host="${BASH_REMATCH[1]}"
    [[ "$host" == "::" ]] && host="::1"
    host="[$host]"
    port="${BASH_REMATCH[2]}"
  elif [[ "$bind" =~ ^([0-9.]+):([0-9]+)$ ]]; then
    host="${BASH_REMATCH[1]}"
    [[ "$host" == "0.0.0.0" ]] && host="127.0.0.1"
    port="${BASH_REMATCH[2]}"
  else
    echo "WEB_BIND must be a numeric IP socket address (for example 127.0.0.1:8788 or [::1]:8788)" >&2
    return 1
  fi
  printf 'http://%s:%s/healthz' "$host" "$port"
}

WEB_READINESS_URL="$(readiness_url_for_bind "$WEB_BIND")"

PI_WEB_ALLOWED_HOSTS="$TAILNET_HOST" \
cargo run --manifest-path rust/Cargo.toml -p pi-web -- \
  --database-url "$DATABASE_URL" \
  --bind "$WEB_BIND" \
  --web-root "$REPO_ROOT/packages/web/dist" &
WEB_PID=$!

for _ in $(seq 1 60); do
  if curl --noproxy '*' --fail --silent --show-error "$WEB_READINESS_URL" >/dev/null; then
    break
  fi
  if ! kill -0 "$DAEMON_PID" "$WEB_PID" 2>/dev/null; then
    echo "pi-relay service exited during startup" >&2
    exit 1
  fi
  sleep 0.5
done
curl --noproxy '*' --fail --silent --show-error "$WEB_READINESS_URL" >/dev/null
echo "pi-relay ready on ${WEB_BIND}"

wait -n "$DAEMON_PID" "$WEB_PID"
