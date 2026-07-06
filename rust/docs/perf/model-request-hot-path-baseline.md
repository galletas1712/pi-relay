# Model-request hot-path Stage 0 baseline

This artifact records the corrected Stage 0 baseline. It is evidence about the
current implementation, not an optimization result and not an allocator
profile.

## Environment

The captured run used:

- Linux 6.17, x86_64;
- `rustc 1.95.0 (59807616e 2026-04-14)`; and
- `cargo 1.95.0 (f2d3ce0bd 2026-03-21)`.

Wall-clock and compression-size observations can vary with CPU scheduling,
toolchain, zstd version, and platform. Deterministic counter snapshots are
asserted in tests. All `*_ns` fields are normalized to zero before exact
aggregate comparison. `lock_wait_ns` and `output_transaction_ns` are narrower
overlapping diagnostics and are not added to exclusive classified phase time.

The database commands used a disposable local PostgreSQL administrator
connection supplied through `PI_RELAY_TEST_DATABASE_URL`. The URL and
credentials are intentionally omitted. Existing test helpers created and
dropped a unique database for each body. Provider traffic used a loopback mock
HTTP server; no provider credentials or external network were used.

## Exact operation snapshots

### Local OpenAI model turn

`local_openai_turn_records_exact_model_hot_path_and_reaches_idle` exercises:

1. OpenAI model metadata retrieval from the loopback server;
2. ordinary request construction and JSON serialization;
3. zstd compression and the physical HTTP send;
4. fragmented chunked SSE framing and parsing;
5. model output persistence; and
6. the final idle transition.

It asserts the GET `/models` and POST `/responses` requests, zstd wire
encoding, non-empty body, persisted output, and idle state. The exact
`model_action` snapshot, after normalizing duration fields, is:

```text
active_context_materializations=0
active_context_materialized_bytes=0
latest_context_bytes=0
request_copies=1
request_copied_bytes=0
logical_model_request_builds=1
provider_body_serializations=1
provider_body_serialized_bytes=<nonzero, toolchain/provider-catalog dependent>
provider_body_compressions=1
provider_body_encoded_bytes=<nonzero, zstd/toolchain dependent>
compaction_gate_passes=1
accounting_passes=0
logical_count_token_requests=0
physical_count_token_sends=0
model_attempts=1
model_retries=0
physical_provider_sends=1
provider_auth_retries=0
auth_refreshes=0
provider_failures_persisted=0
sse_received_bytes=367
sse_scan_windows=1073
sse_frames=2
sse_peak_retained_bytes=247
provider_request_wait_ns=0
provider_stream_wait_ns=0
provider_metadata_wait_ns=0
request_preparation_ns=0
tool_execution_ns=0
output_persistence_wall_ns=0
coordination_wait_ns=0
classified_wall_ns=0
unclassified_wall_ns=0
total_elapsed_ns=0
nested_operation_ns=0
exclusive_elapsed_ns=0
session_registry_scans=1
session_registry_entries_scanned=1
dispatch_task_registry_scans=1
dispatch_task_registry_entries_scanned=0
lock_wait_ns=0
output_sql_statements=18
output_transactions=2
output_transaction_ns=0
recovery_sql_statements=0
scoped_store_calls=15
cold_rows_loaded=0
cold_bytes_loaded=0
empty_persist_passes=1
empty_dispatch_passes=1
action_completion_scans=1
action_completion_entries_scanned=1
```

The SQL evidence is intentionally narrow. `output_sql_statements` covers
scoped output persistence plus the idle event, not every query between
completion and idle. `scoped_store_calls=15` proves the selected repository
operations ran but is not interchangeable with SQL count. Stages 7 and 8 must
add a dedicated total-query measurement before using a total-statement target.

### Cold activation followed by warm work

`cold_recovery_finishes_before_resumed_model_dispatch` starts from an inactive
stored session with queued work, completes the cold owner, then resumes a
separately owned warm model action. Its exact `cold_activation` snapshot is:

```text
active_context_materializations=1
active_context_materialized_bytes=19
latest_context_bytes=19
request_copies=0
request_copied_bytes=0
logical_model_request_builds=0
provider_body_serializations=0
provider_body_serialized_bytes=0
provider_body_compressions=0
provider_body_encoded_bytes=0
compaction_gate_passes=0
accounting_passes=0
logical_count_token_requests=0
physical_count_token_sends=0
model_attempts=0
model_retries=0
physical_provider_sends=0
provider_auth_retries=0
auth_refreshes=0
provider_failures_persisted=0
sse_received_bytes=0
sse_scan_windows=0
sse_frames=0
sse_peak_retained_bytes=0
provider_request_wait_ns=0
provider_stream_wait_ns=0
provider_metadata_wait_ns=0
request_preparation_ns=0
tool_execution_ns=0
output_persistence_wall_ns=0
coordination_wait_ns=0
classified_wall_ns=0
unclassified_wall_ns=0
total_elapsed_ns=0
nested_operation_ns=0
exclusive_elapsed_ns=0
session_registry_scans=0
session_registry_entries_scanned=0
dispatch_task_registry_scans=0
dispatch_task_registry_entries_scanned=0
lock_wait_ns=0
output_sql_statements=0
output_transactions=0
output_transaction_ns=0
recovery_sql_statements=2
scoped_store_calls=2
cold_rows_loaded=3
cold_bytes_loaded=116
empty_persist_passes=0
empty_dispatch_passes=0
action_completion_scans=0
action_completion_entries_scanned=0
```

This snapshot does not claim to count all cold SQL. The two recovery statements
are the two `load_stored_session` queries. Control reconciliation occurs before
the inactive-runtime fence and is deliberately outside this owner.

### OpenAI compaction

`compact_request_records_body_and_physical_send_in_compaction_operation`
performs a loopback `/responses/compact` request under a `compaction` owner. It
asserts exactly one body serialization, serialized bytes equal to the request
`Content-Length`, and one physical provider send. It preserves compaction's
pre-existing request timeout rather than adopting ordinary generation's
response-header timeout.

## Deterministic complexity probes

These are deterministic probes with elapsed time/throughput output, not
statistically controlled benchmarks:

```sh
cd rust
cargo test -p agent-session \
  model_context_materialization_scaling_1_10_100_mib \
  -- --ignored --nocapture
cargo test -p agent-session \
  common_context_clone_cost_probe \
  -- --ignored --nocapture
cargo test -p agent-session \
  reverse_order_completion_scaling_k_1_10_100_1000 \
  -- --ignored --nocapture
```

Captured observations:

| Probe | Exact deterministic counters | Captured wall time / throughput |
| --- | --- | --- |
| context 1 MiB | 1 materialization, 1,048,576 content bytes | 461,982 ns / 2,164.6 MiB/s |
| context 10 MiB | 1 materialization, 10,485,760 content bytes | 4,216,764 ns / 2,371.5 MiB/s |
| context 100 MiB | 1 materialization, 104,857,600 content bytes | 50,241,412 ns / 1,990.4 MiB/s |
| four named clone sites over 10 MiB | 4 copies, 41,943,040 content bytes | 17,092,091 ns / 2,340.3 MiB/s |
| reverse K=1 | 1 completion entry examined | 30,778 ns |
| reverse K=10 | 55 completion entries examined | 10,810 ns |
| reverse K=100 | 5,050 completion entries examined | 124,926 ns |
| reverse K=1,000 | 500,500 completion entries examined | 7,436,317 ns / about 67.3 million examined entries/s |

The four clone-site labels are `dispatch_vector`, `session_start`,
`deferred_subagent`, and `title_worker`. Bytes are measured string/replay
content and intentionally do not claim allocator-exact object size.

SSE exact probes:

| Shape | Received bytes | Frames | Delimiter scan windows |
| --- | ---: | ---: | ---: |
| fragmented ordinary fixture | fixture-defined | 2 | 307 |
| 1,000 small frames in one backlog | 19,000 | 1,000 | 9,524,500 |
| 1 MiB payload fragmented at 4,093 bytes | 1,048,598 | 1 | 271,382,824 |

The expensive 1 MiB adversarial case is ignored in ordinary unit runs:

```sh
cargo test -p agent-provider \
  instrumentation_aggregates_large_fragmented_frame \
  -- --ignored --nocapture
```

The many-frame and fragmented cases expose the current parser's rescanning
cost; Stage 0 does not optimize it. Profiling runs the exact same two
`position` searches as the unprofiled parser. It derives examined-window counts
from each result in O(1), accumulates SSE counters and stream wait locally, and
publishes once when the parser exits.

## Disabled-path overhead methodology

The release example compares no-hook and disabled-hook implementations in
alternating order. Each shape takes 21 paired samples and reports median
baseline time, median instrumented time, median percent overhead, and full
percent range:

```sh
cd rust
env -u PI_RELAY_PERF cargo run --release -p agent-perf \
  --example disabled_overhead
```

Shapes include:

- 2,000,000 calls to one O(1) content hook;
- 500,000 iterations of one batched SSE publication hook; and
- eight reverse-order K=1,000 action scans, which prove the disabled path uses
  the original iterator and does no per-entry counter accumulation.

Three captured invocations produced these median overheads:

| Shape | Run medians | Median range across runs |
| --- | --- | --- |
| O(1) hook | 598.005%, 601.531%, 601.851% | 598.005%..601.851% |
| historical three SSE hooks | 1610.122%, 1910.912%, 1605.619% | 1605.619%..1910.912% |
| reverse K=1,000 | 2.204%, 2.394%, 2.332% | 2.204%..2.394% |

The complete per-run 21-sample ranges are machine-noisy and emitted by the
command. In this capture they were 470.489%..676.910% for O(1),
1597.383%..1930.845% for SSE, and -4.769%..38.201% for K. There is deliberately
no wall-clock assertion: the earlier one-shot `<1%` claim was unsupported. The
O(1)/SSE percentages are large because their no-hook baselines are nearly
empty loops. The historical SSE capture predates local response aggregation;
the current live parser performs one cached enabled/task-local lookup and one
publication per response, not per delimiter window. The
K-shaped probe demonstrates that no additional per-entry work remains when
disabled.

## Live release runbook

Run one provider and one scenario per daemon because fixed records
intentionally contain no IDs:

```sh
cd rust
cargo build --release -p agent-daemon
umask 077
rm -f ./perf-actions.log
PI_RELAY_PERF=1 PI_RELAY_PERF_FILE=./perf-actions.log \
  ./target/release/pi-agentd
```

`PI_RELAY_PERF` is enabled by presence, so `PI_RELAY_PERF=0` also enables it.
`PI_RELAY_PERF_FILE` is a dedicated profiler-only sink opened once on first
emission, created if absent, and written under a process-local lock. It uses
append mode and never truncates or changes an existing file's permissions, so
remove stale output and use `umask 077` before each launch. Use one daemon per
file. Sink open/write failures are best-effort and do not alter daemon behavior.
Without the file setting, records fall back to stderr for convenience, but a
filter over mixed stderr is not a privacy/authenticity boundary because
provider-controlled multiline text can imitate records.

Exclusive fields are `provider_request_wait_ns`,
`provider_stream_wait_ns`, `provider_metadata_wait_ns`,
`request_preparation_ns`, `tool_execution_ns`,
`output_persistence_wall_ns`, and `coordination_wait_ns`; they sum to
`classified_wall_ns`. `total_elapsed_ns` is inclusive owner wall,
`nested_operation_ns` is time spent in synchronous child collector scopes, and
`exclusive_elapsed_ns = total_elapsed_ns - nested_operation_ns`, saturating.
The outer phase is suspended during a nested operation, and
`unclassified_wall_ns = exclusive_elapsed_ns - classified_wall_ns`,
saturating.

Interpret each record independently:

- `provider_wait = provider_request_wait_ns + provider_stream_wait_ns +
  provider_metadata_wait_ns`;
- model daemon/outside-provider time is
  `exclusive_elapsed_ns - provider_wait`;
- tool daemon/outside-tool time is
  `exclusive_elapsed_ns - tool_execution_ns`;
- `classified_wall_ns` is the seven-bucket exclusive sum; and
- `unclassified_wall_ns` is only the unbucketed exclusive remainder, not all
  daemon overhead.

All subtraction and addition in these formulas is saturating. Synchronous
nested scope intervals are disjoint from the parent exclusive duration, but
inclusive totals are not: do not add a `web_sidecar` record to an inclusive
parent tool interval. Independently spawned portions of successor operations
and other concurrent operations, especially title sidecars, can still overlap;
shared metadata wait can also be observed by multiple callers. With no IDs or
timestamps these records cannot form a disjoint scenario total. Measure
whole-scenario wall externally from input acceptance through durable
`session.idle`; action records are not a user-turn trace. Physical send counters
exclude detached metadata GETs, but caller-visible cold/shared metadata wait is
included. Live context byte attribution is intentionally incomplete because
the gate no longer rescans an already-built context solely for profiling.

## Reproduction commands

From `rust/`:

```sh
# Portable focused instrumentation.
cargo test -p agent-perf
cargo test -p agent-provider instrumentation_aggregates
cargo test -p agent-provider \
  compact_request_records_body_and_physical_send_in_compaction_operation

# Explicit database baselines. The environment value must identify a
# disposable local PostgreSQL administrator connection.
PI_RELAY_TEST_DATABASE_URL='<local-test-admin-url>' \
  cargo test -p agent-daemon \
  delegation_runner::tests::local_openai_turn_records_exact_model_hot_path_and_reaches_idle \
  -- --ignored --exact --nocapture
PI_RELAY_TEST_DATABASE_URL='<local-test-admin-url>' \
  cargo test -p agent-daemon \
  delegation_runner::tests::cold_recovery_finishes_before_resumed_model_dispatch \
  -- --ignored --exact --nocapture
```

The database tests are marked ignored so a normal workspace remains portable.
When selected, absence of `PI_RELAY_TEST_DATABASE_URL` is a hard test failure,
not a skipped body.

## Validation evidence

The captured correction run produced:

- exact database baselines: 2 selected bodies passed, 0 failed, 0 ignored;
- database-backed `agent-store`: 55 passed, 0 failed, 0 ignored;
- database-backed `agent-daemon`: 197 passed, 0 failed, 2 explicitly ignored,
  followed by the 2 selected baseline passes above;
- full workspace: 549 test/doc-test bodies passed, 0 failed, 6 explicitly
  ignored; and
- warnings-denied Clippy for `agent-perf`, `agent-session`, `agent-provider`,
  `agent-store`, and `agent-daemon`.

The Clippy command allowed only pre-existing
`clippy::too_many_arguments` on `PostgresAgentStore::switch_active_leaf`; a
Stage 0 empty-if warning was fixed by deleting the no-op. Stored DB logs
contained no unavailable-environment or required-DB skip marker. The ordinary
daemon run still lists the two baseline tests as ignored by design; their
separate selected invocations are the execution evidence.
