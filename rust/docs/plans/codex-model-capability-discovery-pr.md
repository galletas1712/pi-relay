# Stacked PR handoff: authenticated Codex model capabilities

Use this as the body outline for the focused PR stacked on OpenAI Responses
correctness PR #215 (`901c94be72021e2fd0db4c4c6e5497b3d865aa3b`). It records
implementation intent, deterministic validation, pinned-source findings, and
the completed sanitized live release test.

## Summary

- Discover the authenticated account's private Codex model catalog from
  `GET /models?client_version=0.142.3`.
- Share one bounded, account-scoped, memory-only, five-minute catalog cache
  across reconstructed OpenAI provider handles.
- Exact-resolve the configured model before every ordinary and compact request,
  then validate model effort and apply discovered parallel-tool capability.
- Tolerate catalog-only/future effort strings as metadata without exposing or
  emitting them as provider-neutral reasoning values.
- Move provider threshold policy into adapters and keep daemon scheduling
  provider-neutral.
- Delete duplicate static OpenAI runtime model/context/effort policy.

## Why

The private catalog is account- and client-version-sensitive. A static table
cannot safely decide whether a selected slug exists, which efforts it accepts,
or which current/default context window should drive proactive compaction. The
sanitized 2026-07-04 probe demonstrates the concrete risk: GPT-5.6 appears only
for newer client versions, Sol/Terra advertise `ultra` while Luna does not, and
GPT-5.4 advertises a 272k current window alongside a 1M maximum that must not
become the default. Advertisement is not necessarily a raw Responses
capability: pinned Codex maps Ultra to Max at request construction and uses
Ultra as a proactive MultiAgent V2 behavior selector.

## Provider-neutral metadata path

```mermaid
flowchart TD
    A[Model action ready] --> B[Daemon asks ModelProvider.model_metadata]
    B --> C{Provider adapter}
    C -->|OpenAI| D[Exact account catalog lookup]
    C -->|Anthropic| E[Resolved Anthropic metadata]
    D --> Q[Ignore catalog-only effort strings for wire configuration]
    Q --> F[Normalized current input window plus recommended auto limit]
    E --> F
    F --> G{Session compaction config}
    G -->|valid explicit window or limit| H[Use explicit values and clamp safely]
    G -->|no explicit values| I{Adapter recommendation present}
    I -->|yes| J[Use provider recommendation]
    I -->|no| K{Authoritative window present}
    K -->|yes| L[Derive neutral 85 percent policy]
    K -->|no| M[No proactive threshold]
    H --> N[Apply 8k anti-churn floor and persisted scheduler state]
    J --> N
    L --> N
    M --> O[Reactive overflow recovery only]
    N --> P[Gate request or dispatch]
```

Provider details stay in adapters:

- OpenAI resolves `context_window.or(max_context_window)` and recommends
  `min(explicit_auto_limit, 90% resolved window)`, deriving 90% when the
  provider limit is null/missing.
- Anthropic preserves the verified 1M→500k recommendation and its generic
  behavior.
- The daemon does not switch on provider/model ids or normalize OpenAI effort.

## Catalog cache state machine

```mermaid
stateDiagram-v2
    [*] --> Empty
    Empty --> Refreshing: first lookup
    Refreshing --> Fresh: complete valid catalog
    Refreshing --> FailureBackoff: non-401 failure
    Refreshing --> Empty: 401 or abandoned refresh
    Fresh --> Fresh: lookup before 5 minute TTL
    Fresh --> Refreshing: TTL expired
    Refreshing --> FailureBackoff: expired refresh fails / retain old snapshot only for diagnostics
    FailureBackoff --> FailureBackoff: lookup before backoff / return same explicit error
    FailureBackoff --> Refreshing: backoff expired
    Fresh --> Empty: base URL or account identity changes
    FailureBackoff --> Empty: base URL or account identity changes
    Refreshing --> Empty: identity generation changes / late result cannot install
```

Important cache invariants:

- Whole-catalog, memory-only, no ETag/disk/public-API fallback.
- Base URL + account id scope; when account id is absent, use a cryptographic
  token fingerprint that is never logged.
- One detached refresh for concurrent callers; no lock held over HTTP.
- A stale snapshot never shapes a new request after TTL expiry or refresh
  failure.
- A 401 is not negative-cached and enters the daemon's existing one-time
  credential refresh/rebuild path used by ordinary Codex provider calls.

## Request shaping

- One `CODEX_CLIENT_VERSION = "0.142.3"` for the models query and User-Agent.
- Models GET reuses common auth/identity headers but sends no body and no
  generation session/window/turn headers; timeout is five seconds.
- Exact slug only: no aliases, prefixes, namespace stripping, unknown-model
  metadata, or substitution.
- Public configured reasoning efforts (`none` through `max`) must exactly match
  the selected catalog entry. `max` is the highest exposed wire value.
- Bounded catalog strings outside that public enum, including `ultra` and
  future unknown values, remain tolerated metadata but can neither be
  configured nor emitted. There is no Ultra→Max alias in pi-relay.
- `supports_parallel_tool_calls` shapes ordinary and compact bodies.
- Local tool declarations remain authoritative. Catalog search/patch selector
  fields are ignored and non-authoritative; unknown future values cannot
  invalidate the catalog or enable native shell/patch actions.
- `service_tier: "priority"` remains unconditional for ordinary and compact
  requests.
- Redirects are disabled for the fixed private Codex catalog and Responses
  provider endpoints.
- The catalog has no output ceiling, so existing explicit
  `max_output_tokens` behavior is unchanged.

## Validation and bounds

- Maximum response body: 4 MiB.
- Maximum complete catalog: 256 models.
- Slugs: nonempty, unique, at most 256 bytes.
- Token limits: positive and safely representable.
- Efforts: at most 16 per model; consumed strings are bounded and unique.
- Any malformed consumed entry rejects the whole response; a successful empty
  catalog is authoritative empty.

Sanitized fixture expectations:

- Sol/Terra/Luna: 372k current/max, null auto limit → 334,800.
- Sol/Terra advertise `low…ultra`; Luna advertises `low…max`; no `none`.
  `ultra` remains non-wire Codex harness metadata.
- GPT-5.4: 272k current/default plus 1M maximum → 244,800, not 900,000.
- No output-ceiling field.

Pinned/live interpretation:

- Pinned Codex
  `98d28aab54ed86714901b6619400598598876dd0` maps Ultra to Max in
  `reasoning_effort_for_request` before Responses requests.
- Under MultiAgent V2 only, Ultra selects `MultiAgentMode::Proactive`; pi-relay
  does not implement that orchestration behavior.
- Literal Sol/Ultra and Terra/Ultra reached the live `/responses` endpoint but
  returned HTTP 400 on every POST. Sol/High, Terra/High, Luna/Max, and
  GPT-5.4/Medium succeeded.

## Removed

- `agent-daemon/src/model_metadata.rs`.
- Daemon OpenAI model-id context/threshold rows.
- Daemon provider/model effort normalization.
- OpenAI adapter GPT-5.6 special-case/clamp logic.
- Embedded Codex User-Agent version `0.130.0`.

## Explicit non-goals

- Public `api.openai.com` transport.
- Dynamic model picker or new RPC/database catalog storage.
- Disk/ETag cache or stale/static success fallback.
- Service-tier configuration or downgrade.
- Catalog-enabled native shell/patch actions.
- Hosted-search capability claims from ambiguous mappings.
- Codex MultiAgent V2/proactive orchestration, an `agent_mode`, or an
  Ultra-to-Max compatibility alias.
- Long-context paid compaction, local compaction, or replay fallback.
- Changes to the broad five-attempt provider retry behavior.

## Testing

- [x] Exact GET/query/common headers/no body/no generation headers.
- [x] Five-second timeout configuration and explicit timeout error.
- [x] Parse/bounds, unknown fields, empty/duplicate/oversize/invalid catalog.
- [x] Current-vs-max context, null/missing auto derivation, explicit clamp.
- [x] Catalog payloads retain `ultra` and future unknown levels without
      invalidating discovery.
- [x] Exposed efforts exact-validate, including Luna/Max and absent `none`.
- [x] Public serde/TypeScript vocabulary ends at `max`; Rust config rejects
      `ultra`, and ordinary/compact bodies cannot emit it.
- [x] Hardcoded priority and discovered parallel-tool shaping for ordinary and
      compact bodies.
- [x] 20 concurrent cold lookups issue one GET; fresh reuse.
- [x] TTL refresh, explicit cold/expired failure, no stale shaping, backoff,
      401 behavior, account generation guard, cancellation safety, and atomic
      replacement.
- [x] Provider-neutral daemon precedence, 334,800 fixture, GPT-5.4 244,800,
      explicit overrides, no static OpenAI threshold, reactive-only fallback,
      Anthropic 1M→500k.
- [x] Tester live-validated reviewed model/effort combinations: lower exposed
      efforts succeeded; literal Sol/Terra Ultra returned HTTP 400.
- [ ] Tester performs the intentionally deferred paid/long-context validation.

## Reviewer focus

1. No path can shape a new OpenAI request from stale/static/unknown metadata.
2. The account/key generation guard prevents cross-account installation.
3. Exposed reasoning support is rejected locally rather than normalized;
   catalog-only levels remain parseable but unreachable from request bodies.
4. Provider-neutral daemon code contains no OpenAI model-id policy.
5. Existing output fail-closed behavior, hardcoded priority, and broad retry
   count remain unchanged.
