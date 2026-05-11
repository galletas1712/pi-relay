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
   `session.get`. `session.create` remains available for harness/manual uses
   that need an empty durable session.

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
   If the daemon dies with an open turn, the next daemon repairs the session on
   first touch by marking unfinished actions stale and appending a crashed turn
   tail. External side effects, such as files written by tools, are not
   transactional.

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
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
active_leaf_id text null
provider_config jsonb not null
metadata jsonb not null default '{}'::jsonb
```

`provider_config`:

```json
{
  "kind": "codex",
  "model": "gpt-5.5",
  "prompt_cache": { "key": "pi-relay-local" }
}
```

Supported provider kinds are:

- `codex`: OpenAI Responses API through the ChatGPT/Codex backend. Uses
  `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`, plus `ChatGPT-Account-ID` when
  available, and sends the Codex residency routing header required by
  workspace-backed ChatGPT accounts. This is the path tested with real
  credentials.
- `openai`: OpenAI Chat Completions through `OPENAI_API_KEY` or
  `~/.codex/auth.json` if it contains an API key.
- `anthropic` or `claude`: Anthropic Messages API through `ANTHROPIC_API_KEY`.

`prompt_cache.key` maps to `ModelRequest::prompt_cache_key` and is sent on the
OpenAI request path. The Chat Completions path uses a stable default cache key
when this field is omitted. `max_tokens` is optional; when omitted the daemon
does not set an OpenAI/Codex output cap.

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
`consuming` and recording a claim id in `origin`. `input.replace_queued` and
`input.cancel_queued` only work while a row is still `queued`. The daemon marks
the row `consumed` in the same transaction that appends the corresponding
transcript/action events, and validates the claim id before doing so. On daemon
restart, abandoned `consuming` rows are reset to `queued` before recovery
continues.

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

Append-only websocket event log:

```text
id bigserial primary key
session_id text not null references sessions(id) on delete cascade
type text not null
payload jsonb not null
created_at timestamptz not null default now()
```

`events.subscribe(after_event_id)` returns missed rows in the RPC response and
then streams live event frames on the same websocket.

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

### `session.create`

Creates an empty durable session row. The web UI does not use this for blank
new chats; it keeps those as browser-local drafts and calls `session.start`
when the first message is sent.

```json
{
  "session_id": "optional",
  "provider": {
    "kind": "codex",
    "model": "gpt-5.5",
    "prompt_cache": { "key": "pi-relay-local" }
  },
  "metadata": {}
}
```

Result:

```json
{ "session_id": "s1", "activity": "idle" }
```

### `session.start`

Creates a durable session and immediately feeds the first user message. This is
the normal frontend path for a brand-new draft.

```json
{
  "session_id": "optional-stable-id",
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
provider/tool work. Retrying the same stable `session_id` returns the existing
session with `"replayed": true` rather than creating a second session.
For web drafts, the frontend should always provide both the stable draft-owned
`session_id` and `client_input_id`.

### `session.list`

Lists durable sessions, newest first.

```json
{ "limit": 50 }
```

Each row includes `session_id`, `activity`, `active_leaf_id`, `provider`,
`metadata`, and `updated_at`. Defensive listing hides accidental empty
web-created rows that have no transcript, queued input, actions, or fork
provenance. Rows with `metadata.hidden = true` are also omitted from the list;
this is used for local verification cleanup, not as a core lifecycle state.
Browser-local drafts are not returned by this RPC.

### `session.get`

Recovers the session if needed, then returns a durable snapshot.

```json
{ "session_id": "s1" }
```

Result shape:

```json
{
  "session_id": "s1",
  "activity": "idle",
  "active_leaf_id": "entry_9",
  "provider": { "kind": "codex", "model": "gpt-5.5" },
  "metadata": {},
  "pending_actions": [],
  "queued_inputs": [],
  "last_event_id": 42
}
```

`queued_inputs` contains live queued or consuming user inputs with `input_id`,
`priority`, `status`, `content`, `client_input_id`, `created_at`, and optional
`promoted_at`. The web UI uses it for the composer-adjacent queue pane.

### `session.configure`

Idle-only. Replaces provider config and/or metadata.

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

### `input.steer`

High-priority user message. This remains a backend primitive, but the web UI
does not expose it as a slash command. Normal text sends use
`input.follow_up`; users promote queued follow-ups with
`input.promote_queued`.

### `input.promote_queued`

Promotes a still-queued follow-up into the steer queue. Promotions are consumed
in promotion order before remaining follow-ups.

```json
{ "session_id": "s1", "input_id": "input_..." }
```

Rows already `consuming`, `consumed`, cancelled, or already steering fail with
`input_already_consuming`.

### `input.replace_queued`

Replaces a queued input that has not yet been claimed by the daemon.

```json
{
  "session_id": "s1",
  "input_id": "input_...",
  "content": [
    { "type": "text", "text": "Edited queued text" }
  ]
}
```

Rows already `consuming` or `consumed` fail with
`input_already_consuming`; at that point the UI should use interrupt plus
rewind if the message is visible in transcript history.

### `input.cancel_queued`

Cancels a queued input that has not yet been claimed.

```json
{ "session_id": "s1", "input_id": "input_..." }
```

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
mutate the source, so it can run while the source is busy. If the target entry
is inside an open turn, the child receives an interrupted turn finish after the
copied prefix.

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
and creates a running compaction action. The current implementation expects the
development harness to complete or fail that action.

## Development Harness RPC

Harness methods are development-only controls for exercising lifecycle edges
while still using the real websocket router, Postgres repository, session FSM,
and event log.

Implemented harness methods:

- `harness.model.complete`
- `harness.model.fail`
- `harness.compaction.complete`
- `harness.compaction.fail`

There is deliberately no `harness.tool.complete` or `harness.tool.timeout`.
Tool behavior is tested by letting the real builtin tools run.

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

### `harness.compaction.complete`

```json
{
  "session_id": "s1",
  "action_row_id": "action_...",
  "replacement": {
    "items": []
  }
}
```

Invalid replacement contexts produce `compaction.error`, leave the previous
active context intact, and persist the compaction action as `error`.

## Event Set

Current durable event names:

```text
session.created
session.configured
input.accepted
input.queued
input.consumed
input.replaced
input.promoted
input.cancelled
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
2. `events.subscribe`.
3. Capture `model.requested.data.action_row_id`.
4. `harness.model.complete` with a text assistant message.
5. `session.get` and `history.context`.
6. Reconnect and `events.subscribe` with the previous `last_event_id`.
7. Retry `session.start` with the same stable `session_id`.

Verify:

- `input.accepted`, `turn.started`,
  `model.requested`, `model.completed`, `assistant.message`, and `session.idle`
  are durable.
- No durable empty web session is created before the first message, the model
  action is completed, and the active leaf ends at a turn boundary.
- The repeated `session.start` returns `"replayed": true` and does not append a
  duplicate user message.
- Replay returns only events with `id > after_event_id`.

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
is consumed before the unpromoted follow-up after the boundary, and each queued
input is claimed as `consuming` before becoming transcript.
`input.replace_queued`, `input.cancel_queued`, and `input.promote_queued` must
succeed before claim and fail once the row is `consuming` or `consumed`.

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
2. Complete with a valid replacement context.
3. Request compaction again and complete with an invalid context, such as a tool
   result without a matching tool call.
4. Request compaction while a model action is running.

Verify valid compaction emits `compaction.completed` and `history.compacted`,
invalid compaction emits `compaction.error` and leaves `active_leaf_id`
unchanged, and running compaction fails with `session_busy`.

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
- `/rewind` and `/fork` open pickers; there is no raw-id path in the UI.
- Rewind to a user-message target restores that historical text into the
  composer.
- Fork from a user-message target creates a child and restores the historical
  text into the child composer.
- A brand-new local draft survives browser refresh without creating a durable
  empty session.

### 12. Real Codex Provider

With `~/.codex/auth.json` or `CODEX_ACCESS_TOKEN` available:

```json
{
  "method": "session.create",
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
