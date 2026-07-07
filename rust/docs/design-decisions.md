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

### Composer Drafts Are Browser State

There is no browser-local draft *session*. Clicking New session is composer
state: the selected session is cleared, and the first non-command send calls
`session.start`, which creates the durable row and materializes the first user
input in one step (see "The Web UI Does Not Own Session Drafts" below).

Per-session composer *text* is web-owned `localStorage` state, keyed by session
(with a `__new_session__` key for the unsent new chat). It also holds restored
historical user messages after a switch. Composer text is deliberately never
stored in `sessions.metadata` or transcript rows.

Empty web-created durable rows are hidden defensively in `session.list` unless
they have transcript, queued input, or actions. `metadata.hidden = true` is a
separate list-filtering convention for local verification cleanup; it is not a
lifecycle state and does not delete or mutate transcript history.

### Slash Commands Are Thin RPC Calls

Slash commands exist to expose real websocket operations that do not already
have dedicated UI controls, without adding a second frontend command model.

- `/switch` opens the same-session history picker. It is idle-only. User
  message targets move the active leaf to the previous safe boundary and
  restore that message into the composer for editing; completed turn and
  compaction-summary targets simply become the active leaf.
- `/compact` requests context compaction.
- `/system` shows the selected session's rendered PI.md prompt and source
  template. It is unavailable before a durable session exists.

Model selection is not a slash command. The web top bar exposes the small model
picker and provider-specific reasoning effort picker. Provider/model identity is
locked after the first transcript entry because OpenAI Responses and Anthropic
Messages both carry provider-shaped replay state across turns; reasoning effort
is still a per-request knob and can be changed during a running turn. The change
applies to later provider requests, not one already in flight.

Normal composer text in a selected top-level session is `input.follow_up`, even
while the agent is running. In a selected delegation child it is instead a
parent-scoped `delegation.steer_subagent`; slash commands remain commands in
both cases. Queued follow-ups appear in a small pane above the composer. Each
queued follow-up has a row-level steer control; pressing it promotes that row
to the steer queue, ordered by promotion time. Each queued follow-up can also
be edited, cancelled, and reordered from the queue pane; the web UI wires
`input.update_queued`, `input.cancel_queued`, and
`input.reorder_queued_follow_ups`. Steers stay pinned on top and are not
reorderable. Active turns are interrupted with a stop button beside the
composer, not with a slash command.

Composer submission captures the selected session id once. App routing and
per-session draft resolution use that immutable id and a stable per-submission
client input/control id; they do not reread selection after an async boundary.
If the matching snapshot disappeared, the send fails and the text is restored
under the captured draft instead of being retargeted.

Stop is exact-session control: selecting a root stops that root only, and
selecting a child stops that child only. It does not cascade between
parent/children or cancel a delegation. Whole-delegation cancellation is a
separate run-board/model operation. Parent models can also steer one child
without interruption (the backward-compatible default), atomically request an
interrupt-and-steer, or call `interrupt_subagent` for exact-child
interrupt-only control. Both interrupting forms use a durable
parent/delegation/child-scoped ledger, fence the complete active turn attempt
set, and distinguish acceptance (`pending_interrupt`) from application
(`interrupt_applied`) and task settlement (`ready`). Interrupt-only ledger rows
cannot be consumed as model text.

Slash autocomplete is intentionally shallow: it only appears while typing the
command name. Enter on a partial command accepts the highlighted completion and
adds a trailing space; the next Enter submits. Enter on an exact command submits
immediately. This keeps discovery without stealing execution.
The web UI does not render a generic "message queued" notice after ordinary
sends. That acknowledgement only means Postgres accepted a durable queued input;
it is not a transcript event and was visually misleading while the agent was
still running. Errors still surface, while real conversation progress comes
from transcript entries and activity/tool state.

Switch does not accept raw transcript ids in the web UI. The picker is the only
user-facing path, so history mutations are chosen from visible turn context
rather than opaque storage identifiers.

Turn-start, graceful turn-finish, and tool-call-start bookkeeping entries are
not rendered as transcript messages. Assistant tool calls render as compact
collapsible rows inspired by Bigband and Claude Relay, with the matching result
folded into the tool row instead of appearing as a separate raw event.

The central transcript renders only the active root-to-leaf branch. Switch does
not delete abandoned rows from Postgres, but those off-branch rows disappear
from the main conversation view. They remain available to the history tree and
switch picker.

### Global System Prompt

The system prompt is repo-level `PI.md`, not per-session state. The UI exposes
`/system` only for selected sessions because project workspaces must be
materialized before the rendered prompt can include workspace instructions and
skills.

Sessions keep provider and metadata only. This avoids hidden prompt drift and
makes the daemon's behavior easier to reason about for personal use.

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
- provider selection and request execution in `provider_runtime/`
- live session lifecycle, `SessionDriver` serialization, queued-input
  materialization, dispatch, and event publication in `runtime/`
- session creation/materialization ordering in `session_start.rs`
- workspace base refresh and session instantiation in `workspaces/`
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

`agent-daemon` also parses websocket method names into daemon-local enums before
dispatching. JSON content blocks, image sources, and assistant items use the
serde-tagged vocabulary types from `agent-vocab` instead of hand-matching
`"type"` and `"kind"` strings in the codec.

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
events and refreshes `session.get` with `include_entries=true` from the daemon.
The daemon similarly avoids a second durable staging model in RAM; active memory
is only a work-in-progress projection.

This keeps the Postgres-only direction clean: storage changes should not leak
into frontend state semantics.

### Idle Input Skips The Queue, Busy Input Stays Durable

When a session is idle, `input.follow_up` and `session.start` feed the message
directly into the session and persist the resulting
transcript/actions/events before dispatching follow-on work. The same
transaction records a consumed input ledger row when a `client_input_id` is
present. That keeps the common path small without losing retry idempotency:
there is no artificial queued state for a message that can be acted on
immediately.

When a model/tool/compaction action is unfinished, composer sends remain
`queued_inputs.status='queued'` follow-ups by default. Promoting a row changes
its priority to `steer` and records `origin.promoted_at`; the queue consumes
steers first in promotion order, then remaining follow-ups by dense
`follow_up_position` order. Follow-up reorder sends the complete id order rather
than sparse/gapped order numbers; the store rewrites positions to `0..n-1`.
If an active turn has just finished a tool batch and is about to request the
model again, the daemon peeks one queued steer before continuing and appends it
as a same-turn `user_message` after the tool results. Follow-ups are not
eligible for that mid-turn slot. During compaction there is no same-turn slot,
so queued steers wait behind the compaction action and become the next turn from
the compacted root. Switch remains idle-only.

The Rust websocket surface exposes queued follow-up edit, cancel, and reorder
mutations. These mutations only apply to `priority='follow_up'` and
`status='queued'`. Steering rows are top-of-queue control messages: they cannot
be edited, cancelled, or reordered by the follow-up mutation RPCs and remain
ordered by steering/promote time. Editing historical input still uses interrupt
plus switch picker semantics.

Before the daemon materializes a queued row, it peeks the next row without
moving it to `consuming`. The transcript commit marks that row `consumed` and
validates both the row version and that it is still the canonical next queued
input. If a queued mutation or new steer wins the race, the commit fails and
the daemon reloads from Postgres. The legacy `consuming` vocabulary remains
only for old rows and reset-on-touch recovery.

That choice prevents a daemon-death gap where accepted user input has been moved
into an in-memory mailbox but has not yet appeared in transcript history, while
also allowing queued follow-up edits/cancels/reorders to win races cleanly.

### Input Idempotency Is Event-Idempotent Too

For busy-session queued input, `client_input_id` is unique per session.
Retrying the same input id returns the original queued row id and does not emit
a second `input.queued` event. For idle accepted input, retrying the same id
finds the consumed ledger row and does not append another user message.

The web composer keeps the same generated `client_input_id` while a send is
unconfirmed, so a lost websocket response can be retried without duplicating the
user message.

For a brand-new chat, the browser generates the `session_id` and sends it with
`session.start`. `session.start` is idempotent on that id: if the session
already exists it returns `{ replayed: true }` instead of creating a second
session, and the stable `client_input_id` keeps the first message from being
duplicated.

### Switch And History Targets

Switch operates on committed transcript boundaries or root. The UI presents
targets as visible user messages and completed turns, then maps them to the
boundary-only backend operation. It does not checkpoint or restore workspace
files; project sessions keep their current private workspace directories.

Picker actions carry expected active-leaf information for source-mutating
switch and for sending restored composer drafts. If the session moved since the
visible choice was made, the daemon returns `history_changed` and the UI
refreshes instead of applying the edit to a different branch.

Switch mutates the selected session's active branch, so it remains idle-only.
For websocket RPC, idle means no unfinished action and no queued input waiting
to become transcript. In the normal user flow, that is the point after a turn
has finished and the queue has drained. If a user wants to switch during a turn,
they should interrupt first, then switch after the interrupted tail has been
committed.

### Recovery Keeps Transcript Semantics

Daemon recovery checks for unfinished action rows before first use of an idle-
looking session. If previous work died mid-turn, recovery marks unfinished
actions stale and appends a crashed turn tail so the transcript remains
structurally valid.

Interrupted and crashed states are transcript outcomes, not broad session
lifecycle states. The session activity enum stays small: `idle`, `queued`, and
`running`.

### Compaction Is A Typed Root, Not A Replacement Transcript

The daemon summarizes only the dynamic transcript/model context for the active
leaf. It does not summarize or rewrite the global stable system prompt, which
remains provider configuration rendered before transcript history on normal
model calls.

The summary is persisted as `TranscriptItem::CompactionSummary` with
`parent_id = null`. Its `source_session_id` and `source_leaf_id` are lineage
pointers for the UI/tree, not model-visible ancestry. Postgres installs that
root with a compare-and-set transaction that also marks the compaction action
complete and emits `history.compacted` / `compaction.completed`.

This removed the old generic `InjectedMessage` path and the session-owned
replacement-context compaction FSM. `agent-session` no longer asks a harness to
return an arbitrary `ModelContext`; the only special transcript context shape is
the typed compacted root.

### The Web UI Does Not Own Session Drafts

The web client no longer keeps a parallel list of browser-local session drafts.
Only Postgres-backed sessions appear in the sidebar. Starting a new chat is a
composer state: the selected session is cleared, and the first non-command send
creates the durable session through `session.start`.

This keeps the UI aligned with the append-only transcript forest. Switch is a
tree operation over durable transcript entries; its only extra UI
convenience is restoring a selected user message into the composer for editing.
That restored text is transient visible state, not a second session model.

### Provider Scope Is Intentionally Small

`agent-provider` targets OpenAI/Codex and Anthropic/Claude. The daemon reads
Codex credentials from `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`, including
the ChatGPT/Codex account id when present. OpenAI models always use this
ChatGPT/Codex subscription transport; pi-relay no longer supports plain OpenAI
API-key auth.

Provider config supports `prompt_cache.key`, which the daemon forwards on the
OpenAI request path. The Codex Responses request uses the selected private
catalog entry's `supports_parallel_tool_calls` value and hardcodes the
low-variance personal-use policy `service_tier = "priority"` and
`store = false`. It intentionally omits `prompt_cache_retention` because
pi-relay does not use the plain OpenAI API-key path. OpenAI prompt-cache cohort
selection is explicit
`ProviderConfig.prompt_cache.key` first, then the pi-relay session id (matching
Codex CLI's `thread_id`/`prompt_cache_key` behavior), then a fresh UUID fallback
for CLI/test paths that do not carry a session. The actual system prompt remains
global.

Provider requests now carry `PromptSections`: a stable prefix and optional
dynamic context. The stable prefix is the global system prompt. Normal turns keep
dynamic context empty and rely on transcript history for runtime facts. If
dynamic context is used, providers place it at the tail: OpenAI Responses emits
it after transcript input items, and Anthropic appends it as a final uncached
user message after transcript messages. The prompt split itself does not add an
artificial "dynamic context" heading; any heading present is owned by the
dynamic section content.

Delegation state is intentionally not injected into normal parent turns or into
compaction provider inputs. The special case happens after compaction: once the
provider returns a compacted summary, top-level parent compaction appends a fresh
`## Delegation state at compaction time` ledger to the stored summary. That
ledger lists every parent delegation row/status with bounded details and no
transcript body inlining. Later compactions let the provider summarize whatever
prior summary text exists, including older point-in-time ledgers, then append a
fresh ledger again. The latest appended ledger is authoritative and supersedes
older ledger text by position. Subagent compaction omits parent/sibling
delegation state entirely, because subagents do not orchestrate the delegation
tree.

Delegation completion wakeups are daemon-authored observations, not
assistant-authored decisions. The durable transcript stores them as a typed
`daemon_tool_observation` item with an `inspect_delegation` tool name, stable
local call id, arguments, status, summary, and bounded JSON snapshot. This keeps
internal transcript semantics honest: the daemon observed delegation state; the
assistant did not choose a tool call.

Provider adapters render that typed item in the provider-native synthetic
tool-call/result shape only at request construction time. OpenAI receives an
adjacent `function_call` plus `function_call_output` pair without
provider-generated-looking ids/status. Anthropic receives an adjacent assistant
`tool_use` message plus user `tool_result` message with a deterministic
`toolu_...` id. The UI renders the same item as a daemon/system observation, not
as a user bubble and not as a model-selected tool run. Text fallback rendering is
kept for unsupported contexts and diagnostics.

Delegation snapshots avoid context-heavy payloads: raw subagent task prompts,
final-message prose, and transcript bodies are not inlined. Long bodies are
referenced via handoff files such as `task_prompt.md`, `final_message.md`, and
`transcript.md`; only workflow control facts such as `outcome` remain
inline.

Prompt caching works best when the beginning of the prompt is identical across
requests. That means the long-lived global system prompt, stable tool
definitions, transcript prefix, and any reusable static project instructions
stay before volatile conversation-specific state. The daemon does not include a
timestamp in dynamic context because that would churn the prompt for little
value.

The daemon also no longer imposes a default OpenAI/Codex output-token cap.
`provider.max_tokens` remains an optional explicit cap if a particular session
needs one; OpenAI requests emit it as `max_output_tokens` and otherwise omit the
field. Anthropic's Messages API does require `max_tokens`, so its provider uses
API-discovered/static per-model ceilings and clamps explicit limits to that
ceiling. With no explicit limit it requests at most 64k: enough headroom for
high-effort work without making an ordinary turn reserve the full 128k supported
by Sonnet 5, Fable 5, and Opus 4.8. Model discovery is cached and has a
conservative static fallback, so an API outage never removes known UI options or
turns off proactive compaction for known models. Discovery refreshes are
single-flight per model. A transient refresh failure backs off for one minute
while preserving stale last-known-good metadata instead of replacing it with a
less authoritative fallback.

Fable classifier refusals are successful HTTP responses but not successful model
turns. A `stop_reason: "refusal"` discards all partial assistant/replay output,
surfaces its optional category/explanation as an action error, and does not
automatically fall back to a different model.

### Tools Are A Separate Runtime Surface

Tool definitions and execution live outside `agent-core`. The core only models
tool requests and tool results. The daemon's builtin registry currently owns the
actual `edit`, `bash`, `web_search`, `web_fetch`, `LoadSkill`, and
delegation-tool behavior and always executes allowed tool calls. (`web_search`
is registered but has no configured backend yet, so it returns an error result.)

This keeps core portable and makes future tool customization a daemon/runtime
choice.

### Provider Tool Surfaces Diverge Only When Semantics Justify It

Coding tools split into two posture buckets:

- **Uniform custom function tool** for `bash`. Both providers see the same
  JSON-schema function tool, generated from a single builtin definition in
  `agent-tools`. The pi-relay runtime starts a fresh `bash -lc` per invocation,
  so the model-facing contract should match what the runtime can actually
  honor. Its description directs file and text searches to `rg` instead of
  `grep`. Using Anthropic's native `bash_20250124` would have advertised a
  persistent shell session with a `restart` op that the runtime does not
  back, which is worse than losing the small training prior associated with
  the native tool name.
- **Provider-native** for the edit tools: OpenAI's `apply_patch` uses a Lark
  grammar so patches escape the JSON-string ghetto (real token win), and
  Anthropic's `text_editor_20250728` exposes `view`/`create`/`str_replace`/
  `insert` semantics the model is specifically trained to use. These
  schemas are semantically rich enough that paraphrasing them as generic
  function tools would lose information the provider's training already
  encodes.
- **Local JSON wrappers** for `web_search` and `web_fetch`. The main model turn
  always sees ordinary client-executed tools, which keeps transcript replay and
  token accounting on one surface. The tool runtime can still delegate to a
  provider-native web backend in a sidecar call when that backend exists.

The tool workspace is the session `outer_cwd`. `bash` does not accept a
`workdir` override; the model relies on the announced cwd in the dynamic
prompt context and uses `&&` chaining or inline `cd` for subdirectory work.
Any future persistent-shell runtime would add session-level cwd state
underneath the same model-visible schema, not above it.

### `agent-core` Stays The FSM

`agent-core` remains the finite-state turn engine and intentionally does not own
provider IO, websocket RPC, storage backends, or tool execution. Vocabulary
shared by provider/session/daemon code lives in `agent-vocab` so the core does
not need to grow just to share message data shapes.

### `agent-session` Replaces The Orchestrator Role

The old orchestration shape has been demoted into session semantics. The session
crate owns transcript branches, model context materialization, active-leaf
switching, queued input ordering, and restoration from stored session data.
Compaction
installation lives in `agent-store` because it is a durable Postgres
transaction.

The websocket daemon is the process boundary and repository owner. It supports
bounded parent/child subagent delegation (children are forked sessions with a
spawn/list/wait/steer/interrupt surface), but it is not a general hierarchical
orchestrator with cross-session message routing.

## Testing Decisions

The highest-value tests are real behavioral exercises, not stub-heavy checks:

- Rust unit tests cover FSM, session branching, compaction-summary boundaries,
  restoration, and stale completion invariants.
- Manual websocket scripts exercise real Postgres transitions, transient
  reconnect event replay, global config, input idempotency, steer/follow-up
  ordering, switch, interrupt, recovery, tools, and real Codex provider
  calls.
- The web app is built with TypeScript/Vite and then run against the same
  websocket daemon used for manual RPC exercises. Browser checks cover markdown
  and raw HTML rendering, slash autocomplete, picker-only switch, restored
  composer drafts, and local draft survival across refresh.

The manual websocket tests intentionally inspect both user-visible transcript
order and invisible database state, because many correctness bugs only show up
in the gap between those two views.

## Codex Auth Recovery Is Narrow And Explicit

Codex/ChatGPT credentials are not session configuration. The daemon reloads
OpenAI, Codex, and Anthropic credential material at model-call time so a session
can remain durable and idle while the process or auth file changes around it.

Model dispatch retries every `ProviderError` up to five attempts before the
turn fails, preserving provider diagnostics for the user-visible error event.
Context-overflow classification remains special: exhausted overflow errors feed
the daemon's compaction/recovery path instead of being treated as ordinary
provider failures.

Codex 401 recovery is a separate inner auth retry inside a single provider
call: refresh the ChatGPT token in `~/.codex/auth.json`, rebuild the Codex
provider with that refreshed token, and retry the same request once. This
mirrors the upstream Codex behavior without adding a broad auth fallback chain
that would hide real credential failures.

Provider errors are live session events as well as turn outcomes. A model
failure can still close the open turn as `Crashed`, but websocket clients should
surface the paired `model.error` event while it is live so the user sees whether
the cause was auth, network, or provider-side rejection. Old provider errors are
not durable notifications.

## Frontend Selection Is Immediate State

The web composer uses an imperative selected-session ref so queued sends do not
wait for a React render to find their target. Any code path that changes the
selected session must update that ref and the React state together. This matters
for new-session flows: the next Enter key after creating a session should target
the new durable session immediately, not the session that happened to be
selected one render earlier.
