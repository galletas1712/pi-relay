# Design Decisions

This document records the product-facing and invisible engineering choices made
while reducing the Rust rewrite toward a small personal agent runtime.

## Visible Surface

### Sessions Are The Product Unit

The frontend exposes sessions only. There is no task abstraction, project queue,
or explicit open/close/resume/delete lifecycle. A session is idle when it has no
unfinished turn work, queued when durable input is waiting, and running when the
daemon has unfinished model/tool/compaction work.

The UI therefore treats "resume" as selecting a session and reading durable
history. There is no separate resume button because idle sessions are already
recoverable from storage.

### Bigband-Inspired Layout

The TypeScript UI in `packages/web` borrows the dense Bigband shape:

- left sidebar for sessions and activity counts
- central transcript log for the selected session
- bottom composer for messages and slash commands
- right inspector for global config, selected-session state, pending actions,
  tools, and command affordances

The design keeps the app operational instead of marketing-like: compact rows,
small controls, stable panes, low-decoration styling, and transcript-first
interaction.

### Draft Sessions Are Browser State

Clicking New session or running `/new` creates a browser-local draft session,
not a Postgres row. The draft row stores a stable future `session_id`, title,
provider, composer text, and timestamps in `localStorage`; this is enough for a
brand-new unsent draft to survive refresh without polluting durable agent
state.

Sending the first normal message from that draft calls `session.start`, which
creates the durable session and immediately materializes the first user input.
After the backend accepts it, the UI removes the local draft and selects the
durable session. Empty web-created durable rows are also hidden defensively in
`session.list` unless they have transcript, queued input, actions, or fork
provenance. `metadata.hidden = true` is a separate list-filtering convention
for local verification cleanup; it is not a lifecycle state and does not delete
or mutate transcript history.

Composer drafts for existing sessions are also web-owned `localStorage` state.
They are used for restored historical user messages after rewind or fork, and
are deliberately not stored in `sessions.metadata` or transcript rows.

### Slash Commands Are Thin RPC Calls

Slash commands exist to expose real websocket operations without adding a second
frontend command model.

- `/new [title]` creates a local draft session.
- `/refresh` reloads sessions, tools, global config, and the current transcript.
- `/status` reads the selected session snapshot.
- `/rewind` opens a picker of visible rewind targets. User-message targets
  rewind to the previous safe boundary and restore that message into the
  composer for editing. The picker does not expose a bare root option; the
  first user message is the visible way to rewind to root with editable text.
- `/fork [title]` opens a picker of visible fork targets and can prefill the
  fork title. User-message targets fork from before the message and restore
  that message into the child composer.
- `/compact` requests context compaction.
- `/context [entry-id]` inspects materialized model context.
- `/tree` reloads and summarizes the transcript tree.
- `/system [clear|prompt...]` reads or writes the global daemon system prompt.
- `/provider [kind model]` reads or updates the selected session provider.
- `/tools` lists daemon tools.

Normal composer text is always `input.follow_up`, even while the agent is
running. Queued follow-ups appear in a small pane above the composer. Each
queued follow-up has a row-level steer control; pressing it promotes that row
to the steer queue, ordered by promotion time. Active turns are interrupted with
a stop button beside the composer, not with a slash command.

Slash autocomplete is intentionally shallow: it only appears while typing the
command name. Enter on a partial command accepts the highlighted completion and
adds a trailing space; the next Enter submits. Enter on an exact command submits
immediately. This keeps discovery without stealing execution.
The web UI does not render a generic "message queued" notice after ordinary
sends. That acknowledgement only means Postgres accepted a durable queued input;
it is not a transcript event and was visually misleading while the agent was
still running. Errors still surface, while real conversation progress comes
from transcript entries and activity/tool state.

Rewind and fork do not accept raw transcript ids in the web UI. The picker is
the only user-facing path, so history mutations are chosen from visible turn
context rather than opaque storage identifiers.

Turn-start, graceful turn-finish, and tool-call-start bookkeeping entries are
not rendered as transcript messages. Assistant tool calls render as compact
collapsible rows inspired by Bigband and Claude Relay, with the matching result
folded into the tool row instead of appearing as a separate raw event.

The central transcript renders only the active root-to-leaf branch. Rewind does
not delete abandoned rows from Postgres, but those off-branch rows disappear
from the main conversation view. They remain available to the history tree and
fork picker.

### Global System Prompt

The system prompt is daemon-global configuration, not per-session state. The UI
shows it in the inspector and edits it through `/system`, which calls
`config.get` and `config.set`.

Sessions keep provider and metadata only. This avoids hidden prompt drift across
forks and makes the daemon's behavior easier to reason about for personal use.

### No Approval UI

The runtime always allows tool actions. There is no approval state, approval
modal, or await-approval lifecycle. Tool failures are represented as tool
results and action rows, not as user decisions.

## Invisible Runtime Choices

### Daemon Modules Have Narrow Jobs

The daemon is split by responsibility instead of by RPC method family. RPC
handlers stay in `main.rs` because they are protocol glue: validate params,
call the repository/runtime, and shape JSON responses. The pieces that are easy
to accidentally couple to everything else now live elsewhere:

- credential loading and Codex refresh in `auth.rs`
- JSON-to-vocabulary parsing and transcript recovery helpers in `codec.rs`
- provider selection and request execution in `provider_runtime.rs`
- live session lifecycle, pump locking, queued-input materialization, dispatch,
  and event publication in `runtime.rs`
- process-local daemon state in `state.rs`
- all Postgres SQL and transaction boundaries in
  `agent-store::PostgresAgentStore`

`types.rs` is intentionally daemon-local rather than a new public crate. These
are not core domain concepts; they are the daemon's internal protocol structs
and websocket errors. Provider/session persistence contracts live in
`agent-store` now because Postgres is the only durable backend.

### Postgres Store Is The Storage Crate

The old `agent-store` memory/JSONL layer was removed. `agent-store` now means
the concrete Postgres store for websocket sessions: durable sessions,
transcript rows, queued input ledger, actions, events, and global daemon config.
`agent-session` owns `StoredSession` and `StoredTranscriptEntry` because those
are live-session snapshot shapes, not a standalone storage backend.

There is deliberately no repository trait yet. A trait before a second real
backend would force the Postgres model through an imagined abstraction and make
the transaction boundaries harder to read.

The store uses SQLx with a `PgPool` and explicit SQLx transactions. SQL remains
visible because Postgres SQL is the clearest language for the JSONB-heavy
ledger and recovery operations, but the driver, pooling, binding, and row
decoding now live on the maintained SQLx stack. Diesel and SeaQuery are also
maintained, but they add more query-builder/ORM surface than this small
transaction-oriented store currently needs.

### Wire Vocabulary Is Typed At The Boundary

Small closed vocabularies are Rust enums instead of ad hoc strings in control
flow. `agent-store` owns persistence-facing enums for provider kind, input
priority, queued-input status, action kind, action status, session activity,
and event type. They serialize to the same Postgres/websocket strings as
before, so the wire contract stays stable while invalid database values fail
at decode time.

`agent-daemon` also parses websocket method names and fork placement into
daemon-local enums before dispatching. JSON content blocks, image sources, and
assistant items use the serde-tagged vocabulary types from `agent-vocab`
instead of hand-matching `"type"` and `"kind"` strings in the codec.

Provider request bodies still contain provider-specific string fields such as
OpenAI/Anthropic `role` and `model`; those are external API wire values rather
than internal lifecycle vocabulary.

### Postgres Is Authoritative

For websocket sessions, Postgres is the source of truth. The daemon may hold an
`AgentSession` while work is active, but accepted transitions are committed to
Postgres before follow-on provider/tool/compaction work is dispatched.

If a Postgres commit fails after the live session has advanced, the daemon
evicts that live session. The next interaction reloads from durable state rather
than trusting in-memory state that may be ahead of storage.

### No Durable RAM Staging Layer

The frontend does not keep an authoritative transcript cache. It subscribes to
events and refreshes `session.get` plus `history.tree` from the daemon. The
daemon similarly avoids a second durable staging model in RAM; active memory is
only a work-in-progress projection.

This keeps the Postgres-only direction clean: storage changes should not leak
into frontend state semantics.

### Idle Input Skips The Queue, Busy Input Stays Durable

When a session is idle, `input.follow_up`, `input.steer`, and `session.start`
feed the message directly into the session and persist the resulting
transcript/actions/events before dispatching follow-on work. The same
transaction records a consumed input ledger row when a `client_input_id` is
present. That keeps the common path small without losing retry idempotency:
there is no artificial queued state for a message that can be acted on
immediately.

When a model/tool/compaction action is unfinished, composer sends remain
`queued_inputs.status='queued'` follow-ups by default. Promoting a row changes
its priority to `steer` and records `origin.promoted_at`; the queue consumes
steers first in promotion order, then remaining follow-ups in creation order.
Before the daemon materializes a queued row, it claims the row as `consuming`;
queued edit/cancel/promote RPCs only work before that claim. The daemon only
marks a claimed input `consumed` in the transaction that also appends the
corresponding transcript and action events.

That choice prevents a daemon-death gap where accepted user input has been moved
into an in-memory mailbox but has not yet appeared in transcript history.

### Input Idempotency Is Event-Idempotent Too

For busy-session queued input, `client_input_id` is unique per session.
Retrying the same input id returns the original queued row id and does not emit
a second `input.queued` event. For idle accepted input, retrying the same id
finds the consumed ledger row and does not append another user message.

The web composer keeps the same generated `client_input_id` while a send is
unconfirmed, so a lost websocket response can be retried without duplicating the
user message.

For brand-new draft starts, the browser stores a stable future `session_id` on
the draft. Retrying `session.start` with that id returns the existing durable
session instead of creating another one.

### Fork, Rewind, And History Targets

Rewind operates on committed transcript boundaries or root. The UI presents
rewind targets as visible user messages and completed turns, then maps them to
the boundary-only backend operation. Forking is broader: it can branch from any
existing transcript entry because it creates a new session and does not mutate
the source branch. Forking from nothing is rejected. When the selected fork
point is not already a boundary, the child session closes that copied partial
tail as `Interrupted`, which keeps the child runnable without pretending daemon
recovery crashed.

Picker actions carry expected active-leaf information for source-mutating
rewind and for sending restored composer drafts. If the session moved since the
visible choice was made, the daemon returns `history_changed` and the UI
refreshes instead of applying the edit to a different branch.

Rewind mutates the selected session's active branch, so it remains idle-only.
For websocket RPC, idle means no unfinished action and no queued input waiting
to become transcript. In the normal user flow, that is the point after a turn
has finished and the queue has drained. If a user wants to rewind during a turn,
they should interrupt first, then rewind after the interrupted tail has been
committed.

### Recovery Keeps Transcript Semantics

Daemon recovery checks for unfinished action rows before first use of an idle-
looking session. If previous work died mid-turn, recovery marks unfinished
actions stale and appends a crashed turn tail so the transcript remains
structurally valid.

Interrupted and crashed states are transcript outcomes, not broad session
lifecycle states. The session activity enum stays small: `idle`, `queued`, and
`running`.

### Provider Scope Is Intentionally Small

`agent-provider` targets OpenAI/Codex and Anthropic/Claude. The daemon reads
Codex credentials from `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`, including
the ChatGPT/Codex account id when present. OpenAI API-key use still goes through
Chat Completions; Codex credential use goes through streamed Responses.

Provider config supports `prompt_cache.key`, which the daemon forwards on the
OpenAI request path. Chat Completions also hardcodes the low-variance request
policy we want for personal use: `parallel_tool_calls = true`,
`service_tier = "priority"`, `store = false`, and
`prompt_cache_retention = "24h"`. If no explicit `prompt_cache.key` is provided,
the Chat Completions path uses a stable pi-relay default key so repeated local
sessions still route toward the same prompt cache. The actual system prompt
remains global.

Provider requests now carry `PromptSections`: a stable prefix and dynamic
context. The stable prefix is the global system prompt. The daemon appends
dynamic runtime context after it, currently the workspace cwd, and only then
does the provider render transcript history. Chat Completions emits two system
messages in that order. Responses and Anthropic join the same two sections with
plain spacing inside their single instruction/system field. The rendered prompt
does not include an artificial "dynamic context" heading; that split is an
internal cache-layout detail, not model-facing instruction text.

Prompt caching works best when the beginning of the prompt is identical across
requests. That means the long-lived global system prompt, stable tool
definitions, and any reusable static project instructions stay before
conversation-specific state, restored drafts, and user messages. The daemon
does not include a timestamp in dynamic context because that would churn the
prefix-adjacent prompt for little value.

The daemon also no longer imposes a default OpenAI/Codex output-token cap.
`provider.max_tokens` remains an optional explicit cap if a particular session
needs one. Anthropic still receives a provider-local `max_tokens` fallback
because the Messages API requires that field.

### Tools Are A Separate Runtime Surface

Tool definitions and execution live outside `agent-core`. The core only models
tool requests and tool results. The daemon's builtin registry currently owns the
actual `read`, `write`, `edit`, and `bash` behavior and always executes allowed
tool calls.

This keeps core portable and makes future tool customization a daemon/runtime
choice.

### `agent-core` Stays Vocabulary And FSM

`agent-core` remains the finite-state turn engine and intentionally does not own
provider IO, websocket RPC, storage backends, or tool execution. Vocabulary
shared by provider/session/daemon code lives in `agent-vocab` so the core does
not need to grow just to share message data shapes.

### `agent-session` Replaces The Orchestrator Role

The old orchestration shape has been demoted into session semantics. The session
crate owns transcript branches, model context materialization, fork/rewind,
compaction validation, queued input ordering, and restoration from stored
session data.

The websocket daemon is the process boundary and repository owner; it is not a
general hierarchical subagent orchestrator.

## Testing Decisions

The highest-value tests are real behavioral exercises, not stub-heavy checks:

- Rust unit tests cover FSM, session branching, compaction validity, restoration,
  and stale completion invariants.
- Manual websocket scripts exercise real Postgres transitions, durable event
  replay, global config, input idempotency, steer/follow-up ordering, fork,
  rewind, interrupt, recovery, tools, and real Codex provider calls.
- The web app is built with TypeScript/Vite and then run against the same
  websocket daemon used for manual RPC exercises. Browser checks cover markdown
  and raw HTML rendering, slash autocomplete, picker-only rewind/fork, restored
  composer drafts, and local draft survival across refresh.

The manual websocket tests intentionally inspect both user-visible transcript
order and invisible database state, because many correctness bugs only show up
in the gap between those two views.

## Codex Auth Recovery Is Narrow And Explicit

Codex/ChatGPT credentials are not session configuration. The daemon reloads
OpenAI, Codex, and Anthropic credential material at model-call time so a session
can remain durable and idle while the process or auth file changes around it.

The only provider retry currently implemented is a single Codex 401 recovery:
refresh the ChatGPT token in `~/.codex/auth.json`, rebuild the Codex provider
with that refreshed token, and retry the same request once. This mirrors the
upstream Codex behavior without adding a generic fallback chain that would hide
real provider failures.

Provider errors are session events as well as turn outcomes. A model failure can
still close the open turn as `Crashed`, but websocket clients should surface the
paired `model.error` event so the user sees whether the cause was auth, network,
or provider-side rejection.

## Frontend Selection Is Immediate State

The web composer uses an imperative selected-session ref so queued sends do not
wait for a React render to find their target. Any code path that changes the
selected session must update that ref and the React state together. This matters
for `/new` and `fork`: the next Enter key after creating a session should target
the new durable session immediately, not the session that happened to be
selected one render earlier.
