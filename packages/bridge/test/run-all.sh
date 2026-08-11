#!/usr/bin/env bash
# M5 verification runner: B1..B8, then M9 R1..R5 (m9-r6-dom runs separately — it spawns Chrome). Starts a bridge if none is healthy.
set -uo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BRIDGE_DIR="$(cd "$HERE/.." && pwd)"
if ! curl -sf "http://127.0.0.1:${BRIDGE_PORT:-8730}/healthz" >/dev/null 2>&1; then
  echo "=== starting bridge ==="
  node -e "import('$HERE/harness.mjs').then(h => h.startBridge())"
fi
RESULTS=()
for t in b1 b2 b3 b4 b5 b6 b7 b8 m9-r1 m9-r2 m9-r3 m9-r4 m9-r5 m11a m11b; do
  echo "=== $t ==="
  if node "$HERE/$t.mjs"; then
    RESULTS+=("$t PASS")
  else
    RESULTS+=("$t FAIL")
  fi
done
echo
echo "===== M5 VERIFICATION SUMMARY ====="
for r in "${RESULTS[@]}"; do echo "$r"; done
