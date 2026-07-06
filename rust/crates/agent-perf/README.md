# Model hot-path counters

`agent-perf` is a workspace-internal Stage 0 measurement crate. Set
`PI_RELAY_PERF` before process startup to emit one fixed-shape line when a
measured operation finishes:

```text
perf operation=model_turn outcome=completed active_context_materializations=...
```

The enabled flag is read once with `OnceLock`. A disabled process does not
allocate collectors. Existing K-sized action scans and SSE parsing retain their
original iterator paths rather than accumulating per-entry counters.

Records contain integers plus fixed operation/outcome names. They never contain
IDs, prompts, replay, tool arguments, bodies, credentials, or URLs.

## Ownership

Every collector has one non-cloneable owner and one operation:

- `model_turn`: one model action, including its gate, claim, provider work,
  completion persistence, and terminal cleanup;
- `cold_activation`: loading an inactive runtime, ending before resumed warm
  dispatch;
- `title_sidecar`: one title worker generation;
- `web_sidecar`: one web sidecar provider request; and
- `compaction`: one native compaction task.

Tool actions never allocate `model_turn` collectors. Each successor or resumed
model action allocates a fresh collector. Cold activation never lends its
collector to warm dispatch. Pending-control reconciliation that runs before the
inactive-runtime check is intentionally outside `cold_activation`.

Finishing consumes the owner. Outcomes distinguish `completed`, `failed`,
`panicked`, `gate_blocked`, `claim_lost`, `harness_deferred`, and an implicit
`aborted` record if an enabled owner is dropped. A record is not emitted until
the task-local scope and all writers owned by the operation have ended.

## Field definitions and limits

| Fields | What they count |
| --- | --- |
| `active_context_materializations`, `active_context_materialized_bytes`, `latest_context_bytes` | Active-context materializations, cumulative model-visible content bytes, and the most recently observed/materialized content bytes. These are deterministic content-size lower bounds, not allocator usage. |
| `request_copies`, `request_copied_bytes` | Request clones at the auth-retry boundary and the latest context content bytes attributed to each clone. |
| `logical_model_request_builds` | Daemon logical model-request builds. |
| `provider_body_serializations`, `provider_body_serialized_bytes` | Buffered provider request-body serializations and bytes. OpenAI generation and compaction and Anthropic generation/count requests are covered. |
| `provider_body_compressions`, `provider_body_encoded_bytes` | OpenAI zstd operations and compressed bytes. |
| `compaction_gate_passes`, `accounting_passes` | Proactive model gate and model-input accounting passes. |
| `logical_count_token_requests`, `physical_count_token_sends` | Logical count-token operations and actual HTTP sends. Auth retries can make physical sends exceed logical requests. |
| `model_attempts`, `model_retries` | Provider attempts and actual retries after the stale-ownership fence. |
| `physical_provider_sends`, `provider_auth_retries`, `auth_refreshes` | All instrumented provider HTTP sends at the send boundary, 401 retry paths, and credential refresh attempts. Count-token sends are both part of this total and the `physical_count_token_sends` subset. |
| `sse_received_bytes`, `sse_scan_windows`, `sse_frames`, `sse_peak_retained_bytes` | Received bytes, delimiter windows checked by the current front-rescanning parser, complete/final frames, and peak framing input bytes. Scan windows are deliberately not called bytes scanned. |
| `session_registry_scans`, `session_registry_entries_scanned`, `dispatch_task_registry_scans`, `dispatch_task_registry_entries_scanned`, `lock_wait_ns` | Existing weak session-lock cleanup, primary dispatch-task retain passes, entries present, and session-driver lock wait. Auxiliary title-task retention is excluded rather than attributed to a model turn. |
| `output_sql_statements`, `output_transactions`, `output_transaction_ns` | SQL statements inside the scoped `persist_outputs` path plus the final idle-event insert; begun output transactions and time after successful begin. This is not a total SQL/transaction counter. |
| `recovery_sql_statements` | Exactly the two explicit `load_stored_session` queries. It excludes control reconciliation, queue reset, post-compaction, config, workspace, and other recovery work. |
| `scoped_store_calls` | Selected high-level repository calls on measured completion/cold paths, including action claim/preflight, reset, unfinished/pending/queue/activity/config/session/transcript, persistence, event cleanup, and post-compaction operations. Nested calls can each count. This is a call-shape diagnostic, not a SQL statement estimate. |
| `cold_rows_loaded`, `cold_bytes_loaded` | Transcript rows loaded by `load_stored_session` and deterministic model-visible JSON payload bytes. |
| `empty_persist_passes`, `empty_dispatch_passes` | Existing no-output persistence calls and empty pending-dispatch scans. |
| `action_completion_scans`, `action_completion_entries_scanned` | Existing linear outstanding-action completion lookups and queue length examined. Tool actions have no model owner, so reverse-order K behavior is captured by the separate deterministic fixture. |

The unreliable live `full_context_copied_bytes` aggregate was removed. The
deterministic clone probe explicitly clones the same context at four named
Stage 4 comparison sites: dispatch vector, session start, deferred subagent,
and title worker.

## Reproduction

See
[`rust/docs/perf/model-request-hot-path-baseline.md`](../../docs/perf/model-request-hot-path-baseline.md)
for exact snapshots, commands, machine caveats, and the paired statistical
overhead methodology.

The two database baselines are ignored in the portable suite. Selecting either
without `PI_RELAY_TEST_DATABASE_URL` fails with a clear requirement instead of
silently returning. They create and drop isolated databases through the
existing test helper. No test contacts a live provider.
