# Websocket RPC Contract

This is the frontend-facing control plane implemented by `agent-daemon`
(`pi-agentd`). It is intentionally small and personal-use oriented: Postgres is
the durable source of truth, websocket connections are only observers/controllers,
tools always run when requested, and there is no approval interface.

The goal of the contract is to make every user-facing behavior testable by
sending the same websocket frames a frontend would send.

## Core Decisions

1. Sessions are durable rows, not opened processes.
   There is no user-facing `open` or `close` RPC, and no session-level `resume`
   (idle sessions resume implicitly - see below). A frontend starts a new chat
   with `session.start` when the first message is sent, subscribes with
   `events.subscribe`, and gets the current state with `session.get`.
   `session.delete` exists and removes an idle session with its transcript,
   queue, action, and event rows. `turn.resume` is a turn-level operation
   (re-run an interrupted/crashed terminal turn), not a session lifecycle
   command. Empty durable sessions are not part of the websocket contract; a
   new chat becomes durable only through `session.start`.

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
   `history.switch`, `session.configure`, and `compaction.request` fail with
   `session_busy` while work is active or queued. Here idle means there are no
   unfinished actions and no queued inputs waiting to become transcript. A
   frontend should send `input.interrupt`, wait for idle, then retry.

6. Tools are always allowed.
   The daemon runs model-requested tools immediately. There is no approval or
   denial RPC. `input.interrupt` is the one user-facing cancellation command and
   interrupts active work. The daemon keeps a per-action task registry and
   aborts registered model, tool, and compaction futures for the interrupted
   session on a best-effort basis; durable action status remains the source of
   truth for stale completions.

7. Daemon death is recoverable state.
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

Session-owned mutations take a row lock on the target `sessions` row before
validating or writing related transcript/action/queue state:

```sql
select id from sessions where id = $1 for update;
```

This serializes one session at a time; it is not a table-wide or database-wide
lock, and provider/tool work never runs while the lock is held. Fresh databases
get the revision/order columns from the schema below. The current development
database has already been upgraded manually; the daemon does not run old-session
migrations automatically.

### `sessions`

```text
id text primary key
project_id uuid null references projects(id)
outer_cwd text not null
workspaces jsonb not null default '[]'::jsonb
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
active_leaf_id text null
system_prompt text not null
provider_config jsonb not null
metadata jsonb not null default '{}'::jsonb
parent_session_id text null references sessions(id) on delete set null
subagent_type text null
delegation_id text null references delegations(id)
session_revision bigint not null default 0
queue_revision bigint not null default 0
transcript_revision bigint not null default 0
```

### `projects`

```text
id uuid primary key
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
name text not null
metadata jsonb not null default '{}'::jsonb
workspaces jsonb not null default '[]'::jsonb
```

Projects define workspace sources. Git workspaces use
`{ kind: "git", workspace_dir, remote_url, remote_branch }`; local folder
workspaces use `{ kind: "local", workspace_dir, source_path }`. Legacy Git
records without `kind` are treated as `kind: "git"`. When a project session
starts, the daemon creates private workspace directories under the session
`outer_cwd`. For each project workspace, the daemon also maintains a managed
per-project workspace base under its state directory. At session start, Git
bases are destructively refreshed to the configured remote branch head with
`git fetch`/`reset --hard`/`clean -ffdx`; local folder bases are destructively
refreshed from `source_path` with `rsync --delete`. Session workspaces are then
instantiated from those bases, preferring Btrfs subvolume snapshots on
Btrfs-backed state storage and falling back to reflink/copy behavior when CoW is
not available.
Ephemeral host sessions have `project_id: null`, no project workspaces, and use
`$HOME` as `outer_cwd`. Model prompt context and local tools use the session's
stored `outer_cwd`.

`provider_config`:

```json
{
  "kind": "openai",
  "model": "gpt-5.5",
  "reasoning_effort": "xhigh",
  "prompt_cache": { "key": "pi-relay-local" }
}
```

Supported provider kinds are exactly two (`ProviderKind = OpenAi | Claude`):

- `openai`: OpenAI Responses API. It always routes through the ChatGPT/Codex
  subscription transport - it reads `CODEX_ACCESS_TOKEN` or `~/.codex/auth.json`,
  adds `ChatGPT-Account-ID` when available, and sends the Codex residency
  routing header required by workspace-backed ChatGPT accounts. pi-relay
  intentionally does not support plain OpenAI API-key auth for OpenAI models.
  This is the path tested with real credentials.
- `anthropic` or `claude`: Anthropic Messages API through `ANTHROPIC_API_KEY`.

`codex` is **not** a provider kind; it is the auth transport used by the
`openai` kind. A request with `"kind": "codex"` is rejected at decode time.

`prompt_cache.key` maps to `ModelRequest::prompt_cache_key` and is sent on the
OpenAI request path. `max_tokens` is optional; when omitted the daemon does not
set an OpenAI output cap.

`reasoning_effort` defaults to `medium`. OpenAI currently accepts `none`,
`minimal`, `low`, `medium`, `high`, and `xhigh` in pi-relay. Claude accepts
`low`, `medium`, `high`, `xhigh`, and `max`; Claude Opus 4.8 requests are sent
with adaptive thinking and `output_config.effort`.

### `daemon_config`

Reserved daemon key-value configuration table.

```text
key text primary key
value jsonb not null
updated_at timestamptz not null default now()
```

The `PI.md` is the prompt composition template. It is not
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
provider_replay jsonb not null default '[]'::jsonb
turn_id bigint null
sequence bigserial not null
primary key (session_id, id)
```

The active context is the root-to-`active_leaf_id` path. History switch moves the
active leaf; it does not delete rows.

`provider_replay` is a sidecar for provider-native replay records, aligned one
to one with the visible transcript entry. It is not rendered as a visible
transcript item. When the daemon builds a model request, it materializes a
`ModelContext` from the selected root-to-leaf path as `{ item, provider_replay }`
entries, so OpenAI/Anthropic continuation state follows switch and
compaction branches without being duplicated in action payloads.

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
updated_at timestamptz not null default now()
follow_up_position integer null
client_input_id text null
```

`client_input_id` is unique per session when present. Busy-session retries do
not enqueue duplicate rows or emit a second `input.queued` event. Idle accepted
inputs are also recorded here with `status='consumed'` in the same transaction
that appends transcript/action/event state, so retrying a lost websocket
response does not append the user message twice. Busy-session rows stay
`queued` while model/tool/compaction work is unfinished, so a daemon crash
cannot lose accepted input that has not yet appeared in the transcript.

New queue consumption does not claim rows by moving them to `consuming`.
Instead, the daemon peeks the next `queued` row and later marks that same row
`consumed` in the transcript/action/event transaction. The commit validates both
the row version and that the row is still the canonical next queued input, so an
edit/cancel/reorder or a new steer above it causes the stale daemon cursor to
fail and reload from Postgres. The legacy `consuming` status remains only for
rows produced by older daemons; on restart/touch, abandoned `consuming` rows are
reset to `queued`.

Follow-up order is dense and explicit. Steering rows always sort above
follow-ups and are ordered by steering/promote time. Queued follow-ups sort by
`follow_up_position`, then creation time and id. The frontend reorders by
sending the complete ordered follow-up id list; the backend rewrites dense
positions.

### `actions`

Durable external work:

```text
id text primary key
session_id text not null references sessions(id) on delete cascade
turn_id bigint null
action_id bigint not null
attempt_id text not null
kind text not null              -- model | tool | compaction
status text not null            -- pending | blocked | running | completed | error | interrupted | stale
payload jsonb not null
result jsonb null
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
```

`attempt_id` prevents stale completions from a prior daemon attempt from
mutating the transcript after interrupt/recovery.

#### Model action payloads

Model actions store a context leaf reference, not a full replay of the model
context:

```json
{
  "context_leaf_id": "entry_...",
  "context_tokens": 123
}
```

`context_leaf_id` is the transcript entry that was active when the model request
was created. The full provider request context is derived from
`transcript_entries` by walking the parent chain from that leaf and preserving
each entry's `provider_replay` sidecar. This keeps normal `actions` rows,
`action.requested` events, and reconnect snapshots small while retaining exact
restart/recovery semantics.

Live dispatch is still zero-extra-fetch: the persisted action returned from
`persist_outputs` carries the in-memory `SessionAction::RequestModel`, including
its already-materialized `ModelContext`. The context-leaf reconstruction path is
used only when a pending/blocked model action has to be rebuilt from Postgres,
such as daemon restart, pending-action dispatch recovery, or mid-turn
compaction resume.

History operations remain transcript-driven:

- switch updates `sessions.active_leaf_id`; existing model action rows
  keep their explicit `context_leaf_id`, so recovery never depends on a mutable
  active leaf;
- export reads transcript branches and visible transcript items, not model
  action payloads, so provider-native replay state stays out of exported text.

The old embedded `model_context` payloads have been migrated away. Model action
recovery now requires `context_leaf_id`.

### `delegations`

Durable parent/child subagent delegation units:

```text
id text primary key
parent_session_id text not null references sessions(id) on delete cascade
workflow text null
label text null
kind text not null              -- full | readonly_fanout
status text not null            -- running | done | done_with_failures | cancelled | failed
attempt_id text not null
expected_subagents integer not null default 1
created_at timestamptz not null default now()
updated_at timestamptz not null default now()
```

Child sessions link back through `sessions.delegation_id`. The
`delegations_parent_created_idx` index supports the per-parent run-board feed.
The completion runner uses `attempt_id` as an idempotency fence and queues a
deterministic parent steer keyed as `delegation-steer:<delegation_id>:<attempt_id>`.

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
clients should load durable state through `session.get` and, when they need
compact full-tree topology, `transcript.index` instead.
When a session reaches idle, the daemon publishes `session.idle` to live
subscribers and then clears that session's event rows. Idle-only mutations such
as configuration changes and same-session history switching also clear their
session event buffers after live publication. Durable
session state lives in `sessions`, `transcript_entries`, `queued_inputs`, and
`actions`; old toast-worthy events such as `model.error` are not retained as
history.
Parent-scoped `subagent.*` lifecycle events are written to the parent session's
stream, not the child stream. They remain replayable until the parent session's
normal event-buffer cleanup so a parent can reconnect and observe child
completion without polling.

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
- Switch to a non-boundary transcript entry.
- Transcript rows committed without the matching action/event updates.

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
    "kind": "openai",
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

Omit `project_id` to start an ephemeral host session. Ephemeral sessions are not
assigned to a project, have no project workspace records, and use `$HOME` as
their `outer_cwd`.

By default a project session materializes every workspace the project declares.
Pass an optional `workspaces` array to scope the session to a subset, so unrelated
workspace directories — along with their `AGENTS.md` files and skills — stay out of
the session `outer_cwd` and the rendered prompt:

```json
{
  "workspaces": [
    { "workspace_dir": "repo-a" },
    { "workspace_dir": "repo-b", "branch": "feature/login" }
  ]
}
```

Each entry names a `workspace_dir` declared by the project and may set an optional
`branch` to populate that session's git workspace from a branch other than the
project's configured `remote_branch`. The override only affects the session's own
copy: the daemon fetches the branch into the session workspace after instantiating
it from the managed base, leaving the shared per-project base on the project's
configured branch. Branch overrides are only valid for git workspaces. The daemon
rejects (`invalid_params`) a `workspaces` array that is empty, names a directory the
project does not declare, repeats a workspace, or sets `branch` on a local-folder
workspace. Selected workspaces are materialized in the project's declared order
regardless of request order. The field is ignored for ephemeral sessions.

The daemon writes `session.created`, `input.accepted`, transcript entries,
actions, and events in the same session-start transition before dispatching
provider/tool work. For project sessions it snapshots the project's current
`workspaces` into the new session row and assigns a per-session `outer_cwd`;
later `project.update` calls do not change existing sessions. Retrying the same stable
`session_id` returns the
existing session with `"replayed": true` rather than creating a second session.
For web drafts, the frontend should always provide both the stable draft-owned
`session_id` and `client_input_id`.

### `session.list`

Lists durable sessions by last user-message timestamp, latest first.

```json
{ "limit": 50 }
```

Pass a `project_id` to list that project's sessions. Omit `project_id` to list
ephemeral host sessions.

Each row includes `session_id`, nullable `project_id`, `outer_cwd`, `workspaces`,
`activity`, `active_leaf_id`, `provider`, `metadata`, `updated_at`,
`last_user_message_timestamp_ms`, and `has_transcript_entries`. Archived rows
remain at the end of the list. Sessions without user messages sort after
sessions that have user messages, then by creation time. Defensive listing hides
accidental empty web-created rows that have no transcript, queued input, or
actions. Rows with `metadata.hidden = true` are also omitted from the list; this
is used for local verification cleanup, not as a core lifecycle state.
Browser-local drafts are not returned by this RPC.

### `session.get`

Recovers the session if needed, then returns a durable snapshot.

```json
{ "session_id": "s1", "include_entries": true, "entries_scope": "active_branch" }
```

Result shape:

```json
{
  "session_id": "s1",
  "project_id": "f2b0e23c-1fd7-4977-9d60-f6842e25d15b",
  "outer_cwd": "/home/me/.local/state/pi-relay/sessions/s1/cwd",
  "workspaces": [
    {
      "workspace_dir": "repo-a",
      "remote_url": "https://github.com/me/repo-a.git",
      "remote_branch": "main",
      "base_sha": "8e9b2f4b7c2c7f0ef2e3b6f0e5ef4f1b18b3b111",
      "local_branch": "pi/session/s1/repo-a"
    },
    {
      "workspace_dir": "repo-b",
      "remote_url": "git@github.com:me/repo-b.git",
      "remote_branch": "staging",
      "base_sha": "9f7a2b4b2c3d4e5f60718293a4b5c6d7e8f90123",
      "local_branch": "pi/session/s1/repo-b"
    }
  ],
  "activity": "idle",
  "active_leaf_id": "entry_9",
  "provider": { "kind": "openai", "model": "gpt-5.5" },
  "metadata": {},
  "pending_actions": [],
  "queued_inputs": [],
  "session_revision": 12,
  "queue_revision": 5,
  "transcript_revision": 7,
  "last_event_id": 42,
  "last_user_message_timestamp_ms": 1717799900000,
  "server_time_ms": 1717800000000,
  "has_transcript_entries": true,
  "entries": []
}
```

`queued_inputs` contains live queued or consuming user inputs with `input_id`,
`priority`, `status`, `content`, `client_input_id`, `created_at`, `updated_at`,
optional `promoted_at`, and optional `follow_up_position`. The web UI uses it
for the composer-adjacent queue pane. `session_revision`,
`queue_revision`, and `transcript_revision` are monotonically increasing
per-session counters for replacing stale cached views instead of inferring
patches from partial events. `transcript_revision` changes when transcript rows
change; active-leaf-only view changes use `session_revision` without changing
the transcript data counter.
`entries` is included only when `include_entries` is true. `entries_scope` may
be `"active_branch"` or `"full_tree"` and defaults to `"full_tree"` for
compatibility. The web UI can use the active-branch scope for normal display and
reserve the full tree for switch/history UI. `has_transcript_entries`
allows provider/model lock checks even when the active branch is empty after a
root switch. `server_time_ms` is the daemon's wall-clock (ms) at response time,
used by the UI to anchor live timers against server time rather than client
clocks. Transcript entries returned by websocket RPCs include the Postgres
`sequence` column, which is an append/order cursor but not itself a freshness
token. Use `transcript_revision` to decide whether cached transcript data is
fresh.

UI transcript body responses do not include the raw `provider_replay` sidecar.
Replay remains stored durably for model continuation, but websocket RPCs expose
only semantic transcript entries: `id`, `parent_id`, `timestamp_ms`, `sequence`,
and `item`.

For UI display, `"active_branch"` follows compaction provenance: a
`compaction_summary` entry with `parent_id = null` is rendered after its
`source_leaf_id` lineage when that source leaf is available. This does not change
the persisted transcript topology or model-visible ancestry; model context and
`history.context` continue to follow stored `parent_id` links only.

## Transcript RPC

### `transcript.index`

Returns compact transcript topology without full message bodies or
`provider_replay`. This is the normal history-picker endpoint.

```json
{ "session_id": "s1", "after_sequence": 0, "limit": 1000 }
```

Result shape:

```json
{
  "session_id": "s1",
  "active_leaf_id": "entry_9",
  "session_revision": 12,
  "transcript_revision": 7,
  "after_sequence": 0,
  "max_sequence": 42,
  "complete": true,
  "nodes": [
    {
      "id": "entry_1",
      "parent_id": null,
      "source_leaf_id": null,
      "timestamp_ms": 123,
      "sequence": 1,
      "item_type": "user_message",
      "turn_id": null,
      "outcome": null,
      "can_switch_to": false,
      "edit_target_leaf_id": null,
      "display_hint": "hello"
    }
  ]
}
```

`transcript_revision` is the freshness token. `sequence` is only the pagination
cursor for fetching more rows in the same transcript revision. `display_hint` is
best-effort UI copy; clients must not treat it as authoritative transcript
content. `can_switch_to` is daemon-computed boundary truth for direct switch
targets. Editing a historical user message still switches to the previous turn
boundary, which clients can derive from compact topology and the daemon validates
again in `history.switch`.

### `transcript.entries`

Fetches sparse transcript bodies by explicit entry IDs. This is for cases where
the UI has compact topology but needs one or a few bodies, such as
restoring a historical user message into the composer.

```json
{ "session_id": "s1", "entry_ids": ["entry_1", "entry_7"] }
```

Result shape:

```json
{
  "session_id": "s1",
  "session_revision": 12,
  "transcript_revision": 7,
  "entries": []
}
```

### `transcript.turns`

Returns a newest/tail page of active-branch turn cards for the selected-session
transcript view. This is the normal hot-path UI endpoint. Each card carries full
semantic user-message entries for that turn and the full final semantic
assistant-message entry for that turn; intermediate tool calls/results are
fetched with `transcript.turn_detail` when a card is expanded. It omits raw
provider replay.

```json
{ "session_id": "s1", "limit": 50, "before_entry_id": "entry_17" }
```

`before_entry_id` is optional. Omit it to fetch the newest/tail page ending at
the active leaf. To fetch older cards, pass the `next_before_entry_id` returned
by the previous page. The server clamps `limit` to a small maximum.

Result shape:

```json
{
  "session_id": "s1",
  "active_leaf_id": "entry_42",
  "session_revision": 12,
  "transcript_revision": 7,
  "before_entry_id": null,
  "next_before_entry_id": "entry_17",
  "has_more_before": true,
  "limit": 50,
  "cards": [
    {
      "id": "entry_42",
      "turn_id": 7,
      "status": "completed",
      "outcome": "Graceful",
      "start_entry_id": "entry_37",
      "boundary_entry_id": "entry_42",
      "active_leaf_id": "entry_42",
      "start_sequence": 37,
      "end_sequence": 42,
      "start_timestamp_ms": 1700000000000,
      "timestamp_ms": 1700000006000,
      "user_messages": [],
      "assistant_message": null,
      "summary": null,
      "can_resume": false
    }
  ]
}
```

### `transcript.turn_detail`

Fetches the UI-projected entry bodies for one turn card, used when expanding a
collapsed turn. The request uses the `card_id`, `active_leaf_id`,
`start_sequence`, and `end_sequence` returned by `transcript.turns`, so the
daemon can fetch only the requested card path instead of recomputing all cards.

```json
{
  "session_id": "s1",
  "card_id": "entry_42",
  "leaf_id": "entry_42",
  "start_sequence": 37,
  "end_sequence": 42
}
```

Result shape:

```json
{
  "session_id": "s1",
  "active_leaf_id": "entry_42",
  "session_revision": 12,
  "transcript_revision": 7,
  "card_id": "entry_42",
  "entries": []
}
```

## Project RPC

### `project.list`

Returns visible projects:

```json
{ "projects": [] }
```

Each project has `project_id`, `name`, `workspaces`, `metadata`, `created_at`,
and `updated_at`. Project workspaces are defaults for new sessions; each session
snapshots its own values at creation time.

### `project.create`

```json
{
  "name": "my repo",
  "workspaces": [
    {
      "kind": "git",
      "workspace_dir": "repo-a",
      "remote_url": "https://github.com/me/repo-a.git",
      "remote_branch": "main"
    },
    {
      "kind": "local",
      "workspace_dir": "reference-docs",
      "source_path": "/home/me/reference-docs"
    }
  ],
  "metadata": { "created_by": "web" }
}
```

### `project.update`

Renames a project and/or changes the workspace sources used for future sessions.
Each `workspace_dir` must be a direct child name and must not start with `.`.
For Git workspaces, `remote_url` must be reachable by `git ls-remote`, and
`remote_branch` must name an existing branch on that remote. For local folder
workspaces, `source_path` must be an existing directory on the daemon host.
Changing a workspace's name, kind, Git remote/branch, or local source path
causes the managed workspace base for that slot to be discarded and recreated
for future sessions.
Updating a project does not change existing sessions in that project.

```json
{
  "project_id": "f2b0e23c-1fd7-4977-9d60-f6842e25d15b",
  "name": "pi-relay",
  "workspaces": [
    {
      "kind": "git",
      "workspace_dir": "pi-relay",
      "remote_url": "https://github.com/galletas1712/pi-relay.git",
      "remote_branch": "main"
    }
  ]
}
```

### `project.delete`

Deletes an **empty** project. Params: `project_id`. Fails with
`project_not_empty` if the project is missing or still has any session; on
success it removes the project's managed workspace bases and returns
`{ "project_id", "deleted": true }`.

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
{
  "session_id": "s1",
  "title": "Production deploy notes",
  "metadata": { "title": "Production deploy notes" },
  "activity": "idle"
}
```

Emits `session.configured` with patchable `metadata`/`title` fields so
subscribed clients can update lists/snapshots without a transcript refresh.

### `session.configure`

Idle-only for metadata or model/source-changing updates. Replaces provider
config and/or metadata. Once a session has any transcript entry, `provider.kind`
and `provider.model` are locked; clients may still change provider-adjacent
knobs such as `reasoning_effort` during or between turns. Runtime changes apply
to subsequently created provider requests, not to an already in-flight request.
Responses and `session.configured` events include `provider`, `metadata`, and
`activity` so clients can patch cached summaries and selected snapshots.

### `session.sync_active_branch`

Cheap incremental reconciliation of the selected session's active branch.
Params: `session_id` and an optional `base_leaf_id` (the leaf the client already
has). The daemon recovers the session if needed, then returns only the delta
against `base_leaf_id` with a `status` of `unchanged`, `extended` (a suffix of
new entries appended past the client's leaf), or `branch_changed` (the active
branch diverged, so the client replaces its body cache). The response carries
the UI-projected entries (no raw `provider_replay`) plus the session overview
(`active_leaf_id`, revisions, `server_time_ms`). This is the normal selected-
session refresh path; full `session.get(include_entries=true)` is reserved for
cold loads and history UI.

### `session.delete`

Idle-only. Params: `session_id` (and optional `expected_active_leaf_id`).
Acquires the session driver, requires idle (no unfinished action, no queued
input), evicts any live session, removes the row, and cascades to its transcript
entries, queued inputs, actions, and events; session workspace directories are
cleaned up. Returns `{ "session_id", "deleted": true }`. A missing session is
`session_not_found`.

## System Prompt RPC

### `system.prompt`

Returns the repo-level `PI.md` prompt composition template and the rendered
prompt for an existing session's frozen config. `session_id` is required; the
RPC does not preview project prompts before the project workspaces have been
materialized by `session.start`.

```json
{
  "template": "contents of PI.md",
  "rendered": "rendered prompt text"
}
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

`expected_active_leaf_id` is optional. When present, the daemon rejects idle
acceptance with `history_changed` if the active branch moved before the message
was accepted. When the session is already busy, the message is a durable queued
follow-up that will materialize only after earlier work commits from the
then-active branch; queued rows can later be edited/cancelled/reordered before
consumption. The web UI uses this fence for restored composer drafts so a
historical edit cannot silently send into a newer idle context.
`client_input_id` is optional but strongly recommended for frontend sends;
without it, retry idempotency is intentionally not provided.

Idle response:

```json
{ "accepted": true, "queued": false }
```

Busy response:

```json
{
  "input_id": "input_...",
  "accepted": true,
  "queued": true,
  "queue": {
    "session_revision": 12,
    "queue_revision": 5,
    "transcript_revision": 7,
    "activity": "running",
    "queued_inputs": []
  }
}
```

Retries with the same `client_input_id` return `"replayed": true` and the same
canonical queue object when the original ledger row is found.

Queue snapshots in responses and events use the canonical ordering: queued
steers first by steering/promote time, then queued follow-ups by dense
`follow_up_position`.

### `input.promote_queued`

Promotes a still-queued follow-up into the steer queue. Promotions are consumed
in promotion order before remaining follow-ups. If a turn is between completed
tool results and the next model request, the daemon peeks the next queued steer
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
  "promoted": true,
  "queue": {
    "session_revision": 13,
    "queue_revision": 6,
    "transcript_revision": 7,
    "activity": "running",
    "queued_inputs": []
  }
}
```

If the row was already consumed, cancelled, legacy-consuming, or otherwise not a
queued follow-up, the call succeeds with `"promoted": false` and the current row
status. This makes the browser's stale queued-row race non-fatal. A missing
input id still fails with `input_not_found`.

### `input.update_queued`

Edits the content of a still-queued follow-up. Steering messages are not
editable through this RPC because steering order and content are already part of
the high-priority control lane.

```json
{
  "session_id": "s1",
  "input_id": "input_...",
  "expected_queue_revision": 5,
  "content": [
    { "type": "text", "text": "Actually fix the other test" }
  ]
}
```

`expected_queue_revision` is optional but recommended. When supplied, the store
checks it under the per-session row lock before mutating the row. A stale value
returns the canonical current queue with `"updated": false` and
`"reason": "queue_changed"`.

Response:

```json
{
  "input_id": "input_...",
  "updated": true,
  "reason": null,
  "priority": "follow_up",
  "status": "queued",
  "queue": {
    "session_revision": 14,
    "queue_revision": 7,
    "transcript_revision": 7,
    "activity": "running",
    "queued_inputs": []
  }
}
```

If the row is a steer, consumed, consuming, or cancelled, the RPC returns
`"updated": false`, `"reason": "not_editable"`, the row's current priority and
status, and the canonical queue. Updating to identical content is a no-op: it
returns `"updated": false`, `"reason": null`, and does not bump
`queue_revision` or emit `input.updated`.

### `input.cancel_queued`

Deletes a still-queued follow-up from the visible queue by marking it
`cancelled`. Cancelled rows remain in the ledger for idempotency/audit but are
not returned by `session.get` queue projections.

```json
{
  "session_id": "s1",
  "input_id": "input_...",
  "expected_queue_revision": 5
}
```

Response:

```json
{
  "input_id": "input_...",
  "cancelled": true,
  "reason": null,
  "priority": "follow_up",
  "status": "cancelled",
  "queue": {
    "session_revision": 15,
    "queue_revision": 8,
    "transcript_revision": 7,
    "activity": "running",
    "queued_inputs": []
  }
}
```

Stale revisions return `"reason": "queue_changed"` with the canonical queue.
Steers and non-queued rows return `"reason": "not_editable"`.

### `input.reorder_queued_follow_ups`

Reorders queued follow-ups only. Steering rows are omitted from `input_ids`,
remain at the top, cannot be reordered, and keep steering/promote order. The
client sends the full desired follow-up id order; the backend rewrites dense
`follow_up_position = 0..n-1` values.

```json
{
  "session_id": "s1",
  "expected_queue_revision": 5,
  "input_ids": ["input_c", "input_a", "input_b"]
}
```

Response:

```json
{
  "reordered": true,
  "reason": null,
  "input_ids": ["input_c", "input_a", "input_b"],
  "queue": {
    "session_revision": 16,
    "queue_revision": 9,
    "transcript_revision": 7,
    "activity": "running",
    "queued_inputs": []
  }
}
```

The provided id set must exactly equal the current queued follow-up id set.
Mismatch or stale `expected_queue_revision` returns `"reordered": false`,
`"reason": "queue_changed"`, the current follow-up order in `input_ids`, and
the canonical queue. Submitting the already-current order is a no-op and does
not bump `queue_revision` or emit `input.reordered`.

### `input.interrupt`

Interrupts current work. The daemon marks unfinished action rows for the session
`interrupted`, aborts registered model/tool/compaction task handles on a
best-effort basis, emits `session.work_cancelled`, and resumes normal queue
driving. If the session is idle, the daemon emits `input.ignored` and returns
`{ "ignored": true }`.

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

### `history.switch`

Idle-only. Moves the active leaf to a committed turn boundary or to root.
This is the one source-mutating history operation: frontends use the same RPC
both for "switch to the boundary before editing this user message" and for
"switch the active view to this completed branch or compaction root." The RPC
never creates a session and never deletes abandoned branches.

```json
{
  "session_id": "s1",
  "leaf_id": "entry_4",
  "expected_active_leaf_id": "entry_9",
  "return_active_branch": true
}
```

Root switch:

```json
{ "session_id": "s1", "leaf_id": null }
```

Running sessions fail with `session_busy`; non-boundaries fail with
`not_turn_boundary`. If `expected_active_leaf_id` is supplied and the session
has moved since the picker was opened, switch fails with `history_changed`.
When `return_active_branch` is true, the response includes the new
`session_revision`, `queue_revision`, `transcript_revision`, `last_event_id`,
and `active_branch_entries` so the frontend can render the switched branch
without a follow-up `session.get` in the hot path.

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

## Delegation RPC

Delegations are the frontend-facing unit for bounded parent/child subagent work.
A delegation is either one full (writing) subagent or a read-only fan-out. The
websocket API only accepts the canonical `delegation.*` methods below.

### `delegation.start_full`

Starts one full subagent that writes in the parent workspace.

```json
{
  "parent_session_id": "parent-session",
  "role": "implementer",
  "prompt": "Implement the requested change.",
  "workflow": "workflow-implement-review",
  "label": "implement change"
}
```

Result:

```json
{
  "delegation_id": "delegation_...",
  "subagent_session_id": "session_..."
}
```

### `delegation.start_readonly_fanout`

Starts one read-only subagent per task, each in a disposable snapshot.

```json
{
  "parent_session_id": "parent-session",
  "tasks": [
    { "role": "reviewer", "prompt": "Review the backend changes." },
    { "role": "tester", "prompt": "Run focused validation." }
  ],
  "workflow": "workflow-implement-review-test",
  "label": "review and test"
}
```

Result:

```json
{
  "delegation_id": "delegation_...",
  "subagent_session_ids": ["session_...", "session_..."]
}
```

### `delegation.status`

Returns one in-scope delegation as the canonical structured snapshot. The
snapshot includes delegation metadata, progress counts, subagent roles/types,
activity/status, steerability, terminal final-message text/suggested_next (when
available), and handoff artifact paths. It does not inline full transcript
contents; read the `transcript.md` file when detail is needed.

```json
{
  "parent_session_id": "parent-session",
  "delegation_id": "delegation_..."
}
```

Result:

```json
{
  "delegation_id": "delegation_...",
  "kind": "readonly_fanout",
  "status": "running",
  "workflow": "implement_review",
  "label": "review",
  "progress": { "expected": 2, "spawned": 2, "terminal": 1, "running": 1, "failed": 0 },
  "subagents": [
    {
      "id": "session_...",
      "role": "reviewer",
      "type": "read_only",
      "subagent_type": "read_only",
      "activity": "idle",
      "status": "done",
      "steerable": false,
      "final_message": "Looks good.\n\nsuggested_next: approved",
      "suggested_next": "approved",
      "final_message_path": "/.../.pi-handoff/delegation_.../session_.../final_message.md",
      "transcript_path": "/.../.pi-handoff/delegation_.../session_.../transcript.md"
    }
  ],
  "handoff_dir": "/.../.pi-handoff/delegation_..."
}
```

### `delegation.cancel`

Interrupts all running subagents in a delegation and marks the delegation
cancelled. Terminal delegations are left unchanged and return
`{ "cancelled": false }`.

```json
{
  "parent_session_id": "parent-session",
  "delegation_id": "delegation_..."
}
```

### `delegation.list`

Lists all delegations for a parent session, newest first. This is the run-board
feed used by the web UI.

```json
{ "parent_session_id": "parent-session" }
```

Result:

```json
{
  "parent_session_id": "parent-session",
  "delegations": [
    {
      "delegation_id": "delegation_...",
      "kind": "full",
      "status": "done",
      "workflow": "workflow-implement-review",
      "label": "implement change",
      "subagents": [
        {
          "id": "session_...",
          "status": "idle",
          "role": "implementer",
          "subagent_type": "full",
          "task": "Implement the requested change."
        }
      ],
      "handoff_dir": "/.../.pi-handoff/delegation_..."
    }
  ]
}
```

### `delegation.read_handoff_file`

Reads `final_message.md` or `transcript.md` from a delegation subagent
directory. The structured delegation snapshot comes from
`delegation.status`/`inspect_delegation`, not from a handoff `index.json`.

```json
{
  "parent_session_id": "parent-session",
  "delegation_id": "delegation_...",
  "subagent_id": "session_...",
  "file": "final_message.md"
}
```

Result:

```json
{
  "delegation_id": "delegation_...",
  "subagent_id": "session_...",
  "file": "final_message.md",
  "content": "..."
}
```

## Subagent events

When a delegation subagent is spawned or re-driven, the daemon may emit
parent-scoped `subagent.spawned` and `subagent.running` progress events. These
are progress hints only. Parent-visible delegation completion is not a per-child
`subagent.idle`; it is one `InputPriority::Steer` queued to the parent after the
delegation barrier completes, pointing at the handoff directory. Use
`inspect_delegation`/`delegation.status` for structured state and the
per-subagent `final_message.md`/`transcript.md` files for details.

`subagent.idle` remains an event type for non-delegation subagent compatibility
(for example, defensive dispatch-failure compensation). When emitted, idle
notifications are de-duplicated per completed terminal child state, not for the
child session lifetime.

## Tools

### `tools.list`

Requires a `provider` parameter (`"openai"` or `"anthropic"`/`"claude"`) and
returns the model-visible tool definitions for that provider, because the tool
surface is provider-shaped (e.g. OpenAI `apply_patch` vs Anthropic
`text_editor_20250728` for editing). The registered builtins are `edit`, `bash`,
`grep`, `web_search`, `web_fetch`, `LoadSkill`, and the delegation tools
(`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`,
`cancel_delegation`, `steer_subagent`) - there are no `read`/`write` tools. Each returned entry
carries `name`, `description`, `input_schema`, `canonical_name`, `prompt_alias`,
`execution`, and `kind: "local_tool"`.

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
queued follow-ups can advance. A user stop/`input.interrupt` during compaction
aborts the registered compaction task, marks the action `interrupted`, and then
lets queued inputs continue from the original active leaf.

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
input.updated
input.cancelled
input.reordered
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
history.switched
history.compacted
session.work_cancelled
session.recovered
session.idle
subagent.spawned
subagent.running
subagent.idle
```

`subagent.idle` is listed for compatibility; delegation-member completion is
reported by the delegation steer/handoff described above.

No approval or awaiting-approval events are emitted.

Queue-visible events produced by the redesigned paths (`input.accepted`,
`input.queued`, `input.consumed`, `input.promoted`, `input.updated`,
`input.cancelled`, and `input.reordered`) include the canonical post-transition
queue projection:

```json
{
  "session_revision": 13,
  "queue_revision": 6,
  "transcript_revision": 7,
  "activity": "queued",
  "queued_inputs": []
}
```

Clients should replace cached queue state when an event carries a newer
`queue_revision`. They should refetch rather than trying to infer ordering from
partial event payloads.

`transcript.appended` carries the appended entry body when available, plus its
compact `tree_node`, `active_leaf_id`, and revision counters:

```json
{
  "entry_id": "entry_10",
  "entry": { "id": "entry_10", "sequence": 10, "parent_id": "entry_9" },
  "tree_node": { "id": "entry_10", "sequence": 10, "item_type": "assistant_message" },
  "active_leaf_id": "entry_10",
  "session_revision": 14,
  "queue_revision": 6,
  "transcript_revision": 8
}
```

`history.switched` similarly carries `active_leaf_id`, `activity`, and the
revision counters. Events are freshness hints, not a generic patch protocol; if
a client cannot apply an event safely, it should fetch the canonical projection
it needs.

## Manual Websocket Exercise Plan

These checks should be run by sending websocket RPC frames exactly like a
frontend. Harness methods are acceptable for model and compaction timing; tool
behavior should use real builtin tools.

Useful SQL after each scenario:

```sql
select active_leaf_id, provider_config, metadata,
       session_revision, queue_revision, transcript_revision
  from sessions where id = '<SESSION>';
select key, value from daemon_config order by key;
select sequence, id, parent_id, item
  from transcript_entries where session_id = '<SESSION>' order by sequence;
select id, kind, status, attempt_id, payload, result
  from actions where session_id = '<SESSION>' order by created_at;
select id, type, payload
  from events where session_id = '<SESSION>' order by id;
select id, priority, status, client_input_id, content, created_at, updated_at,
       follow_up_position, origin
  from queued_inputs where session_id = '<SESSION>'
  order by case priority when 'steer' then 0 else 1 end,
           follow_up_position nulls last, created_at, id;
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

### 2. PI.md Prompt Preview

1. Call `system.prompt` with a real `session_id`.
2. Verify the response contains the repo-level `PI.md` template and a rendered
   prompt for the requested session.
3. In the web UI, type `/system` and verify the same prompt preview is displayed
   read-only.


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
new queued-input consumption keeps rows `queued` until the same transaction that
appends transcript state marks them `consumed`. When the eligible boundary is a
tool-to-model continuation, verify the promoted steer appears after the tool
results and before the next model action without a `turn_finished` between
them. `input.promote_queued` must promote before consumption and return
`"promoted": false` with the current row status once the row is no longer a
queued follow-up.

Also verify an idle `input.follow_up` retried with the same `client_input_id`
returns `"replayed": true`, leaves exactly one user-message transcript entry,
and records a single consumed input ledger row.

### 4a. Queued Follow-Up Edit, Cancel, And Reorder

1. Start a turn and leave its model action running.
2. Queue three follow-ups.
3. Call `input.reorder_queued_follow_ups` with the full follow-up id list in a
   new order.
4. Call `input.update_queued` on one follow-up with the returned
   `queue_revision`.
5. Call `input.cancel_queued` on another follow-up.
6. Queue or promote a steer and then try to update/cancel/reorder it through
   the follow-up mutation RPCs.
7. Repeat one mutation with a stale `expected_queue_revision`.

Verify the queue pane order matches the canonical `session.get` order after
every event, remaining follow-ups have dense `follow_up_position` values,
steers stay at the top and are not editable/reorderable, stale revisions return
`reason: "queue_changed"` with the current canonical queue, and no mutation is
lost across daemon reconnect/restart.

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

### 5b. Interrupt Running Compaction

1. Request compaction on an idle non-empty session.
2. Before the provider returns, send `input.interrupt`.
3. Optionally queue a follow-up before or immediately after the interrupt.

Verify the compaction action is marked `interrupted`, `session.work_cancelled`
is emitted, no compaction summary root is appended by a late provider result,
and queued follow-ups continue from the pre-compaction active leaf.

### 6. History Switch Lifecycle

1. Create two completed turns and record boundary leaf ids.
2. Start a third pending turn.
3. Attempt `history.switch` to the first boundary.
4. Interrupt, wait for idle, then switch again.
5. Attempt switch to a user-message entry.

Verify running switch fails with `session_busy`, post-interrupt switch succeeds,
descendant rows are preserved, and non-boundary switch fails with
`not_turn_boundary`. Also verify stale picker requests with a mismatched
`expected_active_leaf_id` fail with `history_changed`.

### 7. Real Tools

Use `harness.model.complete` to request real tools:

- `edit` success and missing-target error (`apply_patch` for OpenAI,
  `text_editor_20250728` for Anthropic).
- `bash` success, non-zero exit, malformed args, and timeout.
- `grep` success and no-match.
- Multiple tool calls in one assistant response.

Verify there is no approval event, tools emit `tool.requested`/`tool.started`,
tool-returned errors append error `ToolResult`s, action rows for tool failures
are `error`, and the next model request sees tool results in the assistant's
declared order.

### 8. Compaction Validity

1. Request compaction on an idle session.
2. Observe `compaction.requested`, then let the daemon/provider complete it.
3. Verify the new active leaf is a `compaction_summary` root with
   `parent_id = null`, `source_session_id`, `source_leaf_id`, and
   `last_turn_id`.
4. Queue a normal follow-up while compaction is running and verify it is not
   consumed until `compaction.completed`, `compaction.error`, or user interrupt
   of the compaction.
5. Request compaction while a model action is running.

Verify compaction emits `compaction.completed` and `history.compacted`, the
old source branch remains in `history.tree`, and running compaction requests
fail with `session_busy`.

### 9. Daemon Death Recovery

1. Start a harness turn and leave the model action running.
2. Kill the daemon process.
3. Restart the daemon.
4. `events.subscribe` and `session.get` for the same session.

Verify unfinished actions are stale, recovery appends a crashed turn tail,
`turn.finished` and `session.recovered` are replayable, and no explicit resume
RPC is needed.

### 10. Browser User Flows

Run the TypeScript web UI against the same daemon and Postgres database.

Verify:

- Markdown and raw HTML assistant text render in the transcript.
- Slash autocomplete uses Enter once to complete and the next Enter to execute.
- `/switch` opens the picker; there is no raw-id path in the UI.
- `/switch` is idle-only. Switching to a user-message target restores that
  historical text into the composer; switching to a completed turn or
  compaction root changes the active leaf inside the same session.
- `/new`, `/retry`, `/continue`, `/rename`, `/archive`, `/unarchive`,
  `/tree` are not part of the user-facing slash surface.
- Crashed and interrupted terminal model turns show Retry/Continue actions that
  invoke the `turn.resume` RPC.
- A brand-new local draft survives browser refresh without creating a durable
  empty session.

### 11. Real Codex Provider

With `~/.codex/auth.json` or `CODEX_ACCESS_TOKEN` available:

```json
{
  "method": "session.start",
  "params": {
    "session_id": "manual_real_codex",
    "provider": {
      "kind": "openai",
      "model": "gpt-5.5",
      "prompt_cache": { "key": "pi-relay-real-smoke" }
    },
    "metadata": { "manual": "real-codex" }
  }
}
```

Inspect the prompt template if needed:

```json
{
  "method": "system.prompt",
  "params": { "session_id": "s1" }
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
`provider.kind = "anthropic"` or `"claude"` and ask the model to use the `edit`
tool (`text_editor_20250728`) or `bash`. Verify provider-requested tool calls,
real tool results, a second model request containing those results, and a final
assistant message.

## Documentation Sync Rule

Whenever a crate boundary, RPC method, lifecycle rule, provider credential
path, storage invariant, or manual exercise changes, update this file,
`rust/docs/architecture.md`, `rust/README.md`, crate READMEs as needed, and
`rust/WORKLOG.md` in the same change.
