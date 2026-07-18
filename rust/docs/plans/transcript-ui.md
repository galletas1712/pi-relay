# Transcript and switch UI backlog

Status: partially implemented. Last reviewed 2026-06-07.

## Motivation

The transcript hot path and `/switch` picker were rebuilt around turn cards, a
selected-session cache, and a UI-projected transcript wire format. Those landed
and removed the worst full-tree payload and over-refresh problems. What remains
is a tail of scalability and ergonomics work: rendering stays eager once data
arrives, request transport has no cancellation, `/switch` still pages oldest
targets first, collapsed tool groups can hide failures, and provider prompt-cache
behavior is only asserted at request-shape granularity rather than end-to-end.

This doc tracks only that unfinished work. The "why" behind the durable model and
the wire contract lives in [design decisions](../design-decisions.md); do not
re-litigate it here.

## What exists today

The shipped baseline is documented elsewhere; this plan builds on it.

- Turn cards, lazy turn detail, selected-session cache, incremental append
  patching, scroll-position persistence, and the three-mode collapsible
  `ToolRunGroup`: see [web UI](../../../packages/web/docs/web-ui.md).
- `transcript.turns` / `transcript.turn_detail` bounded paging, UI transcript
  projection without raw `provider_replay`, active-branch reads, and metadata-only
  `session.get`: see [agent-store](../modules/agent-store.md) and
  [websocket-rpc](../websocket-rpc.md).
- Provider-owned prompt-cache shaping (OpenAI stable `instructions` + cohort key,
  Anthropic `cache_control` breakpoints) with request-body unit tests in
  `agent-provider` and `usage` cache metrics persisted on model action results.

```
selected-session hot path (shipped)
  session.get (metadata only)
        │
        ▼
  transcript.turns ──► newest tail page of collapsed cards
        │                     │
   append events        "Show details" / "Load older turns"
        │                     ▼
        ▼              transcript.turn_detail (one card)
  live card summary
```

## Proposed work

### 1. Transcript rendering virtualization

What exists: the selected-session view fetches a bounded tail of turn cards and
defers per-turn detail and per-tool bodies until expanded
([web UI](../../../packages/web/docs/web-ui.md)). Collapsed groups never build their
hidden item bodies.

Gap: once a large turn is expanded, or the full-tree history picker is open, the
list still mounts every display node. There is no row virtualization library in
`packages/web`; `MessageList` maps all nodes to DOM.

Proposed change:

- Virtualize the expanded transcript and the full-tree history picker with a
  variable-height scroller (evaluate `react-virtuoso` first for chat-style
  follow-output behavior).
- Preserve the existing per-session scroll-position cache and sticky-to-bottom
  behavior; adapt them to virtualizer offsets rather than raw `scrollTop`.
- Keep markdown / provider-arg parse memoization scoped per entry id so virtual
  remounts do not re-parse unchanged rows.

### 2. Request cancellation and transport isolation

What exists: the client already drops stale responses via `selectedRef` and
request-generation guards (selecting A → B → A cannot apply the first A response),
and large UI payloads were cut by the turn-card projection.

Gap: a single websocket multiplexes control RPCs, large fetches, and the event
stream. A big in-flight fetch can still delay a small interaction, and the client
cannot abort daemon-side work it no longer needs.

Proposed change (do only if measurement still shows head-of-line blocking after
virtualization):

- Add request ids with an explicit cancel message, or split control/metadata
  traffic from bulk transcript fetches onto a second connection.
- Optionally move bulk export / full-tree fetches to compressed HTTP instead of
  the websocket.

### 3. Newest-first `/switch` targets

What exists: `/switch` and `/fork` page daemon-projected user-message targets via
`history.targets` newest first. Each target includes its safe preceding boundary,
and restore text comes from the full entry body rather than the truncated preview.

The previous oldest-first `transcript.index` picker blocked on loading the full
topology before recent messages became available.

Landed behavior:

- Add a `history.targets(session_id, limit, before_sequence?)` RPC returning the
  newest switch/edit targets first. Each target carries `restore_entry_id`,
  display-only preview text, and the validation data `history.switch` needs
  (`expected_active_leaf_id`, `transcript_revision`, boundary/edit validity). No
  raw provider replay on the wire.
- `/switch` fetches the first newest page and pages older targets on demand.
- `history.switch` stays authoritative for switchability; restore text continues
  to fetch the full user-message body, never the preview.

### 4. Tool-collapse correctness and ergonomics

What exists: `ToolRunGroup` is a three-mode card (`collapsed` / `recent` / `all`).
Live or running groups default to `recent`; completed groups default to
`collapsed`. A user override is stashed so streaming churn does not reset the
chosen mode. Edits render as a diff summary, not raw JSON.

Gaps (all confirmed unimplemented):

- **Failures are hidden when collapsed.** `defaultToolGroupMode` collapses any
  completed group regardless of contents, and the collapsed mode hides every
  item including errors. Failed, interrupted, and crashed tool calls should never
  be auto-hidden inside a collapsed group; they should stay visible (or force the
  group open) even after the turn completes.
- **Expand/collapse state is not persisted.** The mode override is component-local
  `useState` and is lost on remount/session switch, unlike scroll position which
  persists to `localStorage`. Persist per-session group expand/collapse the same
  way scroll position is persisted.
- **No "one expanded group at a time".** Each group owns its override
  independently, so opening one does not collapse the previously expanded group.
  Prefer a single expanded group within the current turn, with optional pinning
  to keep multiple open.

### 5. Daemon-level prompt-cache verification

What exists: `agent-provider` has request-body unit tests asserting the OpenAI and
Anthropic request shapes are cache-friendly (stable prefix, sorted tools, cache
breakpoints), and provider `usage` cache fields are parsed and persisted.

Gap: nothing asserts cache *hits* across two real requests. The `agent-daemon`
crate has no `tests/` directory and no integration coverage that drives the
provider runtime twice in one session and checks reported cache tokens.

Proposed change:

- Add daemon integration tests that send two similar requests through the daemon
  in one session and assert:
  - OpenAI: the second response reports non-zero `cached_tokens` once the prompt
    clears the minimum cacheable size.
  - Anthropic: the first request reports `cache_creation_input_tokens` and the
    second reports `cache_read_input_tokens`.
- Run a multi-tool turn and confirm Anthropic thinking/replay blocks are
  preserved while cache metrics stay sensible.
- Run switch/compact after cached turns and confirm no cache metadata leaks into
  the visible (UI-projected) transcript tree.

## Open questions

- Virtualization library choice and whether the history picker needs its own
  virtualized path separate from the message list.
- Whether transport isolation is worth the complexity, or whether virtualization
  plus the turn-card projection already keep the single socket responsive.
- Whether `history.targets` should be a new RPC or fold target derivation into an
  existing endpoint; and whether persisted target pages need revision fencing
  like the compact index.
- Where persisted tool-group expand/collapse state should live (per-session
  `localStorage` keyed by group id, mirroring scroll positions) and whether
  "one group open" should be a hard rule or a default with pinning.
- Whether daemon prompt-cache integration tests run against live provider
  credentials or a recorded transport; live calls make the suite credential- and
  network-dependent.
