#!/usr/bin/env bash
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO_ROOT"

LOCAL_ORIGIN=http://127.0.0.1:8788
PRODUCTION_ORIGINS=https://relay.example.com,http://127.0.0.1:8788

if ! rg -Fx \
  'PI_RELAY_ALLOWED_ORIGINS="${PI_RELAY_ALLOWED_ORIGINS-http://127.0.0.1:8788}"' \
  infra/dev.sh >/dev/null; then
  echo "infra/dev.sh must default PI_RELAY_ALLOWED_ORIGINS only when unset" >&2
  exit 1
fi

resolve_shell_origins() {
  PI_RELAY_ALLOWED_ORIGINS="${PI_RELAY_ALLOWED_ORIGINS-http://127.0.0.1:8788}"
  printf %s "$PI_RELAY_ALLOWED_ORIGINS"
}

render_origins() {
  docker compose -f infra/docker-compose.yml config --format json |
    node -e '
      let input = "";
      process.stdin.setEncoding("utf8");
      process.stdin.on("data", (chunk) => input += chunk);
      process.stdin.on("end", () => {
        const config = JSON.parse(input);
        process.stdout.write(config.services.control.environment.PI_RELAY_ALLOWED_ORIGINS);
      });
    '
}

assert_equal() {
  local expected=$1
  local actual=$2
  local description=$3
  if [ "$actual" != "$expected" ]; then
    printf '%s: expected <%s>, got <%s>\n' "$description" "$expected" "$actual" >&2
    exit 1
  fi
}

assert_equal "$LOCAL_ORIGIN" \
  "$(env -u PI_RELAY_ALLOWED_ORIGINS bash -c "$(declare -f resolve_shell_origins); resolve_shell_origins")" \
  "unset shell default"
assert_equal "" \
  "$(PI_RELAY_ALLOWED_ORIGINS= resolve_shell_origins)" \
  "explicit empty shell value"
assert_equal "$PRODUCTION_ORIGINS" \
  "$(PI_RELAY_ALLOWED_ORIGINS="$PRODUCTION_ORIGINS" resolve_shell_origins)" \
  "production shell value"
assert_equal "$LOCAL_ORIGIN" \
  "$(env -u PI_RELAY_ALLOWED_ORIGINS bash -c "$(declare -f render_origins); render_origins")" \
  "unset origin render"
assert_equal "" \
  "$(PI_RELAY_ALLOWED_ORIGINS= render_origins)" \
  "explicit empty origin render"
assert_equal "$PRODUCTION_ORIGINS" \
  "$(PI_RELAY_ALLOWED_ORIGINS="$PRODUCTION_ORIGINS" render_origins)" \
  "production origin render"

echo "origin defaults: ok"
