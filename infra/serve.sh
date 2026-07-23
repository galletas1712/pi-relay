#!/usr/bin/env bash
# Publish the pi-relay UI on the tailnet:
#   https://<this-node>.<tailnet>.ts.net/  -> compose `web` (nginx on WEB_PORT)
# nginx proxies same-origin /ws to agent-daemon, so Tailscale only needs the
# one HTTP target (avoids HTTP/2 websocket 502s against the daemon directly).
# Requires `tailscale serve` privileges — usually root or tailscale operator.
set -euo pipefail

WEB_PORT="${WEB_PORT:-8788}"

# Reset any prior config so reruns are idempotent.
tailscale serve reset

tailscale serve --bg "http://127.0.0.1:${WEB_PORT}"

tailscale serve status
