# Websocket RPC Contract

This is the frontend-facing control plane implemented by `agent-daemon`
(`pi-agentd`). It is intentionally small and personal-use oriented: Postgres is
the durable source of truth, websocket connections are only observers/controllers,
tools always run when requested, and there is no approval interface.

The goal of the contract is to make every user-facing behavior testable by
sending the same websocket frames a frontend would send.

## Core Decisions

1. Sessions are durable rows, not opened processes.
   There is no user-facing `open`, `close`, `resume`, or `delete` RPC. A
   frontend starts a new chat with `session.start` when the first message is
   sent, subscribes with `events.subscribe`, and gets the current state with
   `session.get`. Empty durable sessions are not part of the websocket
   contract; browser-local drafts become durable only through `session.start`.

2. Idle sessions resume implicitly.
   When a daemon starts or a browser reconnects, the active leaf, transcript
   entries, queued inputs, actions, and events are already in Postgres. No
   separate resume command is needed.

3. Postgres is authoritative.
   The daemon may materialize an `AgentSession` while driving a turn, but every
   accepted transition is committed to Postgres before any follow-on model/tool
   work is dispatched. The in-process map is an execution cursor and concurrency
   convenience, not a session registry.

4. Activity is derived.
   The frontend sees only `idle`, `queued`, or `running`, derived from queued
   inputs and unfinished action rows. `Interrupted` and `Crashed` are transcript
   turn outcomes, not session statuses.

5. Source-mutating history writes are idle-only.
   `history.rewind`, `session.configure`, and `compaction.request` fail with
   `session_busy` while work is active or queued. Here idle means there are no
   unfinished actions and no queued inputs waiting to become transcript. A
   frontend should send `input.interrupt`, wait for idle, then retry.

6. Fork is source-non-mutating.
   `history.fork` is allowed while the source session is running as long as the
   target `leaf_id` is an explicit existing transcript entry. Fork from `null`
   is rejected with `missing_leaf_id`.

7. Tools are always allowed.
   The daemon runs model-requested tools immediately. There is no approval,
   denial, or tool-specific cancel RPC. `input.interrupt` is the one user-facing
   cancellation command and interrupts the active turn.

8. Daemon death is recoverable state.
   On startup, a daemon marks leftover unfinished action rows stale because the
   provider/tool futures from the previous process cannot resume. If the daemon
   died with an open turn, first touch then repairs the session by appending a
   crashed turn tail. External side effects, such as files written by tools,
   are not transactional.

## Postgres Model

`agent-store::PostgresAgentStore` owns the concrete Postgres model used by the
daemon. Postgres is the only supported durable backend; session snapshot types
live in `agent-session`, and there is no separate in-memory/JSONL store layer.
Closed persistence vocabularies such as provider kind, input priority, queued
input status, action kind/status, session activity, and event type are Rust
enums that serialize to the string values shown below.

### `sessions`

```text
id text primary key
project_id uuid not null references projects(id)
starting_cwd text not null
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
active_leaf_id text null
provider_config jsonb not null
metadata jsonb not null default '{}'::jsonb
```

### `projects`

```text
id uuid primary key
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
name text not null
starting_cwd text not null
metadata jsonb not null default '{}'::jsonb
```

Projects are host folders. Every session has a `project_id` and snapshots the
project `starting_cwd` when the session is created; model prompt context and
local tools use the session's stored `starting_cwd`.

`provider_config`:

```json
{
  "kind": "openai",
  "model": "gpt-5.5",
  "reasoning_effort": "xhigh",
  "prompt_cache": { "key": "pi-relay-local" }
}
```

Supported provider kinds are:

- `codex`: OpenAI Responses API through the ChatGPT/Codex backend. Uses
  `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`, plus `ChatGPT-Account-ID` when
  available, and sends the Codex residency routing header required by
  workspace-backed ChatGPT accounts. This is the path tested with real
  credentials.
- `openai`: alias for the same ChatGPT/Codex subscription transport. pi-relay
  intentionally does not support plain OpenAI API-key auth for OpenAI models.
- `anthropic` or `claude`: Anthropic Messages API through `ANTHROPIC_API_KEY`.

`prompt_cache.key` maps to `ModelRequest::prompt_cache_key` and is sent on the
OpenAI request path. `max_tokens` is optional; when omitted the daemon does not
set an OpenAI output cap.

`reasoning_effort` defaults to `xhigh`. OpenAI currently accepts `none`,
`minimal`, `low`, `medium`, `high`, and `xhigh` in pi-relay. Claude accepts
`low`, `medium`, `high`, `xhigh`, and `max`; Claude Opus 4.7 requests are sent
with adaptive thinking and `output_config.effort`.

### `daemon_config`

Global daemon configuration:

```text
key text primary key
value jsonb not null
updated_at timestamptz not null default now()
```

The global `system_prompt` entry is read for provider requests. It is not
stored per session. The provider request renders that global prompt as the
stable prefix, followed by daemon-generated dynamic context such as the current
workspace, then transcript history.

### `transcript_entries`

Append-only transcript forest:

```text
session_id text not null references sessions(id) on delete cascade
id text not null
parent_id text null
timestamp_ms bigint not null
item jsonb not null
turn_id bigint null
sequence bigserial not null
primary key (session_id, id)
```

The active context is the root-to-`active_leaf_id` path. Rewind moves the active
leaf; it does not delete rows.

### `queued_inputs`

Durable user input queue:

```text
id text primary key
session_id text not null references sessions(id) on delete cascade
priority text not null          -- steer | follow_up
content jsonb not null
origin jsonb null
status text not null            -- queued | consuming | consumed | cancelled
created_at timestamptz not null default now()
client_input_id text null
```

`client_input_id` is unique per session when present. Busy-session retries do
not enqueue duplicate rows or emit a second `input.queued` event. Idle accepted
inputs are also recorded here with `status='consumed'` in the same transaction
that appends transcript/action/event state, so retrying a lost websocket
response does not append the user message twice. Busy-session rows stay
`queued` while model/tool/compaction work is unfinished, so a daemon crash
cannot lose accepted input that has not yet appeared in the transcript.

Before a queued input is materialized, the daemon claims it by moving it to
`consuming` and recording a claim id in `origin`. The user-facing edit path is
`input.interrupt` followed by picker-driven rewind or fork; queued rows can be
promoted to steer priority but are not edited or cancelled through websocket
RPC. The daemon marks the row `consumed` in the same transaction that appends
the corresponding transcript/action events, and validates the claim id before
doing so. On daemon restart, abandoned `consuming` rows are reset to `queued`
before recovery continues.

### `actions`

Durable external work:

```text
id text primary key
session_id text not null references sessions(id) on delete cascade
turn_id bigint null
action_id bigint not null
attempt_id text not null
kind text not null              -- model | tool | compaction
status text not null            -- running | completed | error | interrupted | stale
payload jsonb not null
result jsonb null
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
```

`attempt_id` prevents stale completions from a prior daemon attempt from
mutating the transcript after interrupt/recovery.

### `events`

Transient websocket reconnect buffer:

```text
id bigserial primary key
session_id text not null references sessions(id) on delete cascade
type text not null
payload jsonb not null
created_at timestamptz not null default now()
```

`events.subscribe(after_event_id)` is a reconnect stream, not a historical
notification feed. With a concrete `after_event_id`, it returns missed rows in
the RPC response and then streams live event frames on the same websocket while
those rows are still in the reconnect buffer. With `after_event_id = null` or
omitted, it starts at the current event head and returns no historical events;
clients should load durable state through `session.get`/`history.tree` instead.
When a session reaches idle, the daemon publishes `session.idle` to live
subscribers and then clears that session's event rows. Idle-only mutations such
as configuration changes, same-session history switching, and child fork
creation also clear their session event buffers after live publication. Durable
session state lives in `sessions`, `transcript_entries`, `queued_inputs`, and
`actions`; old toast-worthy events such as `model.error` are not retained as
history.

## Transcript Validity

An open transcript tail is valid while work is in flight if unfinished action
rows explain it.

Valid live states include:

- `TurnStarted` + `UserMessage` with a running model action.
- Assistant tool calls plus matching running tool actions.
- Partial parallel tool completion, where recorded `ToolResult`s are emitted in
  assistant-declared order and remaining tool calls still have running actions.
- `TurnFinished { outcome: Interrupted }` after interrupt.
- `TurnFinished { outcome: Crashed }` after provider failure or daemon recovery.

Invalid states should not be produced by the websocket service:

- A closed turn with missing tool results.
- A tool result without a matching assistant tool call.
- A model request built from an open tool tail.
- Rewind to a non-boundary transcript entry.
- Transcript rows committed without the matching action/event updates.

Forking to a non-boundary entry is valid because the source is not mutated; the
new session owns the copied partial path. The daemon closes that copied partial
tail as `Interrupted` in the child so it is immediately runnable and does not
look like daemon crash recovery.

Accepted transitions commit transcript rows, action updates, queued-input
updates, active-leaf changes, and events in one transaction.

## RPC Envelope

Requests:

```json
{ "id": "req_1", "method": "input.follow_up", "params": {} }
```

Successful responses:

```json
{ "id": "req_1", "ok": true, "result": {} }
```

Errors:

```json
{
  "id": "req_1",
  "ok": false,
  "error": {
    "code": "session_busy",
    "message": "source-mutating history operations require an idle session",
    "data": {}
  }
}
```

Live events:

```json
{
  "event_id": 42,
  "event": "transcript.appended",
  "session_id": "s1",
  "data": {}
}
```

## Session RPC

### `session.start`

Creates a durable session and immediately feeds the first user message. This is
the normal frontend path for a brand-new draft.

```json
{
  "session_id": "optional-stable-id",
  "project_id": "f2b0e23c-1fd7-4977-9d60-f6842e25d15b",
  "provider": {
    "kind": "codex",
    "model": "gpt-5.5",
    "prompt_cache": { "key": "pi-relay-local" }
  },
  "metadata": { "title": "New session", "created_by": "web" },
  "client_input_id": "web_start_draft_1",
  "priority": "follow_up",
  "content": [
    { "type": "text", "text": "Hello" }
  ]
}
```

The daemon writes `session.created`, `input.accepted`, transcript entries,
actions, and events in the same session-start transition before dispatching
provider/tool work. It snapshots the project's current `starting_cwd` into the
new session row; later `project.update` calls do not change that stored session
cwd. Retrying the same stable `session_id` returns the existing session with
`"replayed": true` rather than creating a second session.
For web drafts, the frontend should always provide both the stable draft-owned
`session_id` and `client_input_id`.

### `session.list`

Lists durable sessions, newest first.

```json
{ "limit": 50 }
```

Each row includes `session_id`, `project_id`, `starting_cwd`, `activity`,
`active_leaf_id`, `provider`, `metadata`, and `updated_at`. Defensive listing
hides accidental empty web-created rows that have no transcript, queued input,
actions, or fork provenance. Rows with `metadata.hidden = true` are also omitted
from the list; this is used for local verification cleanup, not as a core
lifecycle state. Browser-local drafts are not returned by this RPC.

### `session.get`

Recovers the session if needed, then returns a durable snapshot.

```json
{ "session_id": "s1", "include_entries": true }
```

Result shape:

```json
{
  "session_id": "s1",
  "project_id": "f2b0e23c-1fd7-4977-9d60-f6842e25d15b",
  "starting_cwd": "/Users/me/src/my-repo",
  "activity": "idle",
  "active_leaf_id": "entry_9",
  "provider": { "kind": "codex", "model": "gpt-5.5" },
  "metadata": {},
  "pending_actions": [],
  "queued_inputs": [],
  "last_event_id": 42,
  "entries": []
}
```

`queued_inputs` contains live queued or consuming user inputs with `input_id`,
`priority`, `status`, `content`, `client_input_id`, `created_at`, and optional
`promoted_at`. The web UI uses it for the composer-adjacent queue pane.
`entries` is included only when `include_entries` is true. The web UI uses that
expanded snapshot for normal transcript refreshes so it does not need a second
round trip.

## Project RPC

### `project.list`

Returns visible projects:

```json
{ "projects": [] }
```

Each project has `project_id`, `name`, `starting_cwd`, `metadata`, `created_at`,
and `updated_at`. The project `starting_cwd` is a default for new sessions; each
session snapshots its own cwd at creation time.

### `project.create`

```json
{
  "name": "my repo",
  "starting_cwd": "/Users/me/src/my-repo",
  "metadata": { "created_by": "web" }
}
```

### `project.update`

Renames a project and/or changes the starting cwd used for future sessions. The
cwd must be an existing directory. Updating a project cwd does not change the cwd
of existing sessions in that project.

```json
{
  "project_id": "f2b0e23c-1fd7-4977-9d60-f6842e25d15b",
  "name": "pi-relay",
  "starting_cwd": "/Users/me/src/pi-relay"
}
```

### `session.rename`

Updates the UI-facing session title stored in metadata. The title is required
and must be a non-empty string. This is a dedicated path so clients do not need
to round-trip or overwrite unrelated metadata keys.

Request:

```json
{ "session_id": "s1", "title": "Production deploy notes" }
```

Response:

```json
{ "session_id": "s1", "title": "Production deploy notes", "activity": "idle" }
```

Emits `session.configured`, so subscribed clients should refresh lists/snapshots.

### `session.configure`

Idle-only. Replaces provider config and/or metadata. Once a session has any
transcript entry, `provider.kind` and `provider.model` are locked; clients may
still change provider-adjacent knobs such as `reasoning_effort` during or
between turns. Runtime changes apply to subsequently created provider requests,
not to an already in-flight request.

## Config RPC

### `config.get`

Returns global daemon configuration:

```json
{ "system_prompt": "optional or null" }
```

### `config.set`

Updates global daemon configuration. Send `null` to clear the prompt.

```json
{ "system_prompt": "Reply briefly." }
```

## Subscription RPC

### `events.subscribe`

```json
{ "session_id": "s1", "after_event_id": 42 }
```

The response contains:

```json
{ "replayed": [] }
```

After the response, matching live events stream as event frames.

If `after_event_id` is `null` or omitted, the daemon subscribes from the current
head and returns an empty replay. Use a concrete id only for reconnecting after
a known high-water mark.

### `events.unsubscribe`

Stops streaming live events for a session on the current websocket.

## Input RPC

### `input.follow_up`

Normal user message. If the session has no unfinished actions and no queued
inputs, the daemon feeds the message into the session immediately. If work is
running or already queued, the daemon stores a durable queued row.

```json
{
  "session_id": "s1",
  "client_input_id": "ci-1",
  "expected_active_leaf_id": "entry_9",
  "content": [
    { "type": "text", "text": "Fix this test" }
  ]
}
```

`expected_active_leaf_id` is optional. When present, the daemon rejects the
input with `history_changed` if the active branch moved before the message was
accepted or queued. The web UI uses this for restored composer drafts so a
historical edit cannot silently send into a newer context. `client_input_id`
is optional but strongly recommended for frontend sends; without it, retry
idempotency is intentionally not provided.

### `input.promote_queued`

Promotes a still-queued follow-up into the steer queue. Promotions are consumed
in promotion order before remaining follow-ups. If a turn is between completed
tool results and the next model request, the daemon claims the next queued steer
and appends it as a same-turn user message before sending that model request.
Follow-ups never use that mid-turn slot. Steers queued while compaction is
running wait for the compaction action and then materialize as the next turn
from the active compacted root.

```json
{ "session_id": "s1", "input_id": "input_..." }
```

Response:

```json
{
  "input_id": "input_...",
  "priority": "steer",
  "status": "queued",
  "promoted": true
}
```

If the row was already claimed or consumed, the call succeeds with
`"promoted": false` and the current row status. This makes the browser's stale
queued-row race non-fatal. A missing input id still fails with
`input_not_found`.

### `input.interrupt`

Interrupts current turn work. If the session is idle, the daemon emits
`input.ignored` and returns `{ "ignored": true }`.

Content blocks use `agent-vocab`:

```json
[
  { "type": "text", "text": "What is in this image?" },
  {
    "type": "image",
    "image": {
      "mime_type": "image/png",
      "source": { "kind": "url", "value": "https://example.com/image.png" }
    }
  }
]
```

Image sources support `url` and `base64`.

## History RPC

### `history.tree`

Returns all transcript entries plus `active_leaf_id`.

### `history.context`

Returns the materialized model context for `leaf_id`, or the active leaf when
omitted.

### `history.rewind`

Idle-only. Moves the active leaf to a committed turn boundary or to root.
This is the one source-mutating history operation: frontends use the same RPC
both for "rewind and edit this user message" and for "switch the active view to
this completed branch or compaction root." The RPC never creates a session and
never deletes abandoned branches.

```json
{ "session_id": "s1", "leaf_id": "entry_4", "expected_active_leaf_id": "entry_9" }
```

Root rewind:

```json
{ "session_id": "s1", "leaf_id": null }
```

Running sessions fail with `session_busy`; non-boundaries fail with
`not_turn_boundary`. If `expected_active_leaf_id` is supplied and the session
has moved since the picker was opened, rewind fails with `history_changed`.

### `history.fork`

Creates a new durable session from any existing transcript entry. This does not
mutate the source, so it can run while the source is busy. The child receives a
snapshot of the source session's full transcript forest, then its active leaf is
set to the requested fork target. That means compaction roots and
pre-compaction branches remain navigable in the child session. If the target
entry is inside an open turn, the child receives an interrupted turn finish on
that copied branch.

```json
{
  "session_id": "s1",
  "leaf_id": "entry_4",
  "placement": "at",
  "new_session_id": "optional"
}
```

Returns the new session id, the requested source leaf, and the child active
leaf. For non-boundary forks, the child active leaf is the appended interrupted
turn finish.

For a user-message target, the frontend can request:

```json
{ "session_id": "s1", "leaf_id": "entry_user_1", "placement": "before" }
```

That creates the child from the previous completed turn boundary, or from root
for the first user message. The selected user message itself remains a
frontend composer draft, not transcript state in the child.

`leaf_id: null` fails with `missing_leaf_id`; unknown entries fail with
`entry_not_found`.

### `turn.resume`

Idle-only. Restarts the active terminal turn when that turn ended as
`Crashed` or `Interrupted` during model work. The daemon looks up the original
model action checkpoint, moves the active leaf back to that checkpoint, and
creates a fresh model action with the same turn/action ids. The old terminal
branch stays in the transcript forest; the retried/continued output becomes a
sibling branch, so the original user message is not duplicated.

```json
{
  "session_id": "s1",
  "leaf_id": "entry_turn_finished",
  "expected_active_leaf_id": "entry_turn_finished"
}
```

`leaf_id` may be omitted to resume the current active leaf. If supplied, it
must be the active leaf; this RPC is not a general history switch. Graceful
turns fail with `not_resumable`, non-terminal targets fail with
`not_terminal_turn`, active/queued sessions fail with `session_busy`, and turns
whose terminal work was tool execution fail with `not_resumable` until explicit
tool-rerun semantics exist.

## Tools

### `tools.list`

Returns the builtin definitions: `read`, `write`, `edit`, and `bash`.

No other tool RPC exists. Tool requests are automatic. A tool-level failure,
such as a missing file, missing edit target, malformed args, non-zero bash exit,
or bash timeout, is fed back as an error `ToolResult` and the action row status
is `error`. The session-level event is still usually `tool.completed` because
the core accepted a tool result; `tool.error` is reserved for session action
failures.

## Compaction RPC

### `compaction.request`

Idle-only in the websocket contract. Requests compaction of the active context
and creates a running compaction action. The daemon runs the configured provider
for compaction and writes the compacted transcript root transactionally before
queued follow-ups can advance.

## Development Harness RPC

Harness methods are development-only controls for exercising lifecycle edges
while still using the real websocket router, Postgres repository, session FSM,
and event buffer.

Implemented harness methods:

- `harness.model.complete`
- `harness.model.fail`

There is deliberately no `harness.tool.complete` or `harness.tool.timeout`.
Tool behavior is tested by letting the real builtin tools run. Compaction is
also no longer harness-completed; `compaction.request` starts a provider-backed
daemon job that writes a compacted transcript root transactionally.

### `harness.model.complete`

```json
{
  "session_id": "s1",
  "action_row_id": "action_...",
  "assistant": {
    "items": [
      { "type": "text", "text": "Done." }
    ]
  }
}
```

Assistant items support:

```json
{ "type": "text", "text": "hello" }
{ "type": "thinking_redacted" }
{ "type": "tool_call", "id": "call_1", "tool_name": "read", "args_json": "{\"path\":\"README.md\"}" }
```

## Event Set

Current event names:

```text
session.created
session.configured
input.accepted
input.queued
input.consumed
input.promoted
input.ignored
transcript.appended
turn.started
turn.finished
assistant.message
action.requested
model.requested
model.completed
model.error
tool.requested
tool.started
tool.completed
tool.error
compaction.requested
compaction.completed
compaction.error
history.rewound
history.forked
history.compacted
session.work_cancelled
session.recovered
session.idle
```

No approval or awaiting-approval events are emitted.

## Manual Websocket Exercise Plan

These checks should be run by sending websocket RPC frames exactly like a
frontend. Harness methods are acceptable for model and compaction timing; tool
behavior should use real builtin tools.

Useful SQL after each scenario:

```sql
select active_leaf_id, provider_config, metadata
  from sessions where id = '<SESSION>';
select key, value from daemon_config order by key;
select sequence, id, parent_id, item
  from transcript_entries where session_id = '<SESSION>' order by sequence;
select id, kind, status, attempt_id, payload, result
  from actions where session_id = '<SESSION>' order by created_at;
select id, type, payload
  from events where session_id = '<SESSION>' order by id;
select priority, status, client_input_id, content
  from queued_inputs where session_id = '<SESSION>' order by created_at;
```

### 1. Draft Start, Basic Turn, And Replay

1. `session.start` with metadata `{ "harness": true, "created_by": "web" }`
   and a stable client-chosen `session_id`.
2. `events.subscribe` with `after_event_id: null`.
3. Capture `model.requested.data.action_row_id`.
4. `harness.model.complete` with a text assistant message.
5. `session.get` and `history.context`.
6. Reconnect and `events.subscribe` with the previous `last_event_id`.
7. Retry `session.start` with the same stable `session_id`.

Verify:

- `input.accepted`, `turn.started`,
  `model.requested`, `model.completed`, `assistant.message`, and `session.idle`
  are observed while the turn is active.
- No durable empty web session is created before the first message, the model
  action is completed, and the active leaf ends at a turn boundary.
- The repeated `session.start` returns `"replayed": true` and does not append a
  duplicate user message.
- Replay returns only buffered events with `id > after_event_id`; once the
  session is idle, the event buffer for that session is empty.
- Initial subscribe with `after_event_id: null` does not replay historical
  events; it only attaches to future live events.

### 2. Global System Prompt Configuration

1. Call `config.get`.
2. Call `config.set` with a string prompt.
3. Create and complete a harness or real-provider turn.
4. Call `config.set` with `"system_prompt": null`.

Verify the prompt is global daemon configuration, not session metadata, and
`session.get` never returns a per-session prompt.

### 3. Image Input Persistence

Send text plus an image block using both `url` and `base64` forms in separate
turns. Complete the model through the harness.

Verify the exact `mime_type` and source survive `history.context` and the
stored transcript.

### 4. Queueing, Steering, And Idempotency

1. Start a turn and leave its model action running.
2. Queue two normal follow-ups with `input.follow_up`.
3. Promote the second queued input with `input.promote_queued`.
4. Send the same normal follow-up again with the same `client_input_id`.
5. Complete the first model action.

Verify only one normal row and one normal `input.queued` event exist, no queued
input is marked `consumed` while a model action is running, the promoted steer
is consumed before the unpromoted follow-up at the next eligible boundary, and
each queued input is claimed as `consuming` before becoming transcript. When
the eligible boundary is a tool-to-model continuation, verify the promoted steer
appears after the tool results and before the next model action without a
`turn_finished` between them.
`input.promote_queued` must promote before claim and return `"promoted": false`
with the current row status once the row is `consuming` or `consumed`.

Also verify an idle `input.follow_up` retried with the same `client_input_id`
returns `"replayed": true`, leaves exactly one user-message transcript entry,
and records a single consumed input ledger row.

### 5. Interrupt And Stale Completion

1. Start a turn and leave the model action running.
2. Send `input.interrupt`.
3. Try `harness.model.complete` for the stale `action_row_id`.

Verify the turn finishes as `Interrupted`, unfinished actions become
`interrupted` or stale, the stale completion is rejected with `stale_action`,
and the assistant text is not appended.

### 5a. Retry And Continue

1. Start a harness-backed turn and fail its model action with
   `harness.model.fail`.
2. Call `turn.resume` on the crashed terminal leaf.
3. Complete the resumed model action with `harness.model.complete`.
4. Repeat with `input.interrupt` instead of `harness.model.fail`.

Verify both retry and continue set activity back to `running`, create a new
model action from the original checkpoint, leave exactly one copy of the user
message in the transcript forest, preserve the old crashed/interrupted terminal
branch off the active path, and finish gracefully once the resumed action
completes. Also verify `turn.resume` rejects an interrupted tool-running turn
with `not_resumable`.

### 6. Rewind Lifecycle

1. Create two completed turns and record boundary leaf ids.
2. Start a third pending turn.
3. Attempt `history.rewind` to the first boundary.
4. Interrupt, wait for idle, then rewind again.
5. Attempt rewind to a user-message entry.

Verify running rewind fails with `session_busy`, post-interrupt rewind succeeds,
descendant rows are preserved, and non-boundary rewind fails with
`not_turn_boundary`. Also verify stale picker requests with a mismatched
`expected_active_leaf_id` fail with `history_changed`.

### 7. Fork Lifecycle

1. Start a pending turn on a session that has older transcript entries.
2. Fork from an older boundary into a new session.
3. Try fork with `leaf_id: null`.
4. Fork from a non-boundary/open-turn entry.

Verify running-safe fork succeeds without mutating the source active leaf,
fork-from-null fails with `missing_leaf_id`, and fork from open/non-boundary
history succeeds as a copied branch in the new session.

### 8. Real Tools

Use `harness.model.complete` to request real tools:

- `read` success and missing-file error.
- `write` success.
- `edit` success and missing-target error.
- `bash` success, non-zero exit, malformed args, and timeout.
- Multiple tool calls in one assistant response.

Verify there is no approval event, tools emit `tool.requested`/`tool.started`,
tool-returned errors append error `ToolResult`s, action rows for tool failures
are `error`, and the next model request sees tool results in the assistant's
declared order.

### 9. Compaction Validity

1. Request compaction on an idle session.
2. Observe `compaction.requested`, then let the daemon/provider complete it.
3. Verify the new active leaf is a `compaction_summary` root with
   `parent_id = null`, `source_session_id`, `source_leaf_id`, and
   `last_turn_id`.
4. Queue a normal follow-up while compaction is running and verify it is not
   consumed until `compaction.completed` or `compaction.error`.
5. Request compaction while a model action is running.

Verify compaction emits `compaction.completed` and `history.compacted`, the
old source branch remains in `history.tree`, and running compaction requests
fail with `session_busy`.

### 10. Daemon Death Recovery

1. Start a harness turn and leave the model action running.
2. Kill the daemon process.
3. Restart the daemon.
4. `events.subscribe` and `session.get` for the same session.

Verify unfinished actions are stale, recovery appends a crashed turn tail,
`turn.finished` and `session.recovered` are replayable, and no explicit resume
RPC is needed.

### 11. Browser User Flows

Run the TypeScript web UI against the same daemon and Postgres database.

Verify:

- Markdown and raw HTML assistant text render in the transcript.
- Slash autocomplete uses Enter once to complete and the next Enter to execute.
- `/switch` and `/fork` open pickers; there is no raw-id path in the UI.
- `/switch` is idle-only. Switching to a user-message target restores that
  historical text into the composer; switching to a completed turn or
  compaction root changes the active leaf inside the same session.
- `/new`, `/retry`, `/continue`, `/rename`, `/archive`, `/unarchive`,
  `/rewind`, and `/tree` are not part of the user-facing slash surface.
- Crashed and interrupted terminal model turns show Retry/Continue actions that
  invoke the `turn.resume` RPC.
- Fork from a user-message target creates a child and restores the historical
  text into the child composer.
- A brand-new local draft survives browser refresh without creating a durable
  empty session.

### 12. Real Codex Provider

With `~/.codex/auth.json` or `CODEX_ACCESS_TOKEN` available:

```json
{
  "method": "session.start",
  "params": {
    "session_id": "manual_real_codex",
    "provider": {
      "kind": "codex",
      "model": "gpt-5.5",
      "prompt_cache": { "key": "pi-relay-real-smoke" }
    },
    "metadata": { "manual": "real-codex" }
  }
}
```

Set the global prompt separately if needed:

```json
{
  "method": "config.set",
  "params": { "system_prompt": "Reply exactly and briefly." }
}
```

Then send:

```json
{
  "method": "input.follow_up",
  "params": {
    "session_id": "manual_real_codex",
    "client_input_id": "ci-real-codex",
    "content": [
      { "type": "text", "text": "Reply with exactly: websocket real codex ok" }
    ]
  }
}
```

Verify a real `model.completed`, assistant transcript entry, prompt-cache key
request path, and `session.idle`.

For image support, send a public image URL, for example:

```json
{
  "type": "image",
  "image": {
    "mime_type": "image/png",
    "source": {
      "kind": "url",
      "value": "https://raw.githubusercontent.com/github/explore/main/topics/rust/rust.png"
    }
  }
}
```

Verify the real response references the image and `history.context` still
contains the original image block.

### 13. Real Anthropic Provider

Run only when `ANTHROPIC_API_KEY` is present. Create a session with
`provider.kind = "anthropic"` or `"claude"` and ask the model to use the `read`
tool. Verify provider-requested tool calls, real tool results, a second model
request containing those results, and a final assistant message.

## Documentation Sync Rule

Whenever a crate boundary, RPC method, lifecycle rule, provider credential
path, storage invariant, or manual exercise changes, update this file,
`rust/docs/architecture.md`, `rust/README.md`, crate READMEs as needed, and
`rust/WORKLOG.md` in the same change.
