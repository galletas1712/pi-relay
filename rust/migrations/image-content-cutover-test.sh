#!/bin/sh
set -eu

: "${PI_RELAY_TEST_ADMIN_DATABASE_URL:?set a disposable PostgreSQL administrator URL}"

ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN="$ROOT/target/debug/image-content-cutover"
SCRIPT_DIR="$ROOT/migrations"
DATABASE_NAME="pi_relay_cutover_test_$(date +%s)_$$_$(od -An -N4 -tu4 /dev/urandom | tr -d ' ')"
SENTINEL="sentinel-$(date +%s)-$$"

cargo build --manifest-path "$ROOT/Cargo.toml" -p agent-store --bin image-content-cutover
"$BIN" test-create "$PI_RELAY_TEST_ADMIN_DATABASE_URL" "$DATABASE_NAME" >/dev/null
trap '"$BIN" test-drop "$PI_RELAY_TEST_ADMIN_DATABASE_URL" "$DATABASE_NAME"' EXIT HUP INT TERM

# Derive the child URL without exposing any general database-drop operation.
CHILD_URL=$(python3 - "$PI_RELAY_TEST_ADMIN_DATABASE_URL" "$DATABASE_NAME" <<'PY'
import sys
from urllib.parse import urlsplit, urlunsplit
url, database = sys.argv[1:]
parts = urlsplit(url)
print(urlunsplit((parts.scheme, parts.netloc, "/" + database, parts.query, parts.fragment)))
PY
)

"$BIN" test-init "$CHILD_URL" "$DATABASE_NAME" "$SENTINEL"
psql_test() {
  psql -X -v ON_ERROR_STOP=1 -v expected_database="$DATABASE_NAME" \
    -v sentinel="$SENTINEL" "$CHILD_URL" "$@"
}
durable_hash() {
  psql_test -Atc "select md5(jsonb_build_object(
    'transcript',(select coalesce(jsonb_agg(to_jsonb(t) order by id),'[]') from transcript_entries t),
    'actions',(select coalesce(jsonb_agg(to_jsonb(a) order by id),'[]') from actions a),
    'queue',(select coalesce(jsonb_agg(to_jsonb(q) order by id),'[]') from queued_inputs q),
    'events',(select coalesce(jsonb_agg(to_jsonb(e) order by id),'[]') from events e),
    'artifacts',(select coalesce(jsonb_agg(to_jsonb(i) order by id),'[]') from image_artifacts i)
  )::text)"
}

psql_test -f "$SCRIPT_DIR/image-content-cutover-test.sql"

if "$BIN" report --expected-database "${DATABASE_NAME}_wrong" "$CHILD_URL"; then
  echo "report unexpectedly accepted the wrong database identity" >&2
  exit 1
fi

BEFORE=$(durable_hash)
"$BIN" check --expected-database "$DATABASE_NAME" "$CHILD_URL"
[ "$BEFORE" = "$(durable_hash)" ] || {
  echo "check changed data" >&2
  exit 1
}

"$BIN" apply --expected-database "$DATABASE_NAME" "$CHILD_URL"
psql_test -f "$SCRIPT_DIR/image-content-cutover-test-assert.sql"
FIXED=$("$BIN" check --expected-database "$DATABASE_NAME" "$CHILD_URL")
printf '%s\n' "$FIXED" | grep -q '^total convertible=0 .* invalid=0$'
FIXED_HASH=$(durable_hash)
"$BIN" apply --expected-database "$DATABASE_NAME" "$CHILD_URL"
[ "$FIXED_HASH" = "$(durable_hash)" ] || {
  echo "second apply was not an exact fixed point" >&2
  exit 1
}

psql_test -c "insert into actions(
  id,session_id,action_id,attempt_id,kind,status,payload,result
) values
  ('mismatch-completed-reason','cutover-session',20,'mismatch-20','tool','completed','{}','{\"reason\":\"wrong\"}'),
  ('mismatch-error-reason','cutover-session',21,'mismatch-21','tool','error','{}','{\"reason\":\"wrong\"}'),
  ('mismatch-completed-control','cutover-session',22,'mismatch-22','tool','completed','{}','{\"reason\":\"wrong\",\"control_input_id\":\"input\"}'),
  ('mismatch-error-control','cutover-session',23,'mismatch-23','tool','error','{}','{\"reason\":\"wrong\",\"control_input_id\":\"input\"}'),
  ('mismatch-completed-error','cutover-session',24,'mismatch-24','tool','completed','{}','{\"error\":\"wrong\"}'),
  ('mismatch-interrupted-error','cutover-session',25,'mismatch-25','tool','interrupted','{}','{\"error\":\"wrong\"}'),
  ('mismatch-interrupted-result','cutover-session',26,'mismatch-26','tool','interrupted','{}',
   '{\"tool_call_id\":\"call\",\"tool_name\":\"Bash\",\"content\":[{\"type\":\"text\",\"text\":\"wrong\"}],\"status\":\"Success\"}'),
  ('mismatch-completed-error-result','cutover-session',27,'mismatch-27','tool','completed','{}',
   '{\"tool_call_id\":\"call-error\",\"tool_name\":\"Bash\",\"content\":[{\"type\":\"text\",\"text\":\"wrong\"}],\"status\":\"Error\"}'),
  ('mismatch-error-success-result','cutover-session',28,'mismatch-28','tool','error','{}',
   '{\"tool_call_id\":\"call-success\",\"tool_name\":\"Bash\",\"content\":[{\"type\":\"text\",\"text\":\"wrong\"}],\"status\":\"Success\"}'),
  ('mismatch-completed-root-unknown','cutover-session',29,'mismatch-29','tool','completed','{}',
   '{\"tool_call_id\":\"call-root\",\"tool_name\":\"Bash\",\"content\":[{\"type\":\"text\",\"text\":\"wrong\"}],\"status\":\"Success\",\"future_metadata\":{\"forbidden\":true}}'),
  ('mismatch-completed-text-unknown','cutover-session',30,'mismatch-30','tool','completed','{}',
   '{\"tool_call_id\":\"call-text\",\"tool_name\":\"Bash\",\"content\":[{\"type\":\"text\",\"text\":\"wrong\",\"future_field\":true}],\"status\":\"Success\"}'),
  ('mismatch-completed-image-unknown','cutover-session',31,'mismatch-31','tool','completed','{}',
   '{\"tool_call_id\":\"call-image\",\"tool_name\":\"ReadImage\",\"content\":[{\"type\":\"image\",\"artifact_id\":\"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\",\"future_field\":true}],\"status\":\"Success\"}')"
MISMATCH_BEFORE=$(durable_hash)
if "$BIN" check --expected-database "$DATABASE_NAME" "$CHILD_URL"; then
  echo "check unexpectedly accepted tool action status/result mismatches" >&2
  exit 1
fi
[ "$MISMATCH_BEFORE" = "$(durable_hash)" ] || {
  echo "status mismatch check changed data" >&2
  exit 1
}
if "$BIN" apply --expected-database "$DATABASE_NAME" "$CHILD_URL"; then
  echo "apply unexpectedly accepted tool action status/result mismatches" >&2
  exit 1
fi
[ "$MISMATCH_BEFORE" = "$(durable_hash)" ] || {
  echo "status mismatch apply changed data" >&2
  exit 1
}
psql_test -c "delete from actions where id like 'mismatch-%'"

psql_test -c "insert into transcript_entries(
  session_id,id,parent_id,timestamp_ms,item,provider_replay
) values (
  'cutover-session','rollback-convertible','legacy-tool',4,
  '{\"type\":\"user_message\",\"content\":[
    {\"type\":\"image\",\"image\":{\"source\":{\"kind\":\"base64\",\"mime_type\":\"image/gif\",\"data\":\"R0lGODlhAQABAIAAAAAAAP///ywAAAAAAQABAAACAUwAOw==\"}}}
  ],\"late_metadata\":{\"must\":\"survive\"}}',
  '[]'
)"
psql_test -c "insert into events(session_id,type,payload) values(
  'cutover-session','transcript.appended',
  '{\"entry\":{\"item\":{\"type\":\"user_message\",\"content\":[{\"type\":\"image\",\"image\":{\"source\":{\"kind\":\"base64\",\"mime_type\":\"image/png\",\"data\":\"invalid\"}}}]}}}'
)"
ROLLBACK_BEFORE=$(durable_hash)
if "$BIN" apply --expected-database "$DATABASE_NAME" "$CHILD_URL"; then
  echo "apply unexpectedly accepted a late invalid event" >&2
  exit 1
fi
[ "$ROLLBACK_BEFORE" = "$(durable_hash)" ] || {
  echo "late failure did not roll back" >&2
  exit 1
}

echo "image artifact cutover harness passed in generated database $DATABASE_NAME"
