#!/usr/bin/env bash
# Publish the pi-relay UI + websocket on the tailnet:
#   https://<this-node>.<tailnet>.ts.net/      -> pi-web (UI + /api)
#   https://<this-node>.<tailnet>.ts.net/ws    -> agent-daemon (websocket)
# Requires `tailscale serve` privileges — usually root or tailscale operator.
set -euo pipefail

WEB_PORT="${WEB_PORT:-8788}"
DAEMON_PORT="${DAEMON_PORT:-8787}"

# Reset any prior config so reruns are idempotent.
tailscale serve reset

tailscale serve --bg --set-path=/ws "http://127.0.0.1:${DAEMON_PORT}"
tailscale serve --bg --set-path=/   "http://127.0.0.1:${WEB_PORT}"

tailscale serve status
