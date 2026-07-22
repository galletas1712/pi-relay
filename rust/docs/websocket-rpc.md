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

5. History writes and snapshots are idle-only.
   `history.switch`, `history.fork`, `session.configure`, and
   `compaction.request` fail with
   `session_busy` while work is active or queued. Here idle means there are no
   unfinished actions and no queued inputs waiting to become transcript. The two
   history operations additionally require every delegation for the source
   session to be terminal. A frontend should interrupt/cancel the relevant
   work, wait for idle, then retry.

6. Fork targets are switch targets.
   `history.fork` accepts the same committed turn boundaries (including
   compaction roots) and root target as `history.switch`, with the same
   active-leaf, transcript-revision, and target-branch fences. It does not
   accept arbitrary mid-turn entries.

7. Tools are always allowed.
   The daemon runs model-requested tools immediately. There is no approval or
   denial RPC. `input.interrupt` is the one user-facing cancellation command and
   interrupts active work. The daemon keeps a per-action task registry and
   aborts registered model, tool, and compaction futures for the interrupted
   session on a best-effort basis; durable action status remains the source of
   truth for stale completions.

8. Daemon death is recoverable state.
   Startup first reconciles durable selected-subagent controls so an
   already-committed exact-child interrupt settles its captured action
   generation. It then validates and recovers pending/running model actions
   carrying transactionally installed post-compaction dispatch intents before
   stale-marking other leftover unfinished actions whose provider/tool futures
   cannot resume. Recovery validates the action/attempt/leaf fences and
   reclaims pending or expired ownership leases; an unexpired lease is left
   alone. The stale sweep protects either durable class if reconciliation
   remains retryable. A process-lifetime watchdog retries post-compaction work
   at the database-derived expiry, wakes when a heartbeat/runner loses
   ownership, and backs off across transient database/recovery errors. Terminal
   completion clears the intent atomically. Dispatch is at least once because a
   crash after provider acceptance but before the terminal commit can lead to a
   duplicate call, and current provider requests have no idempotency key. If
   the daemon died with another open turn, first touch
   repairs the session by appending a crashed turn tail. External side effects,
   such as files written by tools, are not transactional.

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
lock, and provider/tool work never runs while the lock is held. A fresh database
gets the complete current schema, while an existing current database receives
the idempotent current schema statements again at startup. The daemon does not
run historical old-session/data migrations automatically; older deployments
must follow their release-specific migration instructions first.

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
  "model": "gpt-5.6-sol",
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
- `claude`: Anthropic Messages API through `ANTHROPIC_API_KEY`.

`anthropic` and `codex` are **not** provider kinds. `codex` is the auth
transport used by the `openai` kind. Requests using either retired name are
rejected at decode time.

`prompt_cache.key` maps to `ModelRequest::prompt_cache_key` and is sent on the
OpenAI request path. `max_tokens` is optional; when present it is emitted as
OpenAI `max_output_tokens`, and when omitted the daemon does not set an OpenAI
output cap.

`reasoning_effort` defaults to `medium`. The shared wire vocabulary is `none`,
`minimal`, `low`, `medium`, `high`, `xhigh`, and `max`; decoding any other
string fails. For OpenAI, that vocabulary is not a promise that every model
accepts every value: before ordinary and compact requests, the private Codex
catalog must contain the exact selected slug and advertise the exact configured
effort. Unsupported values fail locally without normalization.

The account-scoped GPT-5.6 catalog advertises `ultra` for Sol/Terra but not
Luna. That is Codex harness metadata, not an additional pi-relay wire effort:
pinned Codex converts Ultra to Max before Responses requests and uses Ultra to
select proactive behavior only with MultiAgent V2. Live literal
`reasoning.effort = "ultra"` requests to Sol and Terra returned HTTP 400, while
ordinary exposed efforts succeeded. pi-relay does not implement that proactive
orchestration mode, so it neither exposes `ultra` nor silently aliases it to
`max`. Catalog-only and future unknown levels remain tolerated as metadata and
cannot enter a request body. The same catalog advertises no `none` for the
reviewed GPT-5.6 models.

Claude Sonnet 5, Fable 5, and Opus 4.8 accept `low`, `medium`, `high`, `xhigh`,
and `max`; their provider-specific shaping remains inside the Anthropic
adapter. Fable 5 requires 30-day retention and is not available under Zero Data
Retention, so it is an explicit opt-in UI choice.

Compaction defaults are provider/model aware through provider metadata. OpenAI
uses an authenticated account-scoped catalog and recommends at most 90% of the
resolved current/default context window: the 372,000-token GPT-5.6 fixture
produces 334,800, while GPT-5.4 uses 244,800 from its 272,000 current window
rather than 900,000 from its 1,000,000 maximum. There is no static OpenAI
fallback; if authoritative metadata is unavailable, proactive scheduling has no
derived threshold and reactive overflow handling remains enabled. Verified
1,000,000-token Claude models recommend 500,000. Explicit valid session
metadata overrides provider recommendations and is clamped safely.

### `daemon_config`

Reserved daemon key-value configuration table.

```text
key text primary key
value jsonb not null
updated_at timestamptz not null default now()
```

The `PI.md` is the prompt composition template. It is not stored per session.
Normal provider requests use the rendered prompt as the stable prefix followed
by transcript history; the daemon does not inject a top-level
`## Current delegations` dashboard into ordinary turns. Parent-session
compaction inputs also exclude live delegation dashboards. After the provider
returns a compacted summary, the daemon appends a fresh bounded
`## Delegation state at compaction time` ledger to the stored summary so every
delegation row/status crosses the compaction boundary without inlining
transcript bodies. Subagent compactions exclude parent/sibling delegation state.

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

Durable input queue. Most rows are user follow-ups; daemon-authored wakeup
observations can also be queued at steer priority so parent sessions resume
promptly without treating the notification as a user message.

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

Migration creates `sessions.delegation_id` before inspecting child links, then
idempotently repairs legacy/default-one delegation rows to their actual linked
child count when that count is greater than one. Intentional values greater
than one are preserved and the positive-count constraint is enforced before
the completion fence is used.

Child sessions link back through `sessions.delegation_id`. The
`delegations_parent_created_idx` index supports the per-parent delegation feed.
The completion runner uses `attempt_id` as an idempotency fence and queues a
deterministic parent daemon wakeup observation keyed as
`delegation-steer:<delegation_id>:<attempt_id>` (the key name is retained for
idempotency compatibility).

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
    "model": "gpt-5.6-sol",
    "prompt_cache": { "key": "pi-relay-local" }
  },
  "metadata": { "title": "New session", "created_by": "web" },
  "client_input_id": "web_start_draft_1",
  "priority": "follow_up",
  "content": [
    { "type": "text", "text": "Hello" }
  ],
  "mcp": {
    "inventory_revision": "sha256...",
    "servers": [
      { "server": "workspace", "tools": ["read_file", "search"] }
    ]
  }
}
```

`provider` is optional for a new session. When omitted, the daemon selects its
configured `default_parent_model` (or the static OpenAI `gpt-5.6-sol`/`high`
fallback when that optional policy is omitted) and persists that selected
provider in the new session. An explicit value always wins. A replay with the same
`session_id` returns the persisted session before applying defaults, so
configuration changes do not retarget existing sessions, queued work, or
existing action routes.

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
actions, the optional content-addressed MCP-only manifest reference, and events
in the same session-start transition before dispatching provider/tool work.
Omitting `mcp` (or sending an empty `servers` list) explicitly creates an
MCP-free session. A nonempty selection contains sorted raw server/tool
identities only; it never contains schemas, server configuration, commands,
environment values, or credentials. The daemon validates the semantic
`inventory_revision`, the complete selected server catalogs, and all raw names
before inserting the session. It returns `mcp_inventory_changed` for a stale
revision, `mcp_selection_invalid` for unknown/duplicate/disallowed identities,
and `mcp_unavailable` when a selected server cannot be validated. The client
must refresh/reconcile and must not silently select newly published tools.

The selected MCP manifest is frozen for the whole durable session. Later
configuration refreshes, reconnects, and `tools/list_changed` notifications
affect only New Session inventory. A retry with the same stable `session_id`
returns the existing session before consulting current inventory and cannot
replace its binding.

For project sessions the daemon snapshots the project's current
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

Recovers the session if needed, then returns a durable snapshot. A missing
session returns the stable `session_not_found` error code.

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
  "provider": { "kind": "openai", "model": "gpt-5.6-sol" },
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

`queued_inputs` contains live queued or consuming input rows with `input_id`,
`priority`, `status`, `content`, `client_input_id`, `created_at`, `updated_at`,
optional `promoted_at`, and optional `follow_up_position`. User-message rows
carry their message content. Daemon wakeup observation rows are non-editable
signals (`content_type = "daemon_tool_observation"`, `content = []`,
`editable = false`); their typed transcript entry/result JSON and handoff files
are the source of truth, not queue-event prose. The web UI uses editable
user-message rows for the composer-adjacent queue pane. `session_revision`,
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
`provider_replay`. This supports clients that need the transcript forest;
the `/switch` and `/fork` pickers use `history.targets`.

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
boundary. Picker clients should use `history.targets`, whose boundary mapping is
daemon-computed and validated again in the mutation transaction.

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

### `history.targets`

Returns a deterministic newest-first page of editable historical user-message
targets for the `/switch` and `/fork` pickers without loading the full
transcript tree.

```json
{ "session_id": "s1", "before_sequence": null, "limit": 50 }
```

Each target contains `entry_id`, daemon-resolved `target_leaf_id`,
`timestamp_ms`, the containing `turn_id`, `is_on_active_branch`, and a preview
capped at 160 characters.
`target_leaf_id` is the safe boundary immediately preceding the message, or
`null` for root. `next_before_sequence` is the exclusive cursor for the next
older page. The projection omits ancestry paths, assistant/tool/boundary rows,
and full message bodies; fetch the exact selected message separately with
`transcript.entries`.

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
knobs such as `reasoning_effort` during or between turns. An effort-only update
persists immediately as the session default for future accepted work; it does
not replace an active runtime's route. Each queued input captures the provider
config when accepted, and each action captures its open-turn route. Existing
queued items, provider retries, tool continuations, compaction/recovery, and
steering consumed into an open turn therefore retain their captured config;
newly accepted future work captures the new default. These route snapshots are
daemon-internal and do not add fields to queue RPC views. Responses and
`session.configured` events include `provider`, `metadata`, and `activity` so
clients can patch cached summaries and selected snapshots.

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
input), and re-checks that state under the store transaction that deletes the
row. If a follow-up was accepted concurrently, deletion fails with
`session_busy` rather than cascade-deleting accepted input. Otherwise the RPC
evicts any live session, removes the row, and cascades to its transcript
entries, queued inputs, actions, and events; session workspace directories are
cleaned up. Returns `{ "session_id", "deleted": true,
"deleted_child_session_ids": [...] }`; the child IDs cover the recursively
deleted hidden subagent tree. A missing session is `session_not_found`.

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
{
  "replayed": [],
  "has_more": false,
  "next_after_event_id": null
}
```

After the response, matching live events stream as event frames.

Replay responses are bounded to a server-side page budget (currently 500
events). If more rows remain, the response sets `has_more: true` and includes
`next_after_event_id`, the id of the last returned event. The caller must issue
another `events.subscribe` with that id to continue; a page with
`has_more: true` is not a complete replay and must not be treated as a resync
success. The web client follows these continuation pages automatically while
retaining the same `EventFrame[]` API.

If `after_event_id` is `null` or omitted, the daemon subscribes from the current
head and returns an empty replay with `has_more: false` and a null continuation.
Use a concrete id only for reconnecting after a known high-water mark.

### `events.unsubscribe`

Stops streaming live events for a session on the current websocket.

## Input RPC

### `input.follow_up`

Normal user message. The daemon first records a durable queued row and returns
the canonical queue projection. If the session has no unfinished actions, it
then starts a background drive to consume the row; if work is already running,
the row remains queued until earlier work finishes.

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

`expected_active_leaf_id` is optional. When present, the daemon validates it in
the same store transaction that queues the row and rejects stale sends with
`history_changed`. The row will materialize only after background driving
consumes it from the then-active branch; queued rows can later be
edited/cancelled/reordered before consumption. Idle-only source mutations such
as `history.switch` and `session.delete` re-check for queued input in their own
store transactions, so they fail with `session_busy` instead of redirecting or
deleting already accepted input. The web UI uses this fence for restored
composer drafts so a historical edit cannot silently send into a newer idle
context.
`client_input_id` is optional but strongly recommended for frontend sends;
without it, retry idempotency is intentionally not provided.

Response:

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

For backward compatibility, raw ordinary-priority
`input.follow_up`/`session.input` can target a subagent. It is an ordinary child
follow-up and does not provide parent/delegation control validation. Raw
`priority = "steer"` input to a child is rejected with
`subagent_steer_requires_parent_scope`; callers must use parent-scoped
`delegation.steer_subagent`/`steer_subagent` for a validated child steer.
Internally accepted scoped steers are stored as `InputPriority::Steer` rows on
the child. Promoting a child-session follow-up through
`input.promote_queued` is rejected for the same reason.

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

Interrupts current work for exactly the supplied `session_id`; it never walks
parent, child, sibling, or delegation relationships. The daemon marks
unfinished action rows for that session
`interrupted`, aborts registered model/tool/compaction task handles on a
best-effort basis, emits `session.work_cancelled`, and resumes normal queue
driving. If the session is idle, the daemon emits `input.ignored` and returns
`{ "ignored": true }`.

Consequently, the web Stop button on a selected root interrupts only that root,
and Stop on a selected delegation child interrupts only that child. Use
`delegation.cancel`/`cancel_delegation` to cancel a whole delegation.

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

`history.switch` and `history.fork` share these target fields:

- `leaf_id` is required and must be a committed turn boundary id or explicit
  `null` for root.
- Optional `expected_active_leaf_id` is a string or `null` and fences the
  source's active leaf.
- Optional integer `expected_transcript_revision` fences all transcript
  changes.
- Optional `source_entry_id` identifies a user-message target returned by
  `history.targets`. When present, the store resolves that message's preceding
  boundary again in the mutation transaction and requires it to equal
  `leaf_id`; clients must not compute or substitute ancestry.
- Optional `active_branch_entry_ids` is the exact ordered, compaction-aware
  path to `leaf_id`. Omission disables this fence; an explicit empty array
  asserts an empty root path and is not equivalent to omission.

Malformed, mistyped, or unknown request fields fail with `invalid_params`.

```json
{
  "session_id": "s1",
  "leaf_id": "entry_4",
  "expected_active_leaf_id": "entry_9",
  "expected_transcript_revision": 12,
  "active_branch_entry_ids": ["entry_1", "entry_4"],
  "return_active_branch": true
}
```

Root switch:

```json
{ "session_id": "s1", "leaf_id": null }
```

Running sessions and sessions with any running delegation fail with
`session_busy`; non-boundaries fail with `not_turn_boundary`. If
any supplied fence is stale, switch fails with `history_changed`.
When `return_active_branch` is true, the response includes the new
`session_revision`, `queue_revision`, `transcript_revision`, `last_event_id`,
and `active_branch_entries` so the frontend can render the switched branch
without a follow-up `session.get` in the hot path.

### `history.fork`

Idle-only and available only to managed project sessions. Creates a visible,
independent top-level session without changing the source. Target semantics
and fences are the shared contract above. A user-message edit in the picker
sends the same previous-boundary target used by switch.

```json
{
  "session_id": "s1",
  "leaf_id": "entry_4",
  "expected_active_leaf_id": "entry_9",
  "expected_transcript_revision": 12,
  "active_branch_entry_ids": ["entry_1", "entry_4"]
}
```

The child receives the source's complete committed transcript forest in source
insertion order, preserving entry ids, parent links, typed items, turn ids,
compaction roots/sibling branches, and opaque `provider_replay`. Its active
leaf is exactly `leaf_id`; the fork does not synthesize transcript entries. Queued
inputs, actions, reconnect events, delegations, and subagent relationships are
not copied. Fork provenance is stored in
`metadata.fork = { source_session_id, source_leaf_id }`; the child has no
`parent_session_id` and uses the parent prompt/tool profile. The child inherits
the source title, auto-title preference, and compaction policy, but starts
without source archival/subagent state or compaction `auto_state`.

The filesystem is independent from the source: the daemon snapshots the
source session's **current idle managed cwd**, including current dirty and
untracked files, into a new session cwd and gives copied Git workspaces child
branches. Files are not reconstructed as of `leaf_id`, and no per-turn
filesystem checkpoints are created. Ephemeral/unmanaged/shared session cwds
are rejected rather than copying `$HOME` or another session's workspace. The
daemon also rejects a managed session or cwd root that is a symlink or is not a
directory immediately before cloning; symlinks inside a valid cwd are preserved.

The response includes `session_id`, `source_session_id`, nullable
`source_leaf_id`, `active_leaf_id`, revisions, and `last_event_id`. Busy
sources fail with `session_busy`, stale fences with `history_changed`,
non-boundaries with `not_turn_boundary`, unmanaged sources with
`project_required` or `workspace_unmanaged`, and generated child workspace
collisions are never overwritten. A running delegation also reports
`session_busy`, because a full child can write the parent's cwd while the
parent itself appears idle.

The server generates each child id. A successful fork is not retry-idempotent:
if delivery of the response is uncertain, retrying may create another child.
Clients should use the workspace-operation timeout and refresh the project
session list before deciding to retry.

Workspace snapshot serialization covers daemon-managed local and MCP tool
futures that share the exact managed cwd. `history.fork` and read-only
delegation snapshots wait on that guard; the guard is released when a tool
future returns or is dropped/aborted. `.pi-handoff` is excluded from clones.
The daemon does not claim to track independently running background processes
beyond the managed future lifetime.

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

There is no dedicated delegation rerun/restart RPC. The `delegation.start_full`
and `delegation.start_readonly_fanout` methods launch explicitly supplied new
work; they do not restart a prior delegation. Task-prompt handoff metadata is
retained for Handoffs/inspection only. This is separate from terminal-turn
`turn.resume`.

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
activity/status, steerability, `outcome` (when available), and compact
handoff file references. It does not inline full transcript, task prompt, or
final-message bodies; read handoff files when detail is needed. `steerable` is
true only when a parent-scoped `steer_subagent` request would currently be
accepted for that child: the delegation is running, the child is a delegation
member, the child has queued/unfinished/runtime work, and it is not
completion-terminal.

Daemon wakeup observations also carry this same snapshot. A terminal snapshot is
the normal completion/cancellation handoff. A still-`running` snapshot is a
partial fan-out decision point: at most one queued/consuming partial wakeup is
active per delegation attempt, and it appears only after the expected fan-out
members exist. The parent should steer a running/steerable child, cancel the
delegation, or wait; final completion cancels stale queued partial wakeups before
publishing the terminal wakeup.

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
      "outcome": "approved",
      "final_message_file": "session_.../final_message.md",
      "transcript_file": "session_.../transcript.md",
      "task_prompt_file": "session_.../task_prompt.md"
    }
  ],
  "handoff_dir": "/.../.pi-handoff/delegation_..."
}
```

Roles supplied to `delegation.start_full` and
`delegation.start_readonly_fanout` must be either agentd-configured global
subagent role skills or exact skill names from the available skills JSON.
Workspace-scoped skills must use their prefixed names, for example
`repo/reviewer`; unprefixed names resolve only configured global role skills.
Workflow skills such as `workflow-explore` are loadable global
`LoadSkill` skills and may label delegation orchestration, but they do not
become subagent roles unless separately present under `subagent-roles` or a
workspace skill package.

### `delegation.cancel`

Interrupts all running subagents in a delegation and marks the delegation
cancelled. Terminal delegations are left unchanged and return
`{ "cancelled": false }`. A successful cancellation returns compact
per-subagent transcript file references relative to `handoff_dir`.

```json
{
  "parent_session_id": "parent-session",
  "delegation_id": "delegation_..."
}
```

Successful result:

```json
{
  "cancelled": true,
  "delegation_id": "delegation_...",
  "handoff_dir": "/.../.pi-handoff/delegation_...",
  "subagents": [
    {
      "subagent_id": "session_...",
      "transcript_file": "cancelled/session_abc.transcript.md"
    }
  ]
}
```

### `delegation.steer_subagent`

Parent-scoped steering for one running delegation subagent. The daemon validates
that `parent_session_id` owns the child through a running delegation, that the
child is a full or read-only delegation subagent, and that the child is active
rather than completion-terminal. Omitted `interrupt` defaults to `false`, which
preserves the original noninterrupting steer behavior. `client_control_id` is
optional but recommended for retries.

```json
{
  "parent_session_id": "parent-session",
  "subagent_id": "session_...",
  "message": "Please also check the retry path.",
  "interrupt": false,
  "client_control_id": "web_control_..."
}
```

Result:

```json
{
  "subagent_id": "session_...",
  "accepted": true,
  "queued": true,
  "input_id": "input_...",
  "replayed": false,
  "phase": "ready",
  "interrupted": false,
  "interrupt_outcome": null,
  "drive_status": "started",
  "drive_error": null
}
```

With `interrupt: true`, this is one combined daemon control, not two public RPC
calls. Its durable phases are:

1. `pending_interrupt`: the committed row blocks generic consumption of every
   row in the child mailbox.
2. `interrupt_applied`: for an open turn, one transaction appends the
   interrupted boundary, settles every still-unfinished action attempt captured
   for the active turn/generation, and advances the control phase. If the
   captured leaf is already a durable boundary, reconciliation leaves that leaf
   unchanged: it atomically interrupts the captured boundary-hosted action
   generation, or records `already_between_turns` with `interrupted: false` when
   there was no action to interrupt.
3. `ready`: after exact-child runtime tasks are aborted, the steer is eligible
   for ordinary mailbox consumption.
4. `cancelled`: whole-delegation cancellation settled the control without
   consuming its steer.

An exact-child worker reconciles those phases independently of the requesting
parent task. Fresh acceptance installs a detached owner before any subsequent
await; replay nudges and the periodic live sweep use nonblocking driver
acquisition, and the sweep also discovers ready scoped steers with no unfinished
action owner. Boot recovery uses the same `SessionDriver` path. A crash in
`interrupt_applied` resumes task settlement rather than applying another
interrupt. The control records the active leaf, turn, and deterministic complete
set of unfinished action-attempt IDs present at enqueue. A sibling attempt may
finish before reconciliation: remaining captured siblings are still
interrupted. Any unfinished attempt outside the captured turn/set proves the
generation advanced, records `interrupt_outcome: "generation_advanced"`, and
is never interrupted.

`interrupt_subagent` uses the same phases with control kind
`scoped_subagent_interrupt`. Its `ready` transition also settles the ledger row
as consumed; the marker is excluded from queue claims and never becomes a user
message or steer. Replaying the same durable ID returns its prior phase/outcome
and therefore cannot interrupt a newer generation. Model calls always replace
provider-supplied IDs with `tool-call:<tool_call_id>` at runtime.

Scoped steer/control and completion lock the delegation row first and perform a
fresh state check under that lock. Completion/cancellation-first rejects a new
control; control-first leaves an active mailbox row that prevents completion
until the row settles. Distinct accepted steers remain distinct equal-priority
mailbox rows and do not overwrite one another.

Retries with the same `client_control_id`, child, message, and interrupt mode
return the original `input_id` with `replayed: true`; reusing the id for a
different control returns `client_control_id_conflict`. Model tool calls derive
the id from their tool-call id and overwrite any provider-supplied hidden ID.
`accepted: true` means the mailbox row committed. It does not alone mean the
interrupt was applied or the text was consumed: `phase` and `drive_status`
report that progress, and `interrupted` is `null` while the interrupt outcome is
still pending. After a durable acceptance, a reconciliation/drive failure is
returned as `accepted: true`, `drive_status: "failed"`, and a diagnostic
`drive_error`, rather than as a rejection that invites duplicate text.

Completion, scoped control, and cancellation serialize on the delegation row.
Completion/cancellation-first rejects a new control; control-first leaves an
active row that prevents completion. Whole-delegation cancellation atomically
marks active child queue rows and controls cancelled before exact-child runtime
cancellation, so terminal delegation projections do not retain active mailbox
work. Exact-session `input.interrupt`, exact-child `interrupt_subagent`, and
whole-delegation cancellation remain separate scopes.

This ledger is practical at-least-once retry protection, not a claim of global
exactly-once execution. A caller that deliberately creates a new id creates a
new control; a response can be lost after commit, but retrying the same id
returns the durable prior state. External tool/network side effects that
occurred before interruption remain non-transactional and are not rolled back.

### `delegation.list`

Lists a bounded newest-first page of delegations for a parent session. This is
the lightweight Agents-outline feed used by the web UI; use `delegation.status`
for structured detail or `delegation.read_handoff_file` for Handoff content.

```json
{ "parent_session_id": "parent-session", "limit": 3 }
```

Result:

```json
{
  "parent_session_id": "parent-session",
  "limit": 3,
  "has_more": false,
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
          "status": "done",
          "activity": "idle",
          "role": "implementer",
          "title": "Implement change",
          "subagent_type": "full",
          "task_prompt_file": "session_.../task_prompt.md",
          "transcript_file": null,
          "final_message_file": null,
          "outcome": null
        }
      ]
    }
  ]
}
```

### `delegation.read_handoff_file`

Reads a valid delegation handoff file. Normal running/done/cancelled/failed
delegations expose per-subagent `task_prompt.md`. Normal running/done
delegations expose per-subagent `transcript.md`; terminal
done/done_with_failures delegations also expose per-subagent `final_message.md`.
Cancelled delegations expose the transcript-only cancellation artifact path
reported by `inspect_delegation`, for example
`cancelled/<subagent_id>.transcript.md`. The structured delegation snapshot
comes from `delegation.status`/`inspect_delegation`, not from a handoff root
artifact file. Raw task prompts, full final messages, and full transcript bodies
are never inlined in delegation snapshots, daemon observations, or compaction
ledgers; use this RPC to read an artifact body explicitly when detail is needed.

Allowed `file` values are exactly:

- `task_prompt.md` with matching `subagent_id`
- `final_message.md` with matching `subagent_id`
- `transcript.md` with matching `subagent_id`
- `cancelled/<subagent_id>.transcript.md` (the `subagent_id` parameter is
  optional, but if present it must match the path)

```json
{
  "parent_session_id": "parent-session",
  "delegation_id": "delegation_...",
  "subagent_id": "session_...",
  "file": "task_prompt.md"
}
```

Cancellation transcript request:

```json
{
  "parent_session_id": "parent-session",
  "delegation_id": "delegation_...",
  "file": "cancelled/session_abc.transcript.md"
}
```

Result:

```json
{
  "delegation_id": "delegation_...",
  "subagent_id": "session_abc",
  "file": "cancelled/session_abc.transcript.md",
  "content": "# Transcript for cancelled subagent session_abc\n\n..."
}
```

## Subagent events

When a delegation subagent is spawned or re-driven, the daemon may emit
parent-scoped `subagent.spawned` and `subagent.running` progress events. These
are progress hints only. Parent-visible delegation completion is not a per-child
`subagent.idle`; it is one `InputPriority::Steer` daemon observation queued to
the parent after the delegation barrier completes. The observation is stored as a
typed `daemon_tool_observation` transcript item and is inspect-equivalent to
`inspect_delegation`/`delegation.status`, including per-subagent
`outcome` and artifact paths. Provider adapters
render it as an adjacent synthetic `inspect_delegation` tool call/result pair;
the UI renders it as a daemon/system observation card. Use
`inspect_delegation`/`delegation.status` to refresh/recover state or inspect
later/running; use the per-subagent `task_prompt.md`, `final_message.md`, and
`transcript.md` files for extra detail.

`subagent.idle` remains an event type for non-delegation subagent compatibility
(for example, defensive dispatch-failure compensation). When emitted, idle
notifications are de-duplicated per completed terminal child state, not for the
child session lifetime.

## MCP inventory and tools

### MCP OAuth status and user actions

The daemon exposes five user-only MCP authentication methods. They are
independent of `mcp.inventory`, so a configured login-required server remains
visible even when it has no coherent tool inventory. Every method is scoped to
the runtime that hosts the MCP servers (`runtime_id`); the control plane proxies
to that runtime over the framed-JSON conduit. OAuth credentials live on the
runtime host (under its `workspace_root`), not in the control plane.

`mcp.status` accepts `{ "runtime_id": "runtime-local" }` and returns every
configured server on that runtime:

```json
{
  "servers": [{
    "server": "remote",
    "auth_kind": "oauth",
    "auth_state": "login_required",
    "can_login": true,
    "can_logout": false
  }]
}
```

`auth_kind` is `none`, `bearer`, or `oauth`. `auth_state` is
`not_applicable`, `ready`, `login_required`, `reauthentication_required`,
`authorization_pending`, `unsupported`, or `unknown`. The optional `failure`
is one of the fixed categories `credential_store_unavailable` or
`discovery_failed`; provider bodies and credential details are never returned.

- `mcp.login` takes `{ "server": "remote", "runtime_id": "…" }` and, only after
  an explicit user action, returns `login_id`, `authorization_url`, and
  `expires_at_unix_seconds`. A second login for the same server fails with the
  fixed `mcp_oauth_login_already_pending` error. The loopback OAuth callback
  binds on the runtime host.
- `mcp.complete` takes `server`, `login_id`, `runtime_id`, and the **entire**
  `callback_url`. The coordinator validates the exact transaction redirect and
  state, including the stable Codex-compatible callback path; a bare code or
  state is not accepted. Success returns
  `{ "completed": true }`.
- `mcp.cancel` takes `server`, `login_id`, and `runtime_id` and returns
  `{ "cancelled": true }` only after the listener and transaction are cleaned
  up.
- `mcp.logout` takes `{ "server": "remote", "runtime_id": "…" }` and returns
  `{ "result": "removed" }` or `{ "result": "not_found" }`. It deletes only
  local runtime-host credentials and does not call a provider revocation
  endpoint, matching Codex CLI logout.

Server IDs, login IDs, and callback URLs are bounded. Errors use fixed local
codes/messages and contain no provider response, token, client ID, scope,
credential path, callback/authorization URL, code, state, or verifier. The
websocket has no implicit OAuth callback handling; manual remote-daemon
completion is only through `mcp.complete`.

### `mcp.inventory`

Requires `provider: "openai" | "claude"` and `runtime_id`, and returns the
bounded configured New Session inventory for that runtime:

```json
{
  "revision": "sha256...",
  "servers": [{
    "server": "workspace",
    "revision": "sha256...",
    "health": "healthy",
    "tools": [{
      "raw_name": "read_file",
      "description": "Read a file",
      "context_token_estimate": 94
    }]
  }]
}
```

The inventory and per-server revisions are semantic hashes; health is excluded.
`context_token_estimate` is computed from that provider's exact declaration
JSON and estimates additional MCP declaration context only, not total model
context. The frontend has a distinct provider-keyed inventory cache; inventory
refreshes never overwrite an existing session's tool inspector.

### `tools.list`

Requires a `provider` parameter (`"openai"` or `"claude"`) and returns the
model-visible tool definitions for that provider, because the tool surface is
provider-shaped (e.g. OpenAI `apply_patch` vs Anthropic `text_editor_20250728`
for editing). Callers may pass `session_id` so the daemon can derive the actual
session profile; if omitted, parent/global behavior is preserved. Parent/default
sessions see the registered builtins (`edit`, `bash`, `web_search`, `web_fetch`,
`LoadSkill`) plus delegation tools (`delegate_writing_task`,
`delegate_readonly_tasks`, `inspect_delegation`, `cancel_delegation`,
`steer_subagent`, `interrupt_subagent`). Structurally subagent sessions get the filtered subagent
surface without delegation tools. `prompt_profile` may be supplied only as a
fallback when no `session_id` is available. There are no `read`/`write` tools.
Each returned entry carries `name`, `description`, `input_schema`,
`canonical_name`, `prompt_alias`, `execution`, and `kind: "local_tool"`.

With a `session_id`, the response also includes only that session's frozen MCP
tools. These entries use `kind: "mcp_tool"` and add observational `source`, raw
`server`/`raw_name`, `manifest_fingerprint`, `contract_fingerprint`, and
`health` fields. Without `session_id`, `tools.list` is first-party-only;
`mcp.inventory` exclusively owns New Session discovery. Health is not part of
provider declarations or the persisted prompt. Exact provider declarations,
not this inspector response or PI.md prose, determine what a model may call.

Full and read-only delegation children inherit the parent's exact MCP manifest;
only parent-specific first-party delegation tools are filtered from child
profiles. Read-only status constrains the child's local filesystem view, not
remote MCP side effects.

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
reported by the delegation wakeup observation/handoff described above.

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
partial event payloads. Event payloads are notification/invalidation signals,
not content storage: daemon wakeup observation `input.queued` events do not
inline prose summaries, full result JSON, final messages, or task prompts.

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
      "model": "gpt-5.6-sol",
      "prompt_cache": { "key": "pi-relay-real-smoke" }
    },
    "metadata": { "manual": "real-codex" },
    "client_input_id": "ci-real-codex",
    "content": [
      { "type": "text", "text": "Reply with exactly: websocket real codex ok" }
    ]
  }
}
```

`session.start` creates the session and immediately starts this first turn.
Verify a real `model.completed`, assistant transcript entry, prompt-cache-key
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
`provider.kind = "claude"` and ask the model to use the `edit`
tool (`text_editor_20250728`) or `bash`. Verify provider-requested tool calls,
real tool results, a second model request containing those results, and a final
assistant message.

## Documentation Sync Rule

Whenever a crate boundary, RPC method, lifecycle rule, provider credential
path, storage invariant, or manual exercise changes, update this file,
`rust/docs/architecture.md`, `rust/README.md`, crate READMEs as needed, and
`rust/WORKLOG.md` in the same change.
