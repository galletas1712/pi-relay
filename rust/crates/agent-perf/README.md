# Opt-in action profiler

`agent-perf` is a workspace-internal Stage 0 profiler for the unoptimized
daemon. It is intentionally fixed-shape and numeric-only. Set `PI_RELAY_PERF`
before process startup to emit one line when each measured operation finishes:

```text
perf operation=model_action outcome=completed ... total_elapsed_ns=...
perf operation=tool_action outcome=completed ... total_elapsed_ns=...
```

The enabled flag is read once with `OnceLock`. Any present value enables it,
including `PI_RELAY_PERF=0`. A disabled process allocates no collector. Records
contain only integers and fixed operation/outcome strings: never IDs,
provider/tool/model names, prompts, URLs, arguments, request or response bodies,
replay, errors, or credentials.

## Ownership and outcomes

Every collector has one non-cloneable owner:

- `model_action`: one model action, from the generic dispatch gate through
  claim, provider attempts, completion persistence, successor dispatch, and
  terminal cleanup;
- `tool_action`: one tool action, from generic action dispatch through setup,
  handler/registry/delegation/web execution, completion persistence, successor
  dispatch, and terminal cleanup;
- `cold_activation`: loading an inactive runtime, ending before resumed warm
  dispatch;
- `title_sidecar`: one title worker generation;
- `web_sidecar`: one web sidecar provider request; and
- `compaction`: one native compaction task.

Collectors are not stitched into a user-turn context across spawned tasks.
Each successor model/tool action owns a new record. Outcomes are `completed`,
`failed`, `panicked`, `gate_blocked`, `claim_lost`, `harness_deferred`, and
implicit `aborted` when an enabled owner is dropped. `provider_failures_persisted`
distinguishes a provider failure/refusal/incomplete result whose durable failure
transition succeeded from a genuinely successful model completion.

`total_elapsed_ns` starts when the enabled/test `Metrics` owner is allocated and
is captured before stderr emission on finish or drop.

## Exclusive wall-clock fields

RAII phase guards survive ordinary error returns and account elapsed time when
Rust runs destructors during cancellation or panic unwinding. Nested phases
pause their parent. Therefore these fields are exclusive and their sum is
`classified_wall_ns`:

| Field | Boundary |
| --- | --- |
| `provider_request_wait_ns` | Physical provider request upload and response-header await at `.send().await`. Includes ordinary generation, OpenAI compaction, and Anthropic token count. |
| `provider_stream_wait_ns` | Only response chunk/body awaits: SSE `response.chunk().await`, non-success body awaits, and buffered unary bodies. Framing and JSON parsing are excluded. |
| `provider_metadata_wait_ns` | Caller-visible shared model metadata resolution, including cold single-flight wait. Detached refresh tasks do not inherit the collector. |
| `request_preparation_ns` | Logical prompt/request construction, config/credential/provider lookup, provider body shaping, serialization, and compression. Metadata and transport waits are excluded. |
| `tool_execution_ns` | Only the selected LoadSkill, web, delegation, or tool-registry execution branch. Tool setup and completion orchestration are excluded. |
| `output_persistence_wall_ns` | Output persistence from before pool `begin` through commit, plus standalone terminal event writes such as final `session.idle`. |
| `coordination_wait_ns` | Session-driver acquisition, measured spawn/register/start handoff, model claim, and retry backoff. |
| `classified_wall_ns` | Exclusive sum of the seven phase buckets. |
| `unclassified_wall_ns` | Saturating `total_elapsed_ns - classified_wall_ns`; CPU work and boundaries not explicitly classified remain here. |

`lock_wait_ns` and `output_transaction_ns` are narrower diagnostics retained
from the deterministic baseline. They overlap `coordination_wait_ns` and
`output_persistence_wall_ns`, respectively, and **must not** be added to
`classified_wall_ns`.

## Numeric diagnostics

The remaining counters preserve the Stage 0 shape:

- context materialization/copy counters are deterministic content-size lower
  bounds, not allocator usage. Live byte attribution is incomplete by design:
  the profiler does not rescan an already-built `ModelContext` merely to
  populate `latest_context_bytes`; clone bytes remain zero unless a size was
  already observed in the same collector;
- request build, serialization, compression, attempt/retry/auth counters
  describe logical and physical provider work;
- `physical_provider_sends` and `physical_count_token_sends` count instrumented
  action-owned sends. Detached metadata GETs are deliberately excluded;
- `sse_received_bytes`, `sse_scan_windows`, `sse_frames`, and
  `sse_peak_retained_bytes` are accumulated locally per response and published
  once on parser exit. The enabled parser runs the same delimiter searches as
  the baseline and derives examined windows from the search results;
- session/task registry, selected store call, SQL/transaction, cold-load,
  empty-pass, and action-completion counters are narrow shape diagnostics, not
  total CPU, query, allocation, or I/O measurements.

Deterministic context/clone/scaling probes remain available in their existing
ignored tests; they are not extra traversals in the live gate.

## Release live run

Use one provider and one scenario per daemon. Records intentionally have no
session/action/turn IDs, so concurrent heterogeneous work cannot be correlated
after capture.

```sh
cd rust
cargo build --release -p agent-daemon

# Launch the unoptimized runtime implementation with only profiling enabled.
PI_RELAY_PERF=1 ./target/release/pi-agentd 2>perf.stderr

# Archive only the privacy-minimal fixed records.
grep '^perf operation=' perf.stderr >perf-actions.log
```

Use the daemon exactly as usual and run a long real-provider/tool scenario.
Measure whole-task wall time separately, from external input acceptance through
the durable `session.idle` event. Per-action records cannot replace that
end-to-end measurement.

Do not archive all lines containing `PI_RELAY_PERF` output as privacy-minimal
data. Existing free-form RPC performance lines (for example `perf history.tree
session=...`) can contain identifiers. Only lines matching
`^perf operation=` have this profiler's fixed numeric-only contract.

See
[`rust/docs/perf/model-request-hot-path-baseline.md`](../../docs/perf/model-request-hot-path-baseline.md)
for loopback fixtures, exact deterministic counters, and validation commands.
