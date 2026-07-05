# Web UI

The React/Vite client in `packages/web`. It talks to the `pi-agentd` daemon over a single websocket
([websocket-rpc](../../../rust/docs/websocket-rpc.md)) and renders a session's transcript turn-by-turn.
See the [Rust Agent Stack](../../../rust/docs/architecture.md) overview and [design decisions](../../../rust/docs/design-decisions.md)
for the runtime it drives, and [agent-daemon](../../../rust/docs/modules/agent-daemon.md) for the RPC server.

The UI is operational, not marketing-shaped: a dense three-pane layout, compact rows, small controls,
and transcript-first interaction.

```
+----------------+----------------------------------+--------------+
| Sidebar        | Chat pane                        | Inspector    |
| projects +     | header (model/effort/title)      | global cfg   |
| session list   | transcript (turn cards)          | session head |
| activity counts| ----------------------------------| pending      |
|                | composer + queue pane + slash    | actions/tools|
+----------------+----------------------------------+--------------+
```

Panels collapse responsively: `wide` shows all three, `medium` drops the sidebar to an overlay,
`compact` overlays both side panels. The mobile top bar exposes drawer toggles and a connection pill.

## Responsibilities

- Own the websocket connection, reconnection, and event fan-out (`rpc.ts`), with a typed RPC facade (`agentApi.ts`).
- Keep project and session-list server state in TanStack Query.
- Keep the *selected* session's head, queue, active branch, compact transcript tree, and turn cards/details in a
  separate normalized per-session cache.
- Render the active branch as collapsible turn cards, fetching full entry detail lazily.
- Compose and submit input, manage the queued-follow-up pane, and expose slash commands.

## Data layer

Two distinct stores, split by access pattern.

```
TanStack Query                          SelectedSessionCache (per sessionId)
------------------------------          ------------------------------------
projects        queryKeys.projects      snapshot (head/revisions/queue/metadata)
session lists   queryKeys.sessions(pid) activeBranchEntryIds + entriesById
tools           queryKeys.tools(kind)   tree* (compact topology for /switch)
system prompt   queryKeys.systemPrompt  turnCardsById/turnOrder/turnDetailsById
```

### TanStack Query owns server lists

Projects, per-project session summaries, the provider tool list, and the system prompt are server lists owned by
TanStack Query (keys in `queryKeys.ts`). They are refreshed by invalidation, not bespoke caching. Session-list
refreshes after events are debounced/coalesced.

Metadata-only operations patch the cached list in place instead of triggering a transcript reload. `sessionQueryCache.ts`
provides `patchSessionList*` helpers; rename, archive/unarchive, and provider change call the RPC, then patch the
cached `SessionSummary` (and the selected snapshot) directly. The list query is still invalidated afterward as a
reconciliation fallback, but the visible row updates immediately. Activity transitions from events also patch the
list row before the debounced invalidation fires.

### SelectedSessionCache owns the selected session

`useSelectedSessionStore` (`selectedSessionStore.ts`) holds an in-memory `Map<sessionId, SelectedSessionCache>` plus a
current pointer. `replace`/`reset`/`update` keep React state and a synchronous `cacheRef` in lockstep so async flows
read the latest cache without waiting for a render. `reset(sessionId)` re-points at a cached entry if one exists,
otherwise an empty cache; it does not evict other sessions. Switching away and back to a session reuses its cached
tree/bodies/cards, so revisits feel instant. `drop(sessionId)` evicts a deleted session.

The cache (`selectedSessionCache/types.ts`) is normalized:

- `snapshot` — head: revisions, queue, activity, metadata, `last_event_id`, `server_time_ms`.
- `activeBranchEntryIds` + `entriesById` — render order plus a body map. Reducers preserve object identity for
  unchanged entries (`mergeEntryBodies`) so transcript rows and scroll position stay stable across refreshes.
- `tree*` — compact transcript topology (`TranscriptTreeNode`) for the `/switch` picker, paged by `sequence` and
  fenced by `transcript_revision`.
- `turnCardsById` / `turnOrder` / `turnDetailsById` — turn cards in order and lazily-loaded per-turn detail entry id lists.

Reducers live in `selectedSessionCache.ts` and `selectedSessionCache/{entries,turns}.ts`. They are pure functions over
the cache; every reducer no-ops if `cache.sessionId` does not match the incoming `session_id`, so late responses for a
deselected session are ignored.

### Revisions drive convergence

The daemon stamps `session_revision`, `queue_revision`, and `transcript_revision` on snapshots, queue projections, and
events. These are the freshness tokens; the cache uses them to decide whether to apply an incremental update or refetch:

- Queue projections replace queue state only when their `queue_revision` is newer.
- A changed `transcript_revision` on a snapshot/turns page invalidates the cached tree/turn state for that session.
- Compact-index pages are accepted only when they match the loaded `transcript_revision` and start exactly at the
  loaded prefix sequence; otherwise the reducer restarts from `after_sequence = 0`.
- `last_event_id` is treated as a transient replay cursor only, never as a durable freshness signal — the daemon may
  clear old event rows after a session goes idle, so a fresh `session.get` can legitimately report a smaller cursor.

### Modular RPCs and hot paths

Selected-session loads avoid full active-branch bodies. The hot path is metadata `session.get` (no entries) plus a
bounded `transcript.turns` tail page.

| Flow | RPC(s) | Notes |
| --- | --- | --- |
| Cold open / refresh | `session.get` (no entries) + `transcript.turns` | one bounded tail page of cards |
| Load older turns | `transcript.turns` with `before_entry_id` | prepend-paged on demand |
| Expand a turn | `transcript.turn_detail` | one card's entries, by card id + leaf/sequence bounds |
| Foreground/focus reconcile | `session.sync_active_branch` | suffix-only sync from the cached base leaf |
| `/switch` picker | `transcript.index` pages | compact topology only |
| Switch target | `history.switch` | revision-fenced; returns branch ids + sparse missing bodies |
| Restore user message | `transcript.entries` | full body for the picked message, only if missing locally |

`session.sync_active_branch` returns `unchanged` / `extended` / `branch_changed`. The cache merges overview metadata on
`unchanged`, appends the suffix on `extended`, and falls back to a full `session.get` on `branch_changed` (or when the
returned suffix does not extend the cached leaf).

Provider replay is never on the wire: UI transcript projections omit `provider_replay` entirely, and the frontend
`TranscriptEntry` type has no such field. The UI renders only semantic `TranscriptItem`s.

## Turn-oriented transcript

The chat pane renders the active branch as turn cards (`transcript.tsx` / `selectedSessionCache/turns.ts`), not a flat
entry stream.

- Historical completed turns collapse to a summary card: the turn's user messages plus the final assistant message and
  a "Worked for …" duration. Intermediate tool calls/results are omitted until expanded.
- "Show details" fetches `transcript.turn_detail` for that card and renders the full detailed rows; "Hide details"
  collapses again. "Load older turns" pages older cards.
- The current/running turn auto-expands and auto-loads its detail so progress streams live. A single "Working… {elapsed}"
  row trails the transcript. Its clock anchors only to durable server data — the active branch's `turn_started` entry,
  or a mid-turn `compaction_summary` that remembers the original turn start — and the anchor is rebuilt only when
  `startMs` changes, so streaming entries do not reset the elapsed timer.
- Transcript-append events incrementally update the current card and any already-expanded detail (`appendTurnCard`,
  `appendLoadedTurnDetail`); a canonical `transcript.turns` refresh runs when the cache cannot prove an event extends
  the selected branch. When a card's stable id changes, expanded detail migrates with it (`migrateCurrentTurnDetailId`).
- Compaction renders as a typed summary row, not a transcript replacement. The marker can hide/show the prior entries
  in its segment.
- Crashed or interrupted terminal turns expose a Continue/Retry action inline that calls `turn.resume`.
- Turn-start, graceful turn-finish, and tool-call-start bookkeeping entries are not rendered as messages.
- Turn-jump controls page between turn anchors; scroll position is sticky-to-bottom and persisted per session in
  `localStorage`.

### Tool calls render as collapsible groups

Consecutive assistant tool activity is grouped into a `ToolRunGroup`. Each group is a three-mode card:

- `collapsed` — hides every item (default for a finished group).
- `recent` — shows the last 3 items with a link to expand (default while the group is live/running).
- `all` — shows every item in a capped scrolling list with a link to shrink back.

The default tracks liveness (working → `recent`, done → `collapsed`); once the user toggles a group, an override is
stashed so later status churn or streaming items do not blow away their selection. A single tool renders as a stand-alone
row. Tool results fold into their matching call row rather than appearing as separate raw events. Edit-shaped calls
render an "Edit …" header with a diff-style preview. Display names map the builtin tools (`Edit`, `Bash`, `Grep`,
`Web search`, `Web fetch`); see [agent-tools](../../../rust/docs/modules/agent-tools.md).

## Events and reconciliation

`rpc.ts` parses each frame as either an RPC response (`ok` field present) or an event, then fans events out to handlers.
`sessionEvents.ts` classifies each event into a refresh plan:

- `refreshList` — debounced session-list invalidation (most lifecycle/queue/turn events).
- `syncSelected` — schedule a selected-session reconciliation, but only for events whose canonical projection is not
  otherwise mergeable (idle, recovery, config, history, compaction transitions, and any unknown event).

For the selected session the handler applies as much as it can locally before falling back: queue projections
(`applyQueueProjection`), `transcript.appended` entries (`applyTranscriptAppendedEvent`), and activity hints all merge
into the cache; side-channel events (`turn.started`, `turn.finished`, `assistant.message`) only advance the event
high-water when their entry is already known. Overlapping selected refreshes are coalesced per session. Returning to the
foreground (`visibilitychange` / `focus` / bfcache `pageshow`) invalidates the session list and runs one throttled
active-branch sync.

The app subscribes to events for every visible session via `events.subscribe`, replaying missed events from the stored
`last_event_id`, and unsubscribes from sessions that leave the list.

## Composer, queue, and slash commands

The composer (`composer.tsx`) routes ordinary text according to the selected
transcript:

- no selection calls `session.start`;
- a selected top-level/root session calls `input.follow_up`;
- a selected subagent calls the parent-scoped
  `delegation.steer_subagent`, using the loaded snapshot's
  `parent_session_id`, its `session_id`, and the submitted text.

The placeholder identifies the selected mode ("Follow up" or "Steer this
subagent"). Cmd/Ctrl+Enter sends; Enter inserts a newline. There is no local
"message queued" or pending-bubble shadow row — transcript rows render only
from canonical daemon projections; the send button spinner is the only local
in-flight indication. Successful follow-ups and steers are silent, matching the
ordinary-send UX.

Submit routing lives in `composerRouting.ts`, with pure routing tests and App
wiring that looks up the cache by the captured session id. There is no browser
integration harness for this path. The helper trims the message once and checks
for a leading slash first, so slash commands remain commands in every
transcript. At submit time `Composer` captures an immutable session id, a
`client_control_id`, and a proposed new-session id. App routing, draft
restoration, and RPC dispatch use those captured values rather than rereading
the current selection. App trusts only a loaded snapshot whose `session_id`
exactly matches the submitted id. If that snapshot is no longer available,
submission fails through the normal notice path and restores the text under the
captured session's draft key. It never redirects text to a newer selection.

The daemon's canonical creation model makes this routing unambiguous:
top-level `session.start` stores no `parent_session_id`, while the delegation
spawn path stores `parent_session_id`, `subagent_type`, and `delegation_id`
together. Thus a daemon-created snapshot with `parent_session_id` is a
subagent. The UI uses only that direct relationship; it does not infer children
from roles, ID prefixes, or a delegation-list page. The scoped daemon RPC still
revalidates all three child fields, delegation membership/status, and live work
state. For backward compatibility, a raw ordinary-priority `input.follow_up`
can still target a child. It is not used by the selected-child composer and
does not perform parent/delegation control validation. Raw
`priority = "steer"` input to a child remains rejected; parent-scoped steering
must use `delegation.steer_subagent`.

RPC rejection (terminal child, non-running child, wrong membership, or a
completion race) is shown by the normal error notice and makes
`submitComposer` return `false`. `Composer` then restores the submitted text
through its existing per-session/version-guarded draft path. If selection
changes while an accepted request is in flight, the captured child still owns
the request and draft resolution; stale cache reducers no-op, and a failure is
stored back under that child's draft key rather than appearing in the new
selection.

### Subagent steer and interrupt lifecycle

Textbox steering is an additional producer for the existing delegation
mailbox, not a replacement for parent/model control. The model-facing
`steer_subagent` tool and web `delegation.steer_subagent` RPC both call the same
daemon `steer_subagent_core`, which writes through
`enqueue_scoped_subagent_steer`. Every accepted instruction is one durable
`queued_inputs` row on the child with `priority = steer`; there is no
browser-specific or producer-specific queue. Concurrent parent/model and user
steers serialize on the same delegation/child database locks. Both may be
accepted with distinct `input_id`s, and the mailbox processes them in canonical
steer order (promotion/creation time and ID tie-break), ahead of follow-ups;
neither producer receives extra priority.

A successful response includes `accepted: true`, `input_id`, current `queued`
state, `replayed`, durable `phase`, `interrupted`, `interrupt_outcome`, and
`drive_status`. It acknowledges a committed mailbox row, not necessarily an
applied interrupt or consumed message. `interrupted` is `null` while an
interrupt remains `pending_interrupt`; it becomes truthful once the durable
phase advances. If immediate reconciliation or driving fails after commit, the
RPC still reports acceptance with `drive_status: "failed"` and `drive_error`,
and the daemon records a model-error event. It does not turn a durable
acceptance into an apparent rejection.

Completion and scoped controls take the delegation lock in the same order and
recheck terminal/readiness state after locking. If completion or cancellation
wins first, a new control is rejected. If a scoped steer commits first, the
active mailbox row prevents completion until it is consumed or cancelled.
Distinct concurrent producer messages have distinct rows and cannot overwrite
one another.

Steer and interrupt have three deliberately separate forms:

- **Steer** does not abort an in-flight model or tool (including a long polling
  loop). It waits durably and is injected at the next continuation point; tool
  completion explicitly checks the steer mailbox before the next model step.
  Omitting `interrupt` (or passing `false`) preserves the original
  `steer_subagent(subagent_id, message)` behavior.
- **Interrupt and steer** passes `interrupt: true`. The committed row starts in
  `pending_interrupt` and blocks generic consumption of every row in that
  child's mailbox. Under the exact-child `SessionDriver`, reconciliation
  atomically persists the interrupted transcript boundary, interrupts the
  complete captured set of unfinished attempts for the active turn, and
  advances the row to `interrupt_applied`.
  It then aborts only that child's registered runtime tasks, advances the row
  to `ready`, and allows ordinary mailbox driving to consume the steer. The
  instruction is durable before the old work becomes terminal-visible.
- **Interrupt only** is available to the parent model as
  `interrupt_subagent(subagent_id)`. It uses a distinct durable
  `scoped_subagent_interrupt` ledger row with the same phases and generation
  fence. Its ready row settles without enqueueing or consuming text.

The selected transcript's Stop button calls `input.interrupt` with the captured
selected session id. Stopping a parent interrupts only that parent; stopping a
child interrupts only that exact child, not its parent, siblings, or delegation.
Whole-delegation cancellation remains the separate run-board
`delegation.cancel`/model `cancel_delegation` operation. Its status transition
atomically cancels active child mailbox rows (including pending combined
controls), then exact-child runtime cancellation interrupts remaining child
work. A cancelled control reports phase `cancelled`; cancellation does not roll
back external tool side effects.

Detached exact-child workers, a periodic live sweep, and a boot sweep recover
`pending_interrupt` and `interrupt_applied` rows independently of the parent
tool/RPC future. A crash after `interrupt_applied` resumes task settlement and
does not append another interrupted boundary. Each control is fenced to the
active leaf, active turn, and complete deterministic set of unfinished attempts
captured at enqueue. A completed parallel sibling does not hide remaining
captured work; an unfinished attempt outside that set means the generation
advanced, so reconciliation records
`interrupt_outcome: "generation_advanced"`, reports `interrupted: false`, and
never blindly interrupts the newer turn.

The web retains one stable control/input id while unchanged text is restored
after an uncertain response; a deliberate edit or a definite success clears or
replaces it. New-session submissions also retain the proposed session id, so a
retry targets the same `session.start`; the daemon treats an existing requested
session id as a replay. Model-facing controls derive their id from the durable
tool-call id. A matching steer retry does not enqueue text twice; a matching
interrupt-only retry returns its prior durable state and cannot stop a newer
generation. This is practical ledger idempotency, not an exactly-once guarantee:
a deliberate new submission/control creates a new id, a response can still be
lost after commit, and external tool or network side effects remain
non-transactional and cannot be rolled back.

### Workspace scope picker

When composing a new session inside a project (no session selected), a collapsible `WorkspaceScopePicker`
(`workspaceScopePicker.tsx`) sits above the composer. It scopes the next session to a subset of the project's workspaces
and lets git workspaces start from a non-default branch, then feeds `session.start`'s optional `workspaces` array via
`startWorkspacesFromScope`. It defaults to every workspace included at its default branch (sending no `workspaces`, i.e.
the daemon's existing behavior), so the type-and-send flow is unchanged. Per-project choices persist in `localStorage`
under `piRelayWorkspaceScope:v1` (`workspaceScope.ts`); stored entries for workspaces a project no longer declares are
dropped when the picker re-derives.

### Queue pane

When follow-ups are queued, a pane above the composer (`QueuedInputPane`) lists them with row-level controls:

- promote a follow-up to steer (`input.promote_queued`),
- edit a queued follow-up's text (`input.update_queued`),
- cancel it (`input.cancel_queued`),
- reorder follow-ups up/down (`input.reorder_queued_follow_ups`, sending the full ordered follow-up id list).

Each mutation passes the cached `queue_revision` as an optimistic fence and replaces queue state from the returned
canonical `queue` projection. Steering rows are immutable: they stay above follow-ups and expose only disabled controls.
Queue events apply immediately so a stale row that the daemon already consumed disappears fast; a "no longer editable"
promote/edit is a benign no-op, not an error.

### Per-session composer drafts

Composer text is persisted per session in `localStorage` under `piRelayComposerDrafts:v1`, keyed by `session_id`
(new-session text uses a fixed key). Switching sessions swaps the visible draft; a failed send restores the typed text.
Submission IDs are retained in memory with the pending draft for an unchanged
retry, but are not persisted across a full page reload.
There are no browser-local *session* drafts — only Postgres-backed sessions appear in the sidebar, and starting a new
chat is purely composer state. UI selection (`piRelayUiResume:v1`) and transcript scroll position are the other
`localStorage`-backed UI state.

### Slash commands

Slash commands (`slash.ts`) are thin wrappers over RPCs that lack a dedicated control. Autocomplete is shallow: it shows
only while typing the command name; Enter on a partial accepts the highlighted completion and adds a trailing space,
Enter on an exact command submits.

| Command | Action |
| --- | --- |
| `/switch` | Opens the same-session history picker (idle only). User-message targets restore the full original message into the composer; turn/compaction targets just become the active leaf. |
| `/compact` | Requests context compaction (`compaction.request`). |
| `/system` | Shows the selected session's rendered `PI.md` prompt and source template (`system.prompt`). Requires a durable session. |
| `/export` | Exports the current branch's assistant/user messages, fetching active-branch bodies for the export view. |

Switch never accepts raw transcript ids; the picker is the only path. The picker shows a loading state until the compact
tree is complete (or a fresh complete tree is cached), and `history.switch` revalidates the expected leaf and
transcript revision server-side, refreshing the picker if history changed underneath.

## Model and reasoning controls

The chat header exposes a model picker and a provider-specific reasoning-effort picker (`sessionDefaults.ts`). OpenAI
offers `gpt-5.6-sol` (default), `gpt-5.6-terra`, and `gpt-5.6-luna`; Claude offers Sonnet 5, Opus 4.8, and Fable 5.
Sonnet 5 is the normal Claude default at `high` effort. Fable 5 is listed last as an explicit opt-in, and its option text
and tooltip state Anthropic's required 30-day retention and lack of Zero Data Retention. The provider/model is locked
once the session has any transcript history, because both providers carry provider-shaped replay state across turns.
Reasoning effort is a per-request knob and can change during or between turns (applying to subsequently created
requests). Effort options differ by provider/model: OpenAI generally offers `none…xhigh`, `gpt-5.6-sol` also offers
`max`, and these Claude models offer `low…max`. Changing model/effort calls `session.configure` and patches the cached
list/snapshot. The list is a seeded offline-safe fallback; the daemon's cached Anthropic Models API metadata remains
authoritative for runtime limits and capabilities without adding transient model records to the database.

## Notes

- Every cache reducer is keyed by `session_id` and no-ops on mismatch; stale async responses for a deselected session are
  safely ignored.
- Compact topology from events is conservative: an event `tree_node` extends the tree only when it is already complete;
  otherwise topology recovery is left to `transcript.index` so `/switch` stays correct without re-deriving Rust
  turn-boundary logic in TypeScript.
- The selected cache is tab-lifetime only; there is no IndexedDB or persistent transcript cache.
- `/switch` previews and compact `display_hint` text may be truncated; mutation/restore content always comes from full
  entry bodies, never the preview.
- Thinking blocks never reach the UI — they are discarded at the provider parse layer, so `AssistantItem` is only
  `text` or `tool_call`. See [agent-provider](../../../rust/docs/modules/agent-provider.md).
