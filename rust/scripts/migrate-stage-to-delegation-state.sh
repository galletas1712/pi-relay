#!/usr/bin/env bash
# TEMPORARY one-time helper for the hard stage->delegation state migration.
# Run from any directory in this checkout. Remove this script after migration.
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

cd "${RUST_DIR}"
exec cargo run -p agent-store --example migrate_stage_to_delegation_state -- "$@"
