#!/usr/bin/env bash
# M5 bridge runner: wires env (agent dir, GLM key, kernel venv, PG DSN, token),
# ensures the GLM SSE shim is up, then starts the bridge in the FOREGROUND.
# Killing the bridge does not kill the shim (needed for B6 restart-in-place).
# Usage: ./run-bridge.sh
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
DEMO="$(cd "$HERE/../../.pi/m1-demo" && pwd)"

export PI_CODING_AGENT_DIR="${PI_CODING_AGENT_DIR:-$DEMO/agent}"
export PRIME_RLM_KERNEL_VENV="${PRIME_RLM_KERNEL_VENV:-$DEMO/venv}"
# GLM key: piped from file, never printed or persisted.
export NVIDIA_INFERENCE_API_KEY="$(tr -d '\n\r' < /home/schwinns/inference_hub_key)"
export SHIM_PORT="${SHIM_PORT:-8571}"

# Control plane: TEST container only (127.0.0.1:56432). NEVER the live 55432.
BRIDGE_PG_PASSWORD="$(tr -d '\n\r' < "$DEMO/.bridge-pg-password")"
export BRIDGE_PG_URL="${BRIDGE_PG_URL:-postgres://postgres:${BRIDGE_PG_PASSWORD}@127.0.0.1:56432/pi_relay_bridge}"

# Auth token for the WS contract (generated once, root-only file, gitignored tree).
TOKEN_FILE="$DEMO/.bridge-auth-token"
if [ ! -f "$TOKEN_FILE" ]; then
  (umask 077 && head -c 24 /dev/urandom | base64 | tr -d '=+/ \n' > "$TOKEN_FILE")
fi
export BRIDGE_AUTH_TOKEN="$(tr -d '\n\r' < "$TOKEN_FILE")"
export BRIDGE_ALLOWED_ORIGINS="${BRIDGE_ALLOWED_ORIGINS:-http://localhost:3000,https://relay.pi.test}"
export BRIDGE_PORT="${BRIDGE_PORT:-8730}"
# Test-only static bearer for the M8 mock MCP server (never a real credential).
export MOCK_MCP_TOKEN="${MOCK_MCP_TOKEN:-mock-static-token-abc123}"
export BRIDGE_PID_FILE="${BRIDGE_PID_FILE:-$HERE/data/bridge.pid}"

mkdir -p "$HERE/data"

# Shim: start detached only if not already healthy (survives bridge restarts).
if ! curl -sf "http://127.0.0.1:$SHIM_PORT/healthz" >/dev/null 2>&1; then
  nohup node "$DEMO/shim-proxy.mjs" >> "$HERE/data/shim.log" 2>&1 &
  for i in $(seq 1 50); do
    curl -sf "http://127.0.0.1:$SHIM_PORT/healthz" >/dev/null 2>&1 && break
    sleep 0.1
  done
fi

cd "$HERE"
exec node src/index.ts
