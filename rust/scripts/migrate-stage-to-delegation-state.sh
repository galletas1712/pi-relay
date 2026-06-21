#!/usr/bin/env bash
# TEMPORARY one-time helper for the hard stage->delegation state migration.
# Run from any directory in this checkout. Remove this script/branch after
# migration. See rust/docs/temp-stage-to-delegation-migration.md for the
# runbook, safety policy, and rollback notes.
set -euo pipefail

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
  cat <<'EOF'
Usage:
  rust/scripts/migrate-stage-to-delegation-state.sh [options]

This wrapper runs:
  cargo run -p agent-store --example migrate_stage_to_delegation_state -- [options]

Options:
  --database-url URL        Postgres URL. Defaults to PI_RELAY_DATABASE_URL, then DATABASE_URL.
  --apply                   Mutate database/files. Without this flag the script is a dry-run.
  --dry-run                 Explicit dry-run (default).
  --no-backups              Skip table backup copies on --apply.
  --handoff-root PATH       Scan PATH/.pi-handoff for structured index.json manifests only.
  --help                    Show this help.

Operator sequence:
  1. Stop pi-agentd; do not run this against a live daemon.
  2. Take a pg_dump backup.
  3. Run a dry-run and inspect the summary/conflicts.
  4. Run again with --apply.
  5. Restart the post-rename daemon.

Important policy:
  - Temporary one-time migration for the hard stage->delegation rename.
  - No compatibility aliases are added.
  - Existing stage_* IDs and .pi-handoff/stage_* directory names are preserved.
  - Handoff final_message.md/transcript.md and arbitrary transcript/user/tool
    text are NOT rewritten; only structured DB/API/tool/RPC fields and handoff
    index.json manifests are migrated.
  - See rust/docs/temp-stage-to-delegation-migration.md for full design
    rationale, fail-closed expected_subagents inference, backup names, and
    rollback caveats.
EOF
  exit 0
fi

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
RUST_DIR="$(cd -- "${SCRIPT_DIR}/.." && pwd)"

cd "${RUST_DIR}"
exec cargo run -p agent-store --example migrate_stage_to_delegation_state -- "$@"
