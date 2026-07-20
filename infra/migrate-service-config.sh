#!/usr/bin/env bash
# One-time split of the former shared pi-relay configuration root.
# Stop pi-agentd and pi-runtime before running this script.
set -euo pipefail

if [ "${1:-}" != "--apply" ] || [ "$#" -ne 1 ]; then
  echo "usage: $0 --apply" >&2
  echo "stop pi-agentd and pi-runtime before applying this one-time migration" >&2
  exit 2
fi

CONFIG_HOME="${XDG_CONFIG_HOME:-"$HOME/.config"}"
STATE_HOME="${XDG_STATE_HOME:-"$HOME/.local/state"}"
for home in "$CONFIG_HOME" "$STATE_HOME"; do
  case "$home" in
    /*) ;;
    *)
      echo "XDG configuration and state homes must be absolute: $home" >&2
      exit 1
      ;;
  esac
done
OLD_ROOT="$CONFIG_HOME/pi-relay"
AGENTD_ROOT="$CONFIG_HOME/pi-agentd"
RUNTIME_ROOT="$CONFIG_HOME/pi-runtime"
WORKSPACE_ROOT="${PI_RUNTIME_ROOT:-"$STATE_HOME/pi-relay"}"

if [ -L "$OLD_ROOT" ]; then
  echo "source configuration root must not be a symlink: $OLD_ROOT" >&2
  exit 1
fi
if [ ! -f "$OLD_ROOT/config.toml" ]; then
  echo "missing source configuration: $OLD_ROOT/config.toml" >&2
  exit 1
fi
for destination in "$AGENTD_ROOT" "$RUNTIME_ROOT"; do
  if [ -e "$destination" ]; then
    echo "destination already exists: $destination" >&2
    exit 1
  fi
done
case "$WORKSPACE_ROOT" in
  /*) ;;
  *)
    echo "PI_RUNTIME_ROOT must be absolute: $WORKSPACE_ROOT" >&2
    exit 1
    ;;
esac

RUNTIME_STAGING="$CONFIG_HOME/.pi-runtime-migration-$$"
if [ -e "$RUNTIME_STAGING" ]; then
  echo "staging path already exists: $RUNTIME_STAGING" >&2
  exit 1
fi
cleanup_staging() {
  rm -f "$RUNTIME_STAGING/config.toml"
  rmdir "$RUNTIME_STAGING" 2>/dev/null || true
}
trap cleanup_staging EXIT
mkdir -m 700 "$RUNTIME_STAGING"
cat >"$RUNTIME_STAGING/config.toml" <<EOF
runtime_id = "${PI_RUNTIME_ID:-runtime-local}"
name = "${PI_RUNTIME_NAME:-Local runtime}"
control_addr = "${PI_RUNTIME_CONTROL_ADDR:-127.0.0.1:8786}"
workspace_root = "$WORKSPACE_ROOT"
EOF
chmod 600 "$RUNTIME_STAGING/config.toml"

mv "$OLD_ROOT" "$AGENTD_ROOT"
trap - EXIT
if [ -f "$AGENTD_ROOT/mcp.toml" ]; then
  mv "$AGENTD_ROOT/mcp.toml" "$RUNTIME_STAGING/mcp.toml"
fi
mv "$RUNTIME_STAGING" "$RUNTIME_ROOT"

echo "agentd configuration: $AGENTD_ROOT"
echo "runtime configuration: $RUNTIME_ROOT"
echo "runtime workspace state remains at: $WORKSPACE_ROOT"
