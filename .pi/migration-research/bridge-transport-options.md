# Bridge/Transport Architecture Options — pi-relay Web Frontend → prime-agent-core Backend

**Author:** bridge-architect subagent (READ-ONLY research; no files in any repo were modified)
**Date:** 2026-02-22
**Purpose:** Enumerate and analyze every viable transport/embedding architecture for making pi-relay's React web frontend (Cloudflare Pages static build; browser opens WSS to a daemon) drive a prime-agent-core backend (RLM/IPython harness) instead of pi-relay's Rust daemon (`pi-agentd`).

## Table of Contents

1. Sources & Method
2. The Two Wire Protocols Today
   - 2.1 pi-relay websocket-rpc.md contract (inventory)
   - 2.2 pi-relay web frontend connection layer
   - 2.3 prime-agent daemon protocol (inventory)
   - 2.4 prime-agent RPC / JSON / ACP / SDK surfaces
   - 2.5 pi-mono client / protocol / server packages
   - 2.6 oh-my-pi headless options
3. Semantic Gap Analysis — pi-relay semantics with NO prime-agent counterpart
4. Command-by-Command Mapping Table (pi-relay RPC → prime-agent daemon commands)
5. Candidate Architectures A–G
6. Cross-Cutting Concerns Matrix
7. Recommendation: Primary + Fallback
8. Appendix: Event-name tables; capability negotiation notes


---

## 1. Sources & Method

All claims below are grounded in these files, read directly (no builds, no daemons started, no processes touched):

| Source | Path |
|---|---|
| pi-relay WS contract | `/home/schwinns/pi-relay/rust/docs/websocket-rpc.md` (100,537 bytes, read in full) |
| pi-relay web WS client | `/home/schwinns/pi-relay/packages/web/src/rpc.ts` (`AgentRpcClient`) |
| pi-relay profile model | `/home/schwinns/pi-relay/packages/web/src/serverProfiles.ts` (`ServerProfileStore`) |
| pi-relay typed API | `/home/schwinns/pi-relay/packages/web/src/agentApi.ts` (51 distinct RPC methods called) |
| pi-relay event→refresh map | `/home/schwinns/pi-relay/packages/web/src/sessionEvents.ts` |
| pi-relay app shell / profiles | `/home/schwinns/pi-relay/packages/web/src/serverApp.tsx`, `App.tsx`, `connectionRecovery.tsx` |
| prime-agent architecture doc | `/home/schwinns/pi-relay/.pi/prime-agent-architecture.md` (esp. §2 API/RPC Surface) |
| prime-agent installed dist | `/home/schwinns/.npm-global/lib/node_modules/prime-agent/dist/modes/{daemon,rpc,json,acp,agent-connection}/` |
| prime-agent daemon protocol | `dist/modes/daemon/daemon-protocol.{js,d.ts}` — full command/event enums quoted below |
| prime-agent DaemonClient | `dist/modes/daemon/daemon-client.{js,d.ts}` |
| prime-agent session events | `dist/modes/agent-connection/types.d.ts` (`AgentConnectionSessionEvent`) + `node_modules/@earendil-works/pi-agent-core/dist/types.d.ts` (`AgentEvent`) |
| prime-agent repo | `/home/schwinns/pi-relay/.pi/migration-research/repos/prime-agent/` (`packages/{agent,ai,coding-agent,tui}`, `prime-agent-runtime`) |
| pi-mono | `/home/schwinns/pi-relay/.pi/migration-research/repos/pi-mono/packages/{client,protocol,server}/` (READMEs + all non-test src) |
| oh-my-pi docs | `/home/schwinns/pi-relay/.pi/migration-research/repos/oh-my-pi/docs/{rpc.md,sdk.md,collab.md,agent-hub.md}` |

---

## 2. The Two Wire Protocols Today

### 2.1 pi-relay websocket-rpc.md contract — inventory

Source: `/home/schwinns/pi-relay/rust/docs/websocket-rpc.md`. Implemented by `agent-daemon` (`pi-agentd`). Design pillars quoted from the doc:

- **Sessions are durable rows, not opened processes.** No `open`/`close` RPC, no session-level resume; idle sessions resume implicitly because Postgres is authoritative (Core Decisions 1–3).
- **Postgres is authoritative.** Every accepted transition is committed to Postgres before follow-on model/tool work is dispatched. Tables: `sessions`, `projects`, `daemon_config`, `transcript_entries` (append-only forest with `parent_id`, `provider_replay` sidecar, `sequence`), `queued_inputs` (durable queue, `priority: steer|follow_up`, `client_input_id` idempotency, dense `follow_up_position`), `actions` (durable model/tool/compaction work with `attempt_id` fences), `delegations`, `events` (transient reconnect buffer, cleared at idle).
- **Activity is derived**: `idle | queued | running` only.
- **History writes and snapshots are idle-only**: `history.switch`, `history.fork`, `session.configure`, `mcp.add`, `compaction.request` fail `session_busy` while active.
- **Tools always allowed**; `input.interrupt` is the only cancellation command.
- **Daemon death is recoverable state** — startup reconciliation of actions/leases/intents.

**Transport acceptance** (§Browser connection acceptance): daemon upgrades only requests with exactly one `Origin` header exactly matching a configured canonical browser origin. Missing/duplicate/`null`/malformed/non-HTTP(S)/noncanonical origins rejected pre-RPC. Frames/messages capped at 8 MiB. This is explicitly *not* CORS and *not* arbitrary-client authn — tailnet ACLs/SSH are the authorization boundary. WSS required remotely; cleartext WS only to loopback.

**RPC envelope**: requests `{id, method, params}`; responses `{id, ok:true, result}` or `{id, ok:false, error:{code,message,data}}`; live events `{event_id, event, session_id, data}`; lossy request-correlated progress frames `{id, progress}` (used by `session.start` workspace materialization: phases `refreshing_base|copying|branch_override|done|error`).

**Full RPC method inventory** (from the doc's section headers; web client wrapper file in parentheses uses the same names — `agentApi.ts` additionally calls `runtime.list`, which is NOT documented in websocket-rpc.md but exists in the daemon):

- Session: `session.start` (stable client-chosen `session_id`, `client_input_id`, idempotent replay → `{replayed:true}`, optional `project_id`/workspaces subset + branch overrides, MCP manifest selection with `inventory_revision` fence, progress frames), `session.list` (durable sessions by last user message; hides empty/hidden), `session.get` (recover-if-needed + durable snapshot: `activity`, `active_leaf_id`, `pending_actions`, `queued_inputs`, `session_revision`/`queue_revision`/`transcript_revision`, `last_event_id`, `server_time_ms`, optional `entries` with `entries_scope: active_branch|full_tree`), `session.rename`, `session.configure` (idle-only provider/metadata replace; effort-only updates allowed while active), `session.sync_active_branch` (delta vs `base_leaf_id`: `unchanged|extended|branch_changed`), `session.delete` (idle-only, cascades, returns deleted child ids).
- Transcript: `transcript.index` (compact topology: nodes with `item_type`, `can_switch_to`, `edit_target_leaf_id`, `display_hint`; `transcript_revision` is freshness token, `sequence` is pagination cursor), `transcript.entries` (sparse bodies by id), `transcript.turns` (paged turn cards — the normal hot-path UI endpoint), `transcript.turn_detail` (expanded turn bodies).
- History: `history.tree` (all entries + active leaf), `history.context` (materialized model context for leaf), `history.targets` (paged editable user-message targets for `/switch` picker with daemon-resolved `target_leaf_id`), `history.switch` (idle-only; move `active_leaf_id` to committed turn boundary or root; fences: `expected_active_leaf_id`, `expected_transcript_revision`, `source_entry_id`, `active_branch_entry_ids`; errors `session_busy|not_turn_boundary|history_changed`; optional `return_active_branch`), `history.fork` (idle-only, managed project sessions only; duplicates session incl. full transcript forest + `provider_replay` + filesystem snapshot of current idle cwd via btrfs/reflink; child gets new id; not retry-idempotent), `turn.resume` (idle-only; restart terminal Crashed/Interrupted turn from its model-action checkpoint; output becomes sibling branch).
- Input: `input.follow_up` (durable queue row first, then background drive; `client_input_id` idempotency; `expected_active_leaf_id` fence → `history_changed`; response is canonical queue projection with `accepted/queued/replayed` flags + revisions), `input.promote_queued` (follow-up → steer lane; mid-turn steer injection between tool results and next model request), `input.update_queued` (edit queued follow-up; `expected_queue_revision` fence), `input.cancel_queued`, `input.reorder_queued_follow_ups` (full ordered id list; dense positions rewritten), `input.interrupt` (exact-session only; marks actions interrupted, aborts task registry, emits `session.work_cancelled`; emits `input.ignored` when idle). Raw follow-up to a subagent session is allowed but validated (`delegation_not_running`); raw steer to a child is rejected (`subagent_steer_requires_parent_scope`) — parent scope required.
- Delegation: `delegation.start_full` (one writing subagent in parent cwd; `client_launch_id` replay-safe), `delegation.start_readonly_fanout` (one read-only subagent per task in disposable btrfs snapshots), `delegation.status` (canonical structured snapshot: progress counts, per-subagent role/type/activity/status/`steerable`/`outcome`/handoff file refs), `delegation.cancel`, `delegation.steer_subagent` (parent-scoped; optional `interrupt:true` with durable phases `pending_interrupt → interrupt_applied → ready|cancelled`; `client_control_id` idempotency; ledger-based at-least-once), `delegation.list` (active + bounded terminal history feed), `delegation.read_handoff_file` (`task_prompt.md`/`final_message.md`/`transcript.md`/`cancelled/<id>.transcript.md`). Limits: one running full delegation + read-only fan-outs totaling ≤8 slots per parent.
- Workspace: `workspace.list_dir` (paged shallow listing), `workspace.read_file` (base64 ranged chunks ≤4 MiB), `workspace.watch` (per-session fs-interest; changes published as ephemeral `workspace.fs_changed` events with `event_id: 0` that must NOT advance the high-water mark), `workspace.git_status` (`against: working_tree|branch`; PR metadata via `gh`), `workspace.git_diff` (unified diff capped ~1 MiB).
- MCP/tools: `mcp.status`, `mcp.login`, `mcp.complete`, `mcp.cancel`, `mcp.logout` (OAuth lifecycle scoped by `runtime_id`; fixed local error codes; no secrets over the wire), `mcp.inventory` (semantic-hash `revision`, per-server health + tool list + `context_token_estimate`, `selected_servers` + `session_revision` fence when `session_id` passed), `mcp.add` (idle-only, root-session-only, unions selection, atomically installs manifest + rerendered prompt, emits `mcp.tools_added`), `tools.list` (provider-shaped tool surface; `local_tool` builtins incl. delegation tools; `mcp_tool` entries with fingerprints/health when `session_id` given).
- System: `system.prompt` (PI.md template + rendered prompt), `compaction.request` (idle-only; provider-backed compaction writing a compacted transcript root transactionally; interruptible), `harness.model.complete`, `harness.model.fail` (dev-only).
- Subscription: `events.subscribe` (`after_event_id` reconnect stream; bounded 500-event pages with `has_more`/`next_after_event_id` continuation; `null` starts at head), `events.unsubscribe`.

**Event set** (§Event Set): `session.created`, `session.configured`, `mcp.tools_added`, `input.accepted`, `input.queued`, `input.consumed`, `input.promoted`, `input.updated`, `input.cancelled`, `input.reordered`, `input.ignored`, `transcript.appended`, `turn.started`, `turn.finished`, `assistant.message`, `action.requested`, `model.requested`, `model.completed`, `model.error`, `tool.requested`, `tool.started`, `tool.completed`, `tool.error`, `compaction.requested`, `compaction.completed`, `compaction.error`, `history.switched`, `history.compacted`, `session.work_cancelled`, `session.recovered`, `session.idle`, `subagent.spawned`, `subagent.running`, `subagent.idle`, plus ephemeral `workspace.fs_changed` (`event_id: 0`). Events are explicitly **freshness hints / invalidation signals, not content storage** — clients refetch canonical projections (queue events carry the canonical post-transition queue projection; `transcript.appended` carries the entry + `tree_node` + revisions).

Subagent completion model: parent-visible completion is ONE steer-priority daemon wakeup observation queued to the parent after the whole delegation barrier completes, stored as a typed `daemon_tool_observation` transcript item; `subagent.spawned`/`subagent.running` are parent-scoped progress hints replayable until the parent's event-buffer cleanup.

### 2.2 pi-relay web frontend connection layer

Files: `packages/web/src/{rpc.ts,serverProfiles.ts,agentApi.ts,sessionEvents.ts,serverApp.tsx,App.tsx,connectionRecovery.tsx}`.

- **`AgentRpcClient` (`rpc.ts`, 9.6 KB)**: one `WebSocket` per profile URL. JSON-RPC-style envelope exactly matching §RPC Envelope above. Client-generated ids `web_N`. Distinguishes `RpcRequestError` (typed server error with `code`) from `RpcTransportError` (outcome unknown — socket closed/timed out after send; doc comment: "The server may still have executed it, so callers with non-idempotent requests must reconcile"). Default request timeout 15 s; workspace operations 330 s (deliberately above the daemon's 300 s `MaterializeSession` budget). Handles request-correlated `progress` frames via `onProgress`. Half-open detection: on timeout, if no frame arrived for the whole window, closes the socket to force reconnect. Auto-reconnect: 750 ms fixed-delay loop (`scheduleReconnect`), plus manual `reconnect()`. No auth handshake, no protocol-version negotiation — the Origin check + WS URL is the whole acceptance model.
- **`ServerProfileStore` (`serverProfiles.ts`)**: multi-profile model = a list of `{id, name, url}` in `localStorage` (`piRelayServerProfiles:v1`), active profile id in `sessionStorage`, per-profile namespaced storage. URL validation: `ws://` or `wss://` only, no credentials/fragment, **`ws://` allowed only for loopback hosts** (`127.0.0.1`, `localhost`, `::1`); default `ws://127.0.0.1:8787/` when served from loopback. **Multi-daemon support today = multiple WS URLs, one connection at a time** (profile switch remounts `ConnectedServer` keyed by `id:url`, which builds a fresh `AgentRpcClient` + React Query client).
- **`agentApi.ts`**: `createAgentApi(new AgentRpcClient(profile.url))`; wrappers calling **51 distinct RPC methods** (45 of the 46 documented + 6 undocumented: **`runtime.list`**, `mcp.cancel`, `mcp.complete`, `mcp.login`, `mcp.logout`, `mcp.status`; the only documented-but-uncalled method is `delegation.status`). `runtime.list` returns `{runtimes: Runtime[]}` and is used by the MCP pickers to scope `mcp.inventory`/`mcp.status` by `runtime_id`. Any contract-preserving backend must implement all six undocumented methods.
- **`sessionEvents.ts`**: pure mapping from event name → `{syncSelected, refreshList}` refresh plan. Unknown events conservatively trigger `syncSelected`. Confirms the client treats events as invalidation hints and re-pulls canonical state (`session.sync_active_branch`, `session.get`, `transcript.turns`, `session.list`).
- **`App.tsx` / `serverApp.tsx` / `connectionRecovery.tsx`**: per-session `last_event_id` high-water marks; on (re)subscribe it calls `events.subscribe({after_event_id})` and follows continuation pages; snapshot commits take `max(observed, snapshot.last_event_id)`. Disconnected UI blocks remote actions (`remoteActionBlockedReason`) with a manual retry banner (`ConnectionRetryController`). Composer allows only local slash commands (`/help`, `/export`) while disconnected.

**Frontend coupling summary**: the frontend is deeply coupled to (a) the `{id,method,params}` envelope, (b) the 34-event vocabulary, (c) the three revision counters + `last_event_id` freshness model, (d) queue projections embedded in input responses, (e) `transcript.turns`-style paged turn cards, (f) history/delegation/MCP/workspace RPC surface. It is NOT coupled to any pi-relay transport beyond "one JSON text WebSocket per daemon URL".


### 2.3 prime-agent daemon protocol — inventory

Sources: `dist/modes/daemon/daemon-protocol.{js,d.ts}`, `daemon-client.d.ts`, `daemon-supervisor.js`, `daemon-socket.js`, `dist/modes/agent-connection/types.d.ts`, `node_modules/@earendil-works/pi-agent-core/dist/types.d.ts`, and `/home/schwinns/pi-relay/.pi/prime-agent-architecture.md` §1–§2.

**Topology** (arch doc §1): detached **supervisor** owns the public socket and all client connections, routes commands to per-root-tree **workers** (resident or client-owned), manages worker crash recovery (250 ms/1 s/5 s, 3 failures = root failed), command journals for idempotency/crash recovery, and two-phase coordinated updates. Workers own the actual `AgentSessionRuntime`/`AgentSession` (provider calls, queues, tools, compaction, goals, RLM children, IPython kernels, schedulers). If the supervisor dies, a worker acquires an atomic launch lease and starts a replacement that adopts live workers.

**Transport**: `node:net` `createServer` on a Unix socket (`daemon-supervisor.js:453`; path from `daemon-socket.js`, default `~/.prime/agent/daemon.sock`, mode 0600, dir 0700, proper-lockfile lease; Windows named pipe `\\.\pipe\prime-agent-daemon`). **There is no TCP, HTTP, or WebSocket listener anywhere in the daemon modes** (`rg createServer` over `dist/modes` finds only `node:net` in the supervisor and the kernel fork-server). The protocol file's own header comment is the key seam:

> "This is the transport used by DaemonAgentConnection today, **not the final remote gateway protocol**. The protocol primitives below are intentionally JSON-serializable so **a future gateway can wrap or proxy this local transport** without leaking transport details back into InteractiveMode." — `dist/modes/daemon/daemon-protocol.js` header

**Framing**: strict LF-delimited JSONL (one JSON object per line). Envelope types (`daemon-protocol.js`):

- Command: `{type:"command", id, protocol:{name:"prime-agent.daemon", version:7}, clientId?, command:{...}}`
- Response: `{id, type:"response", command, success:true/false, data?/error?, errorInfo?}` — note **stringly `error` message, no stable machine codes** (unlike pi-relay's `{code,message,data}`).
- Event: `{type:"event", id:"<activeSessionId>:<sequence>", protocol, activeSessionId?, sequence?, cursor?:{generation,sequence}, emittedAt, event:{...}}`
- Request-scoped progress: `DaemonRequestProgress` outbound type (used e.g. by `list` → `session_list_progress`/`session_list_item`).
- Greeting: `daemon_hello` on connect: `{socketPath, protocol, schemaId:"protocol-7-schema-13-816309b1cd50", schemaRevision:13, appVersion, runtime, supervisorGeneration, supervisorPid, supervisorOwnerToken, supervisorProcessStartId, supervisorSocketPath, clientId, serverCapabilities}`. `daemon_closing {reason}` signals planned shutdown/restart.

**Capability negotiation**: client sends `capabilities` in `attach` metadata; supported client caps: `attach_snapshot`, `event_sequence`, `extension_ui`, `slim_attach`, `chunked_snapshot`, `client_owned_sessions`. Server caps add `delete_rlm_child`-family, `heartbeat_catalog`, `heartbeat_management`, `model_catalog`, `side_question_transcript`, `transient_bash`, `session_input_admission`, `prompt_admission_cancellation`. Per-command compatibility table `DAEMON_COMMAND_COMPATIBILITY` (minProtocol / minSchemaRevision / required capability) — e.g. `prompt`/`steer`/`follow_up`/`prompt_and_wait`/`resume_queue` require `session_input_admission`; `cancel_prompt_admission` requires schema ≥8 + `prompt_admission_cancellation`.

**Command inventory** (~100; from `DAEMON_COMMAND_COMPATIBILITY` + the `DaemonCommand` union in `daemon-protocol.d.ts`):

- Lifecycle/attach: `list`, `list_saved_sessions`, `create` (sessionPath?/continueRecent?/noSession?/name?/config?/runtimeMetadata?/lifecycle `resident|client_owned`, allowlisted client env `HERDR_*` only, launch env), `attach` (→ `DaemonAttachResult`: snapshot + replay info + `lastEventSequence`/`lastEventCursor`; `chunked_snapshot` cap moves messages into a `session_snapshot_begin/chunk/end` stream), `reattach`, `detach`, `complete_owned_session`, `promote_owned_session`, `kill`, `rename`.
- Input: `prompt` (message/content/images, `streamingBehavior:"steer"|"followUp"`, `queueIfBusy`, `source`, `agentMessageId`, `customMessage`, optional cancellable `admissionId`), `prompt_and_wait`, `steer`, `follow_up`, `cancel_prompt_admission`, `restore_next_turn`, `restore_actions`, `append_custom_message`, `resume_queue`.
- Messaging: `send_message` (session→session agent messaging; `agentOrigin` internal), `agent_messages_status|pause|resume|clear`.
- Control: `abort`, `abort_bash`, `abort_compaction`, `abort_branch_summary`, `abort_retry`, `execute_bash` (`transient`, `runId`), `execute_bash_and_wait`, `cancel_rlm_child`, `delete_rlm_subagent`, `wait_for_idle`, `wait_for_headless_completion`.
- Introspection: `get_session_header`, `get_state`, `get_connection_state`, `get_messages`, `get_session_stats`, `get_context_tree`, `get_commands`, `get_resource_snapshot`, `get_model_catalog`, `get_available_models`, `get_queue`, `clear_queue`, `abort_and_clear_queue`, `get_session_context`, `get_session_tree`, `get_user_messages_for_forking`, `get_last_assistant_text`, `get_system_prompt`, `get_tool_definition`, `set_session_entry_label`.
- Scheduling: `cron_list/add/cancel`, `heartbeats_list`, `heartbeat_manage/get/set/update`.
- Model/config: `set_model`, `cycle_model`, `set_scoped_models`, `set_thinking_level`, `cycle_thinking_level`, `set_service_tier`, `set_transport`, `set_steering_mode`, `set_follow_up_mode`, `set_auto_compaction`, `set_auto_retry`.
- Compaction/refine: `compact` (`customInstructions?`), `refine` (continual-harness refinement), 
- Session tree ops: `new_session` (`parentSession?`), `switch_session` (by session file path, `cwdOverride?`), `fork` (`entryId`, `position:"before"|"at"` — **in-file branch navigation**, see below), `navigate_tree` (`targetId`, `summarize?`, `customInstructions?`, `replaceInstructions?`, `label?` — tree navigation with optional summarization), `import_jsonl`, `export_html`, `export_jsonl`, `set_session_name`.
- RLM: `get_rlm_max_depth_status`, `set_rlm_max_depth`.
- Saved-session catalog: `rename_saved_session`, `delete_saved_session`.
- Extension UI: `extension_ui_response` (answers `extension_ui_request`).
- Supervisor ops: `ack_result`, `prepare_update_restart`, `retry_worker`, `restart`, `shutdown`.

**Outbound/event inventory** (`DAEMON_OUTBOUND_COMPATIBILITY` + `DaemonOutbound` union): `response`, `session_list_progress`, `session_list_item`, `daemon_hello`, `daemon_closing`, `heartbeats_changed`, `session_event` (carries `AgentConnectionSessionEvent`), `side_question_event`, `session_status` (recap), `session_replaced` (new state+messages; `snapshotFollows?`), `session_resynced` (full `DaemonSessionSnapshot`), `session_attached`, `session_snapshot_begin` (snapshot sans messages + `messageCount`, `targetChunkBytes`, `purpose:"attach"|"replacement"|"resync"`), `session_snapshot_chunk` (indexed `AgentMessage[]`), `session_snapshot_end` (`lastEventSequence`/`lastEventCursor`), `session_snapshot_failed`, `session_detached`, `session_closed` (reason), `extension_ui_request`, `extension_error`.

**Inner session events** (`AgentConnectionSessionEvent` in `dist/modes/agent-connection/types.d.ts`) = pi-agent-core `AgentEvent` (`agent_start`, `agent_end`, `turn_start`, `turn_end`, `message_start`, `message_update` (streaming deltas via `assistantMessageEvent`), `message_end`, `tool_execution_start`, `tool_execution_update`, `tool_execution_end`) **plus** daemon-level: `ipython_sent_agent_message`, `session_action_update` (durable action snapshot), `compaction_start`/`compaction_end` (`reason: manual|threshold|overflow|requested`, result/aborted/willRetry/errorSeverity), `session_info_changed`, `thinking_level_changed`, `service_tier_changed`, `auto_retry_start/end`, `auth_stale`, `rlm_child_update` (live RLM child snapshot — the closest thing to pi-relay's `subagent.*`), `recap_update`, `goal_update`, `bash_start/bash_output/bash_end`, `refine_complete/refine_failed`.

**Event replay semantics — important**: `createDaemonReplayInfo()` in `daemon-protocol.js` shows replay is **not** a general after-the-fact event log: with no resume cursor → `status:"complete"` (start at head); same sequence → `complete`; generation mismatch → `unavailable`/`event_generation_changed`; cursor ahead → `unavailable`/`resume_cursor_ahead_of_session`; anything else → `unavailable`/`event_replay_not_available`. The recovery model is **snapshot-based resync** (`session_resynced` / attach snapshot / chunked snapshot), not event-log replay. This contrasts with pi-relay's `events.subscribe(after_event_id)` durable reconnect buffer (500-event pages) — see §3.

**Attach semantics**: `attach` returns `DaemonAttachResult {activeSessionId, snapshot{summary,state,messages,sessionContext?,sessionTree?{tree,leafId},lastEventSequence,lastEventCursor,parent?,children?}, replay, client:{id,capabilities}}`. With `slim_attach`, `state`/`messages` are omitted (use snapshot). With `chunked_snapshot`, messages stream as `session_snapshot_*`. `DaemonSessionSnapshot.sessionTree` gives `{tree: AgentConnectionSessionTreeNode[], leafId}` — the in-file branch topology — and `children` gives live RLM child snapshots (including grandchildren). `parent` links a child session to its parent node.

**Sessions as files**: tree-structured JSONL at `~/.prime/agent/sessions/<id>.jsonl` (v3 schema; `id`/`parentId` per entry — in-place branching without new files; arch doc §1 "Session Model", §3). Process-safe leases keyed by canonical path; concurrent opens fail `session_already_active`.

**DaemonClient** (`daemon-client.d.ts`): `connect/reconnect/disconnect/close`, `waitForHello`, `supportsServerCapability`, `request(command, timeoutMs, {onProgress})`, `enableRequestRecovery()` (resends stable envelopes after reconnect), `enableAutoReconnect({recoverDaemon,...})` (for supervisor replacement), `authenticateWorker`/`requestWorker` (private worker channel: 4-byte JSON header length | 4-byte payload length | JSON routing header | opaque payload; per-worker tokens fenced to supervisor generation). Errors: `DaemonSocketClosedError` (with `daemonClosingReason`), `DaemonCapabilityUnavailableError`.

### 2.4 prime-agent RPC / JSON / ACP / SDK surfaces

From arch doc §2.B–E and the installed dist:

- **RPC mode** (`--mode rpc`; `dist/modes/rpc/rpc-mode.js`): headless single-session process; stdin/stdout LF-delimited JSONL; `InProcessAgentConnection` (NOT a daemon client — it hosts one in-process session). Commands observed in the switch: `prompt`, `steer`, `follow_up`, `abort`, `new_session`, `get_state`, `set_model`, `cycle_model`, `get_available_models`, `set_thinking_level`, `cycle_thinking_level`, `set_steering_mode`, `set_follow_up_mode`, `compact`, `refine`, `set_auto_compaction`, `set_auto_retry`, `abort_retry`, `bash`, `abort_bash`, `get_session_stats`, `export_html`, `switch_session`, `fork`, `clone`, `get_fork_messages`, `get_last_assistant_text`, `set_session_name`, `get_messages`, `send_message`, `agent_messages_status|pause|resume|clear`, `list_schedules`, `add_schedule`, `cancel_schedule`, `list_heartbeats`, `get_heartbeat`, `set_heartbeat`, `update_heartbeat`, `manage_heartbeat`, **`observe`** / **`unobserve`** (watch another active session in the same process: streams `observed_session_event` / `observed_session_closed`, returns current messages), `get_commands`, and extension-UI `extension_ui_request` bridging. Events are the raw `AgentConnectionSessionEvent` stream. This is a per-session embedding protocol, not a multi-session control plane.
- **JSON mode** (`--mode json`): print-mode variant emitting all events as JSON lines.
- **ACP mode** (`--mode acp`; `dist/modes/acp/acp-mode.js`, `acp-events.js`, `acp-meta.js`): implements Agent Client Protocol via `@agentclientprotocol/sdk` for editor integrations (Zed-style). Analyzed in §5.C.
- **TypeScript SDK** (`dist/index.d.ts`; repo `packages/coding-agent/src/index.ts`): exports `createAgentSession`, `createAgentSessionRuntime`, `AgentSession` (`prompt, steer, followUp, subscribe, setModel, compact, abort, navigateTree, dispose`), `AgentSessionRuntime` (session replacement across new/switch/fork/import), `SessionManager` (`create, open, continueRecent, inMemory, forkFrom, list, listAll`), `ModelRegistry`, `AuthStorage`, **`DaemonClient`** + all daemon protocol types (`DaemonCommand`, `DaemonOutbound`, `DaemonSessionSnapshot`, `defaultDaemonSocketPath`, … — confirmed in repo `packages/coding-agent/src/index.ts`), `InteractiveMode`, `runPrintMode`, `runRpcMode`, extension system. So a Node bridge can `import { DaemonClient } from "prime-agent"` (package `@earendil-works/pi-coding-agent`, exports `.` and `./hooks`) — no need to re-implement framing.
- **Repo check for other transports**: `rg createServer` over `repos/prime-agent/packages/{agent,coding-agent}/src` finds only `core/kernel/fork-server.ts` (IPython kernel fork server) and `modes/daemon/daemon-{mode,supervisor}.ts` (Unix socket). **No HTTP/WebSocket server transport exists in prime-agent today.** WebSocket mentions in `packages/ai` are provider-side (Codex websocket transport to OpenAI), unrelated to client-facing transports.


### 2.5 pi-mono client / protocol / server packages

Repo: `/home/schwinns/pi-relay/.pi/migration-research/repos/pi-mono/packages/` (version 0.84.1). **A separate, newer, explicitly experimental line of work** — not the protocol prime-agent's daemon speaks.

- **`@earendil-works/pi-protocol`** (`protocol/README.md`, `protocol/src/schemas.ts`): transport-neutral **binary** wire protocol — `[uint32-be length][definite-length CBOR item]`, strict RFC 8949 subset, TypeBox schemas with `additionalProperties:false`, 16 MiB default frame limit. First client message `hello {version:1}`. Design mottos: "Session and server snapshots are authoritative. Progress events are transient UI hints and must not be reduced into authoritative state." **Command surface is only 9 verbs**: `list`, `create` (cwd/name/model/thinkingLevel), `attach`, `detach`, `prompt`, `steer`, `abort`, `set_model`, `set_thinking`. Server events: `server_snapshot`, `session_snapshot` (full `SessionSnapshot`: id/name/cwd/phase `idle|turn|compaction|branch_summary|retry`/model/thinkingLevel/attached/locked/`revision`/`transcript: TranscriptItem[]`/`queuedSteer` + `queuedSteerCount`), `session_progress` (`item_started|assistant_delta|item_updated|item_finished`), `session_removed`. Errors: closed set `version|busy|session_locked|not_found|invalid_request|not_implemented|internal_error`. **No tree navigation, no fork, no compaction control, no queue editing/reorder, no delegation, no MCP, no workspace/file/git RPCs, no events-log replay.** "The protocol is experimental and has no compatibility guarantees."
- **`@earendil-works/pi-client`** (`client/README.md`): `PiClient` over a user-supplied `ByteTransport` (WebSocket explicitly mentioned as an example factory); no Node-specific imports in the root export — **browser-usable in principle**, but you'd need a CBOR stack in the web bundle and a WS→bytes listener server-side. Lease model: `createSession()` → exclusive lease; `acquireSession({mode:"exclusive"|"shared"})`; `attachSession()` shared; no auto-reconnect (`reconnect()` manual); server snapshots are authoritative; structured `PiServerError`.
- **`@earendil-works/pi-server`** (`server/README.md`, `server/src/types.ts`): `PiServer` composes `PiServerListener`s (only a Unix listener ships: `pi-server/unix`; "a WebSocket listener can validate credentials during the HTTP upgrade" is mentioned as an exercise for the reader). **The agent loop is fully behind a user-supplied service interface**: `PiServerService { listSessions(), listModels(), createSession(options) → PiSessionRuntime, openSession(sessionId) → PiSessionRuntime }` where `PiSessionRuntime { snapshot(), getPhase(), prompt(), steer(), abort(), setModel(), setThinking(), subscribe(), dispose() }` and "Conflicting operations must reject rather than queue." "This package does not provide a standalone CLI or coding-agent service. Applications supply the PiServerService implementation." Nothing inside pi-mono's coding-agent currently implements `PiServerService` (`rg PiServerService` finds only the server package itself) — it's a scaffold.
- **Relationship to prime-agent**: pi-mono `coding-agent@0.84.1` depends on `pi-client`/`pi-protocol` and has `src/client/remote-session.ts` + `src/server/create-harness.ts` (an `AgentHarness` factory wiring read/bash/edit/write tools + system prompt). prime-agent (0.7.1, the migration target) does NOT consume pi-protocol; its daemon speaks the JSONL protocol of §2.3.


### 2.6 oh-my-pi headless options

Repo: `/home/schwinns/pi-relay/.pi/migration-research/repos/oh-my-pi/` (`packages/{agent,ai,coding-agent,tui,collab-web,browser-relay,...}`). Docs read: `docs/{rpc.md,sdk.md,collab.md,agent-hub.md}`. OMP is a pi-mono descendant with a much larger feature surface, but its headless/control surfaces are:

- **RPC mode** (`omp --mode rpc`; `docs/rpc.md`; canonical schema in `packages/coding-agent/src/modes/rpc/rpc-types.ts`): single-session stdin/stdout JSONL, 1 MiB physical frames, optional protocol v2 with base64 `rpc_chunk` reassembly up to 64 MiB. Commands: `prompt`/`steer`/`follow_up`/`abort`/`abort_and_prompt`/`new_session`, state/model/thinking/queue-mode getters+setters (`set_steering_mode`, `set_follow_up_mode`, `set_interrupt_mode` with `one-at-a-time|all` / `immediate|wait` semantics), compaction (`compact`), retry, `bash`, session/`messages` reads, login, plus host-tool / host-URI / extension-UI sub-protocols. Prompts are **ack'd immediately; completion is signalled only by `agent_end` with `isTerminal !== false`**. **Subagent visibility exists here**: `set_subagent_subscription: off|progress|events`, `get_subagents`, `get_subagent_messages` (byte-offset incremental transcript tailing with `reset` semantics). Client libraries: TS (`rpc-client.ts`, spawns `bun <cli> --mode rpc`) and Python (`python/omp-rpc`, `omp_rpc.RpcClient`). **All per-process, single-session, no multi-session control plane, no network transport.**
- **SDK** (`docs/sdk.md`): in-process TS embedding via `createAgentSession()` with auto-discovery; `AgentSession.subscribe(listener)` event model = core `AgentEvent` + session events (`auto_compaction_*`, `auto_retry_*`, `model_changed`, `goal_updated`, …). Same embedding class as prime-agent's SDK.
- **Collab** (`docs/collab.md`; `packages/collab-web`, `packages/coding-agent/src/collab/`): live session *sharing* — host TUI is authoritative; a **content-blind Go relay** serves the static web guest at `/` and upgrades `GET /r/<roomId>?role=host|guest` to WebSocket; E2E encrypted with the key in the URL fragment. Frame taxonomy: host→guest `welcome`, `snapshot-chunk` (byte-bounded transcript chunks), `entry` (durable session entries broadcast pre-blob-externalization), `event` (live agent events into the guest's normal event controller), `state` (debounced footer snapshots), `bus` (task-subagent lifecycle mirror), `agents` (agent-registry snapshots for a guest-local Hub), `ui-request(-end)`; guest→host `hello`, `prompt`, `abort`, `agent-cmd`, `fetch-transcript`, `ui-response`. This is a **working existence proof of "static web app + WSS → agent session" with snapshot+event streaming and subagent panels** — but the relay is deliberately content-blind (no semantics can live there), the host is a full omp TUI process, and guests are replicas, not controllers of a multi-session daemon.
- **Agent Hub** (`docs/agent-hub.md`): TUI-only roster/inspector for subagents (running/idle/parked/aborted; revive/kill/steer). Not a network surface.
- No HTTP/WebSocket **control** server exists in OMP (`rg WebSocketServer` finds only collab-web's dev mock/local relay, the collab relay client, and the browser-automation relay).

**Net relevance to the migration**: OMP validates the browser-WSS-to-agent pattern (collab-web) and offers a richer RPC-mode subagent-observability design to crib from, but provides no reusable multi-session server. Its queue-mode vocabulary (`steer`/`followUp` × `one-at-a-time`/`all`, `interruptMode immediate|wait`) is the same pi-agent-core vocabulary prime-agent exposes via `set_steering_mode`/`set_follow_up_mode` daemon commands.


## 3. Semantic Gap Analysis — pi-relay concepts with no prime-agent counterpart

This section enumerates every pi-relay semantic that has **no native counterpart** anywhere in prime-agent's current surfaces, plus the subtler cases where a counterpart exists but with materially different semantics. Each gap is tagged: **[bridge]** = a bridge/adapter can implement it without touching prime-agent core; **[core]** = requires new prime-agent code (daemon command and/or AgentSession API); **[drop]** = probably has to be dropped or redesigned.

### 3.1 Durable, replayable event log — **[core]**

pi-relay's defining property: every session event is a Postgres row (`events` table) with a per-session monotonically increasing `id`; `events.subscribe { after_event_id, limit }` returns paged replay (`500`-event pages, `has_more`, `next_after_event_id`); the web client reconnects, replays from its high-water mark, and refetches canonical snapshots (`sessionEvents.ts` maps event types to refetch plans; App.tsx tracks `last_event_id` per session).

prime-agent's daemon protocol has event **sequences and cursors** (`createDaemonEventMeta`, `DaemonEventCursor {generation, sequence}`) but **explicitly no replay store**: `createDaemonReplayInfo()` returns `complete` only when the client already has the newest sequence; any actual gap yields `unavailable` with `reason: "event_replay_not_available" | "event_generation_changed" | "resume_cursor_ahead_of_session"` (`daemon-protocol.js`). The recovery model is **re-attach → fresh authoritative snapshot (`session_attached` / chunked `session_snapshot_*`) → live events from there**, plus `session_replaced`/`session_resynced` push frames. Sessions are JSONL files (no DB anywhere in prime-agent — arch doc §7).

**Nuance that shrinks the gap**: pi-relay's event durability is itself only a *reconnect buffer*, not an archive — per the doc's exercise plan, "once the session is idle, the event buffer for that session is empty", and `events.subscribe(after_event_id: null)` deliberately starts at the head with **no** historical replay. Both systems therefore already agree that **snapshots are canonical and events are freshness hints** ("Events are freshness hints, not a generic patch protocol"). What pi-relay adds — and prime-agent lacks — is *durability across reconnects while work is in flight*.

Consequences: (a) an event-log shim must persist daemon `session_event` frames keyed by `(activeSessionId, sequence)` — bridge-side SQLite/Postgres/file spool, drained on idle mirroring pi-relay semantics — if the pi-relay frontend's `events.subscribe` contract is kept; or (b) the frontend must be re-tooled for snapshot-based resync (a substantial rewrite of `connectionRecovery.tsx` + `sessionEvents.ts` invalidation model). Note the daemon's in-memory per-session event ring (if any) is bounded and generation-scoped; a worker restart changes the generation and voids all cursors.

### 3.2 Durable, editable, reorderable queued inputs — **[core]**

pi-relay: `queued_inputs` table rows with ids, positions, kind (`follow_up`/`steer`), content, `input.promote_queued`, `input.update_queued`, `input.cancel_queued`, `input.reorder_queued_follow_ups`, `input.follow_up` (enqueue semantics). The web UI edits and drag-reorders queued items; they survive daemon restarts (Postgres).

prime-agent: `steer()`/`followUp()` enqueue **in-memory** queue entries; `get_queue` returns `{steering: string[], followUp: string[]}` — text previews, **no ids, no positions**; `clear_queue`, `removeQueuedFollowUp(queueKey)` (keyed coalescing only), `set_steering_mode`/`set_follow_up_mode` (`one-at-a-time|all`), and an internal `ActionStore` with lifecycle states (`queued→selected→preparing→committing→running→completed|failed|cancelled`, `DeliveryRecord.durable`, `SessionActionRecoverySnapshot`) used for **crash/update-restart recovery** (`restore_actions`, `DaemonUpdateRestartManifest.queue`), not as a user-facing durable queue. There is no update-in-place, no reorder, no per-item cancel by id, no promotion from follow-up lane to steer lane. The `session_input_admission` capability and `prompt.admissionId`/`cancel_prompt_admission` give cancellable *pre-ownership* admission, not queue editing.

### 3.3 Client-initiated subagent spawn — **[core]**

pi-relay: `delegation.start_full {task, model?...}` and `delegation.start_readonly_fanout` are client RPCs — the **user** spawns subagents from the UI, plus `delegation.status/cancel/steer_subagent/list/read_handoff_file`.

prime-agent: RLM children are spawned **by the agent** from inside the IPython kernel (`rlm('task')`); daemon commands only expose `cancel_rlm_child`, `delete_rlm_subagent`, `get_context_tree`, `get_rlm_max_depth_status`, `set_rlm_max_depth`, `rlm_child_update` events, and `children` in the attach snapshot (`AgentConnectionRlmChildAgentSnapshot`: id/parentId/activeSessionId/label/status/recap/tokenCount…). A client **can** attach directly to a child's `activeSessionId` and `prompt`/`steer` it through normal session commands (that is how steering a subagent would be built), but there is no daemon command to *create* one. Closest client-triggerable approximation: `prompt` the parent with instructions to call `rlm(...)` — indirect, model-dependent.

### 3.4 Workspace file/git RPCs — **[bridge]**

pi-relay: `workspace.list_dir`, `workspace.read_file`, `workspace.watch` (ephemeral `workspace.fs_changed` events with `event_id 0`), `workspace.git_status`, `workspace.git_diff` — served by the daemon against the session's working directory.

prime-agent: nothing. `get_resource_snapshot` enumerates *agent resources* (context files, skills, prompts, extensions, themes), not workspace files. A bridge can implement these directly against the session's `cwd` (known from `get_state`) with plain fs/git calls — straightforward, but it is new code living in the bridge, and the fs-watch push must be synthesized (Chokidar/`fs.watch`).

### 3.5 MCP server management over RPC — **[bridge, mostly]**

pi-relay: `mcp.*` RPC group (list/enable/disable/auth flows against the daemon).

prime-agent: MCP servers are configured in `settings.json` (`mcpServers`: http|stdio configs, `enabled`, `enabledTools`/`disabledTools`; OAuth creds in `auth.json` as `mcp:<name>`, login/refresh host-side via `McpManager`, kernel-side integration reads creds). **No daemon commands** for MCP. A bridge could expose list/toggle by reading/writing `settings.json` + calling a reload (`reload` command exists), but OAuth login flows are host-side and would need bespoke bridging. Also note pi-mono/OMP both have richer MCP stories (OMP `docs/mcp-*`); prime-agent's is minimal.

### 3.6 Transcript paging/index RPCs vs snapshot+JSONL — **[bridge]**

pi-relay: `transcript.index`, `transcript.entries`, `transcript.turns`, `transcript.turn_detail` — paged reads over normalized Postgres rows (turns as first-class rows).

prime-agent: `get_messages` (full current-branch message array — potentially large; chunked snapshot delivery exists for attach: `session_snapshot_begin|chunk|end`), `get_session_tree` (whole tree as nested `AgentConnectionSessionTreeNode`), `get_session_context`, `export_jsonl`. **No server-side paging** on the daemon protocol; turn semantics exist only implicitly (turn boundaries inferred from user/assistant alternation + `compaction`/`branch_summary` entries). A bridge can synthesize `transcript.*` by loading the snapshot/JSONL once and serving paged/indexed views bridge-side (OMP's `get_subagent_messages` shows the byte-offset paging pattern for JSONL).

### 3.7 Session "start" vs create/attach/reattach — mostly mappable, semantics differ

pi-relay `session.start {id?, profile...}`: idempotent start-or-attach against a *running daemon-owned* session registry in Postgres; sessions are daemon-resident regardless of clients.

prime-agent: `create` (spawns worker/session; `sessionPath?`, `continueRecent?`), `attach`/`reattach` (client binding; leases! `session_already_active` on concurrent open), `detach`, `kill`, `complete_owned_session`/`promote_owned_session` (client-owned lifecycle), plus idle **eviction/passivation** (`canEvictWorker`, `canPassivateSession`, `idleEvictionMinutes` setting). Sessions are **not** guaranteed daemon-resident: workers get evicted when idle; `attach` rehydrates. The web frontend's "session always exists server-side" assumption mostly holds via lazy rehydration, but attach latency after eviction and lease conflicts (`session_already_active`) are new failure modes the frontend doesn't model.

### 3.8 Smaller gaps

- **`runtime.list`** (undocumented pi-relay RPC the frontend calls): prime-agent has no runtime inventory concept; bridge must answer from supervisor state (`list` gives sessions; runtime/worker topology is `retry_worker`/supervisor-internal). **[bridge]**
- **`history.targets`** (fork-target listing): prime-agent `get_user_messages_for_forking` = equivalent. ✅
- **`history.switch`** → `navigate_tree` (in-place, optional branch summary via `summarize`/`customInstructions`/`label` — richer than pi-relay); **`history.fork`** → `fork {entryId, position: before|at}` (new session file). ✅ with richer options. **`history.tree`** → `get_session_tree`. **`history.context`** → `get_session_context`. ✅
- **`turn.resume`** (resume an interrupted turn from Postgres state): prime-agent's `resume_queue`/`restore_next_turn`/auto-resume-on-attach (`shouldResume`, `wasStreaming` in update-restart manifest) cover crash recovery internally, but there is **no client "resume this turn" verb**. **[core]** (or drop: auto-resume may suffice)
- **`compaction.request {customInstructions?}`** → `compact` + `abort_compaction` + `set_auto_compaction`; events `compaction_start/end` exist in `AgentConnectionSessionEvent`. ✅ (plus `refine` — prime-agent-only).
- **`system.prompt`** (fetch current system prompt): → `get_system_prompt`. ✅
- **`session.list/get`** → `list`/`list_saved_sessions`, `get_state`, `get_session_header`, `get_session_stats`. ✅ (`SessionSummary` vs pi-relay session row: names, cwd, ids differ but mappable.)
- **Origin-based single-tenant acceptance** (pi-relay accepts exactly one canonical Origin; WSS remote / WS loopback only): prime-agent daemon is unix-socket local-only, so any exposed network surface is bridge-owned policy. TLS termination, Origin checking, auth tokens = all bridge. **[bridge]** (pi-mono `pi-server` listener notes "a WebSocket listener can validate credentials during the HTTP upgrade" — same conclusion.)
- **Multi-profile** (`serverProfiles.ts`): purely client-side profile list — any backend that terminates WSS at a URL works; per-profile isolation requires separate daemon/bridge endpoints. Neutral.
- **Bash**: pi-relay has no user-bash RPC; prime-agent has `execute_bash`/`abort_bash`/`transient` bash + events. Web UI could gain features; no gap.
- **Extension UI requests** (`extension_ui` capability, `extension_ui_response`): prime-agent extensions can push UI requests to attached clients; pi-relay has no analog — new capability, not a gap.


## 4. Command-by-Command Mapping Table

Columns: **Match** ✅ = near-equivalent exists; ~ = partial/semantics differ; ✗ = no counterpart. **Impl** = where the gap must be built: **PA** = prime-agent core (new daemon command / AgentSession API), **BR** = bridge layer only.

### 4.1 Session lifecycle

| pi-relay RPC (frontend call) | prime-agent daemon command | Match | Impl | Notes |
|---|---|---|---|---|
| `session.start {id?…}` | `create {sessionPath?, continueRecent?, name?, config?}` + `attach {activeSessionId}` | ~ | BR | pi-relay start-or-attach is idempotent; daemon separates create/attach, enforces leases (`session_already_active`), and may lazily rehydrate an evicted worker. Bridge must hide lease/retry churn. |
| `session.list` | `list {all?, cwd?}` / `list_saved_sessions` | ✅ | — | Response shape differs (`SessionSummary` vs Postgres row); bridge reshapes. |
| `session.get {include_entries}` | `attach` → `DaemonSessionSnapshot {summary, state, messages, sessionTree, children}` | ~ | BR | Daemon's attach snapshot is the cold-load path (chunked `session_snapshot_*` for large ones). No opt-out of entries; "include_entries=false" ≈ snapshot.summary only. |
| `session.rename` | `rename {activeSessionId, name}` / `set_session_name` | ✅ | — | |
| `session.configure` | `set_model`, `set_thinking_level`, `set_steering_mode`, `set_follow_up_mode`, `set_auto_compaction`, … | ~ | BR | pi-relay batches config into one RPC; daemon is one verb per knob. Bridge fan-out or frontend multi-call. |
| `session.sync_active_branch {base_leaf_id}` | none — compute from `get_state` (`leafId`) + `get_messages` diff | ✗ | BR | The frontend's hot refresh path (unchanged/extended/branch_changed). Bridge can implement by comparing leaf ids and message-array tails; must also emit the "revisions" the frontend expects. |
| `session.delete` (idle-only, cascades to children) | `kill {activeSessionId}` + `delete_saved_session` | ~ | BR/PA | `kill` stops a live session; `delete_saved_session` removes the file. Idle-only guard + recursive hidden-child cascade = bridge policy; `session_busy` semantics exist daemon-side. |
| `project.list/create/update/delete` | none (no project entity) | ✗ | BR | Projects are pi-relay's grouping concept. Bridge can model projects as a tag/cwd mapping, or frontend drops project UI. |
| `runtime.list` (undocumented but called) | none | ✗ | BR | Bridge fabricates from supervisor state; or frontend drops. |

### 4.2 Transcript & history

| pi-relay RPC | daemon command | Match | Impl | Notes |
|---|---|---|---|---|
| `transcript.index` / `transcript.entries` / `transcript.turns` / `transcript.turn_detail` | `get_messages`, `get_session_tree`, `get_session_context` | ~ | BR | Daemon returns whole-branch/tree structures, no paging. Bridge pages over a loaded snapshot/JSONL. Turn rows must be synthesized from message boundaries + compaction entries. |
| `history.tree` | `get_session_tree` | ✅ | — | Nested `AgentConnectionSessionTreeNode` with labels; pi-relay returns flat forest — reshape in bridge. |
| `history.context` | `get_session_context` | ✅ | — | Returns `{messages, thinkingLevel, serviceTier, model}`. |
| `history.switch {leaf_id}` | `navigate_tree {targetId, summarize?, customInstructions?, label?}` | ✅+ | — | Daemon adds optional branch-summary on abandon. In-place, same file — same semantics. |
| `history.targets` | `get_user_messages_for_forking` | ✅ | — | |
| `history.fork {entry_id}` | `fork {entryId, position: before|at}` | ✅ | — | Both create a new session file/row. Daemon `switch_session`/`clone` cover adjacent flows. |
| `turn.resume {leaf_id?}` | none (auto-resume on attach after crash; `resume_queue`, `restore_next_turn` are internal recovery) | ✗ | PA | No client verb to re-drive a crashed/interrupted model turn from a checkpoint. pi-relay's "move leaf back to checkpoint, fresh action, sibling branch" has no analog. Auto-recovery may cover the common case; explicit resume needs core work or is dropped. |

### 4.3 Input / queue / interrupt

| pi-relay RPC | daemon command | Match | Impl | Notes |
|---|---|---|---|---|
| `input.follow_up {content, lane: steer|follow_up}` | `prompt {streamingBehavior}` / `steer` / `follow_up` | ~ | BR | Daemon steer/followUp enqueue in-memory; `queueKey` coalescing exists. Content blocks map (`content?: (TextContent|ImageContent)[]`). |
| `input.promote_queued {id}` (follow-up → steer) | none | ✗ | PA | No lane promotion. |
| `input.update_queued {id, content}` | none | ✗ | PA | Queue entries immutable; no ids. |
| `input.cancel_queued {id}` | `removeQueuedFollowUp(queueKey)` (not a daemon command); `clear_queue`; `abort_and_clear_queue`; `cancel_prompt_admission {admissionId}` (pre-ownership only) | ~ | PA | Per-item cancel needs queue items to carry ids/keys over the wire — `get_queue` returns text previews only. |
| `input.reorder_queued_follow_ups {ids}` | none (`ActionStore.enqueueFront` exists internally) | ✗ | PA | |
| `input.interrupt {session_id}` (exact session, no cascade) | `abort {activeSessionId}` | ✅ | — | Also per-session in daemon. `abort_compaction`/`abort_branch_summary`/`abort_retry`/`abort_bash` give finer control the frontend doesn't use. |

### 4.4 Delegation / subagents

| pi-relay RPC | daemon command | Match | Impl | Notes |
|---|---|---|---|---|
| `delegation.start_full {task,…}` | none (children are agent-spawned via `rlm()`) | ✗ | PA | Client-initiated spawn needs a new daemon command, or an indirect `prompt`("spawn subagent…") hack. |
| `delegation.start_readonly_fanout {tasks[]}` | none | ✗ | PA | Same. |
| `delegation.list` | attach snapshot `children[]` + `rlm_child_update` events; `get_context_tree` | ~ | BR | Live roster exists; pi-relay's durable delegation records (handoffs) vs prime-agent's ephemeral `AgentConnectionRlmChildAgentSnapshot` + persisted artifacts. |
| `delegation.status {id}` | (same roster) | ~ | BR | |
| `delegation.cancel {id}` | `cancel_rlm_child {childId}` | ✅ | — | |
| `delegation.steer_subagent {id, message}` | `reattach`/second client to child's `activeSessionId` + `steer`/`prompt` | ~ | BR | Children expose `activeSessionId` precisely so clients can attach directly. Bridge multiplexes. |
| `delegation.read_handoff_file {path}` | none (artifacts live in `session-artifacts/<id>/`) | ✗ | BR | Bridge serves artifact files from the session-artifacts dir (workspace-style read). |

### 4.5 Workspace / MCP / tools / system / events / compaction

| pi-relay RPC | daemon command | Match | Impl | Notes |
|---|---|---|---|---|
| `workspace.list_dir` / `read_file` / `watch` / `git_status` / `git_diff` | none | ✗ | BR | Bridge implements fs+git against session `cwd`; synthesizes `workspace.fs_changed` (event_id 0, ephemeral). |
| `mcp.inventory` / `mcp.add` / `mcp.login` / `mcp.logout` / `mcp.status` / `mcp.cancel` / `mcp.complete` | none (settings.json `mcpServers` + host-side OAuth via `McpManager`; `reload` exists) | ✗ | BR/PA | List/toggle = bridge edits settings + `reload`. OAuth login flows are interactive host-side — hardest part; likely scoped out initially. |
| `tools.list` | `get_commands` + `get_resource_snapshot` + `get_tool_definition` | ~ | BR | Different vocab (slash commands/skills/prompts vs tool inventory). |
| `system.prompt` | `get_system_prompt` | ✅ | — | |
| `events.subscribe {after_event_id, limit}` / `events.unsubscribe` | attach (snapshot) + live `session_event` stream; **no replay** (`event_replay_not_available`) | ✗ | BR/PA | See §3.1 — the deepest gap. Bridge-side event spool, or frontend moves to snapshot-resync. |
| `compaction.request {custom_instructions?}` | `compact {customInstructions?}`, `abort_compaction`, `set_auto_compaction` | ✅ | — | Events `compaction_start/end` exist. |

### 4.6 Scoreboard

- **Direct daemon equivalents (✅/✅+)**: ~10 of 51 frontend-called methods.
- **Bridge-implementable without prime-agent changes (~, BR)**: ~22 — includes the entire workspace group, transcript paging, session.sync_active_branch, delegation list/status/steer-via-attach, tools.list, session.get/list reshaping.
- **Requires prime-agent core work (PA)** or redesign: ~10 — queue edit/promote/reorder/cancel-by-id, `turn.resume`, client-initiated subagent spawn, durable event replay (if kept), MCP-over-RPC (partial).
- **Drop/redesign candidates**: `project.*`, `runtime.list`, `mcp.login/logout` flows (short term).


## 5. Candidate Architectures

Terminology: **browser** = pi-relay React app on Cloudflare Pages; **bridge** = new network-facing service we would write; **daemon** = prime-agent supervisor+workers (unix socket); **core** = `@earendil-works/pi-agent-core`/prime-agent in-process libraries.

---

### Option A — Thin WS⇄unix-socket tunnel; browser speaks daemon protocol verbatim

**Shape**: a small Node service (or Caddy/NGINX-style proxy with a framing shim) accepts WSS, opens a connection to the daemon's unix socket, and pipes LF-delimited JSONL frames both ways, one daemon connection per browser tab. The browser becomes a *daemon client* — implements `daemon_hello` (capability negotiation), `create/attach/reattach`, consumes `session_attached`/snapshot chunks/`session_event`/`session_replaced`/`session_resynced`.

- **Frontend changes**: **large protocol-layer rewrite, app shell survives.** Replace `rpc.ts` envelope with daemon framing + hello handshake; replace `agentApi.ts` methods with daemon commands (`attach`, `prompt`, `steer`, `navigate_tree`, …); rewrite `connectionRecovery.tsx` around attach-snapshot-resync (no `events.subscribe(after_event_id)` — on any reconnect, re-attach and apply a fresh authoritative snapshot); remap `sessionEvents.ts` invalidation to `AgentConnectionSessionEvent` names (`message_start/update/end`, `tool_execution_*`, `compaction_start/end`, `session_action_update`, `rlm_child_update`…). Transcript UI renders from snapshot `messages`/`sessionTree` instead of paged `transcript.*`. Queue UI degrades to preview/clear (no edit/reorder/promote — §3.2). Delegation spawn buttons removed or routed through prompts (§3.3). Workspace/MCP/project panels lose their data source unless a companion bridge (below) is added.
- **Backend code**: the tunnel itself is ~200–400 LOC (WS accept, Origin/TLS policy, socket dial, byte pipe, backpressure, per-connection daemon client). Optionally run a WS listener **inside the supervisor** (variant **A′**: patch `daemon-supervisor.ts` to also listen on TCP/TLS) — removes a process but forks prime-agent.
- **Tree navigation**: native (`navigate_tree`, `fork`, `get_session_tree` — richer than pi-relay: branch summaries, labels).
- **Subagents**: native roster via snapshot `children[]` + `rlm_child_update`; steering = bridge opens a second daemon connection attached to the child's `activeSessionId` (frontend already handles per-session views). No client-side spawn.
- **Steers/queues**: `steer`/`follow_up`/`abort`/`clear_queue`/`set_*_mode` native; durable+editable queue lost (in-memory, previews only).
- **Compaction**: native (`compact`, `abort_compaction`, `set_auto_compaction`, events).
- **Recovery**: attach-with-snapshot; missed events are *not* replayable — frontend must treat any reconnect as a full state reload (snapshot is chunked for big sessions). Crashed turns auto-resume on attach (`shouldResume`/`wasStreaming` machinery); explicit `turn.resume` disappears.
- **Multi-profile**: each profile URL = a tunnel in front of a daemon. Works.
- **TLS/Origin**: tunnel terminates TLS, enforces Origin allowlist (pi-relay's "exactly one canonical Origin" rule is trivially portable), can add an auth token. The daemon socket itself stays local — good blast-radius containment.
- **Ops complexity**: +1 tiny stateless process per host. Lowest new-code count of all options. Protocol version drift handled by hello negotiation (`DAEMON_PROTOCOL_VERSION=7`, schema rev 13).
- **Fatal flaw / cost**: pushes prime-agent's *client* complexity into the browser (hello/caps, snapshot application, resync semantics, lease errors) and *loses* pi-relay's durability features the UI exposes (queue editing, event replay, paged transcripts). Every future daemon-protocol change touches the shipped web app.

---

### Option B — Stateful bridge implementing pi-relay's websocket-rpc contract over `DaemonClient`

**Shape**: a new long-lived Node/TS service terminates WSS exactly like `pi-agentd` does today (same `{id,method,params}` envelope, same event names, same Postgres model externally), and internally drives the prime-agent daemon through the SDK's `DaemonClient` (one per active session, plus a supervisor-level client for `list`). The bridge owns the semantic-debt layer: an event spool (SQLite/Postgres/append-files) to serve `events.subscribe(after_event_id)`, a durable queue shim, transcript paging over snapshots/JSONL, workspace fs/git handlers, project table, `runtime.list`.

- **Frontend changes**: **~zero.** This is the only option that keeps the shipped frontend (including `runtime.list`, queue editing, paged transcripts, Origin/8 MiB assumptions) working as-is.
- **Backend code**: the largest of A–C but all in one new place: (1) protocol façade (50 methods, mapping table §4); (2) event persistence — subscribe to each session's `session_event` stream and append `(session_id, seq, event)` so `after_event_id` replay works (must also synthesize pi-relay-only events: `input.queued/updated/reordered`, `delegation.*`, `workspace.fs_changed`); (3) durable queue store — *the subtle one*: to honor `input.update_queued/cancel_queued/reorder`, the bridge should hold follow-ups itself and only release to the daemon at turn boundary (listen for `agent_end`/`idle`, then `prompt`/`follow_up`), while steer-lane goes straight to daemon `steer`. Two sources of queue truth (bridge store + daemon in-memory queue) must be reconciled on daemon restart (daemon loses its in-memory queue; bridge store wins — actually simplifies recovery); (4) workspace fs/git + watch; (5) transcript paging from `get_messages`/JSONL; (6) project/registry tables.
- **Tree navigation**: full support by translation (`history.switch→navigate_tree`, `history.fork→fork`, `history.tree→get_session_tree` with reshape nested→flat forest, `history.context→get_session_context`, `history.targets→get_user_messages_for_forking`).
- **Subagents**: list/status from snapshot `children[]`+`rlm_child_update`; steer via second `DaemonClient` attach to child `activeSessionId`; cancel via `cancel_rlm_child`. `delegation.start_full/fanout`: either prompt-mediated (fragile) or defer until a `start_rlm_child` daemon command is added to prime-agent (small, well-scoped core patch: registry admit + `runRlmChild()` — the pieces exist; see §7).
- **Steers/queues**: full pi-relay semantics bridge-side (durable, editable, reorderable); mapping to daemon only at delivery time. Steer lane = daemon `steer` (immediate in-turn delivery preserved).
- **Compaction**: `compaction.request→compact`; events forwarded; `compaction.*` pi-relay events synthesized from `compaction_start/end`.
- **Recovery**: best of both — browser-facing durable replay (bridge spool) over daemon-facing snapshot-resync. Bridge restart = reconnect to daemon(s), rebuild from spool + fresh attach snapshots. Daemon restart = re-attach all sessions (new generation), mark spool continuity with a synthetic resync event.
- **Multi-profile**: one bridge per daemon host; profiles unchanged.
- **TLS/Origin**: bridge owns both, exactly like pi-agentd today (accept exactly one canonical Origin; 8 MiB frames; WSS remote/WS loopback).
- **Ops complexity**: +1 stateful service with its own small DB. Heaviest build, but it is the *only* option that preserves every shipped behavior without frontend surgery, and it concentrates all future prime-agent drift behind one adapter.
- **Risks**: (a) double-queue reconciliation bugs; (b) bridge must keep pace with daemon protocol revs (mitigated: `DaemonClient` + hello caps); (c) subagent spawn gap needs a core patch for full parity.

---

### Option C — ACP bridge (WS⇄ACP)

**Shape**: bridge translates browser WebSocket JSON-RPC into ACP (`@agentclientprotocol/sdk`) against `prime-agent --mode acp` processes.

- **Findings** (`dist/modes/acp/acp-mode.js`): ACP mode is **one session per process** ("a second session/new is refused"), **`loadSession: false`** (no resume of existing sessions), cwd fixed at process launch (client cwd rejected with `_meta` notice), capabilities = prompt(image, embeddedContext) + `session/close`. prime-agent extras ride in a reverse-domain `_meta` envelope. There is **no session list, no tree navigation, no queue management, no subagent surface, no compaction control** in ACP itself.
- **Verdict**: a browser-to-ACP bridge would re-implement essentially everything that matters (multi-session hosting, resume, tree, queues, events) *in the bridge*, on top of a protocol whose session model actively resists it. ACP's transport (NDJSON stdio per process) also means one process per session with no supervision, leases, or eviction — strictly worse than talking to the daemon. **Not viable as the primary bridge; listed for completeness.** ACP remains useful only if the frontend were replaced by an ACP-native editor client — out of scope.


### Option D — Drop the daemon; embed prime-agent core in a new server process

**Shape**: the bridge **is** the backend: a Node/Bun server `import`s the SDK (`createAgentSessionRuntime`, `AgentSession`, `SessionManager`, `McpManager`…), hosts one `AgentSessionRuntime` per session in-process, and serves either (i) the pi-relay WS contract or (ii) a daemon-like protocol over WS. No prime-agent daemon involved.

- **What you gain**: single process; direct, synchronous access to everything (`navigateTree`, `getContextTree`, `McpManager`, action store — the whole object graph, not the 96-command wire subset); queue editing/reorder could call internal APIs (`ActionStore.remove`, `enqueueFront`) that the daemon protocol doesn't expose; `turn.resume` could be built on internal checkpoint machinery; event log = tap `session.subscribe()` and persist.
- **What you lose / must rebuild** (this is what the daemon *is*): per-session **worker process isolation** (one bad session/kernel crash takes down every session in the bridge — prime-agent deliberately supervises workers, auto-restarts them, and journals recovery), session **leases**, idle **eviction/passivation**, **update-restart** with queue/action manifest handoff, heartbeat/cron supervision, the `DaemonClient` reuse path, and future daemon-side features. You'd also be pinned to prime-agent internals (no protocol stability promise at the SDK level either, but at least it's a published entry point — `dist/index.d.ts` exports these deliberately).
- **Frontend changes**: same as A if serving a daemon-like protocol, none if serving pi-relay's contract (like B, minus the daemon hop).
- **Variant D′ — `PiServerService` implementation**: implement pi-mono's narrow `@earendil-works/pi-server` interface over `AgentSessionRuntime` and expose it via a WS listener. The interface is only 9 verbs (§2.5) — far below the frontend's needs; you'd immediately outgrow it. Also pi-mono `coding-agent@0.84.1` ≠ prime-agent's core (`@earendil-works/pi-coding-agent@0.7.1`); straddling both is a migration *away* from the migration target. **Not recommended** beyond being the conceptual precedent that "the agent loop is pluggable behind a service interface."
- **Verdict**: architecturally the "cleanest" end-state and the only one that can fully implement pi-relay's queue/resume semantics without core patches, but it means **forking prime-agent's operational core** (supervision, recovery, leases). As a *first* bridge it's too much re-implementation; as a possible *end-state* after a B-phase it stays on the table.

---

### Option E — Adopt pi-mono pi-protocol/pi-client end-to-end (CBOR protocol, new frontend client)

**Shape**: put `@earendil-works/pi-protocol` (CBOR frames over WS) in the browser via `@earendil-works/pi-client`, and write a `PiServerService` over prime-agent core (or wait/use pi-mono coding-agent as the backend instead of prime-agent).

- **Frontend changes**: near-total rewrite of the data layer (binary framing, CBOR in the bundle, snapshot-authoritative state model, no events.subscribe, no queue editing, no tree nav — the protocol has *no* tree/fork/compaction-control/delegation/workspace commands at all, §2.5).
- **Backend**: a WS `PiServerListener` (doesn't exist — only unix listener ships) + a `PiServerService` over prime-agent (doesn't exist — nothing implements it today) + all the pi-relay semantics the protocol doesn't model (which is most of them).
- **Verdict**: **not viable now.** The protocol is self-described experimental/unstable and an order of magnitude narrower than the frontend's contract. Revisit only if the project later switches backends from prime-agent to pi-mono's coding-agent line (a different product decision).

---

### Option F — HTTP(S) RPC + server-push (SSE or WebSocket) hybrid

**Shape**: replace the single WS-RPC channel with REST/HTTP endpoints for commands/queries and SSE (or a lightweight WSS) for events.

- **Attraction**: Cloudflare-Pages-friendly, cacheable GETs for transcript pages, no long-lived RPC coupling, per-request auth.
- **Costs**: this is a **frontend architecture change, not just transport** — the entire `AgentRpcClient`/recovery/`sessionEvents` stack is built around one duplex channel with ordered events and request/response correlation; SSE is text-only (base64 for binaries), has no client→server channel (so input/interrupt still need POSTs — fine), and loses the single ordered event stream that `after_event_id` replay relies on unless carefully sequenced. Every pi-relay semantic gap from §3 still has to be implemented somewhere — this option is orthogonal to the daemon-vs-embed decision and can be combined with B or D later.
- **Verdict**: legitimate future direction for CDN-friendliness, but strictly more work than B for the same semantic coverage, and it throws away the working frontend contract. **Deferred.**

---

### Option G — OMP-collab-style host-authoritative relay (found during research)

**Shape** (existence proof, §2.6): a content-blind WS relay + static web guest client, host process authoritative, snapshot-chunk + entry/event/state/bus/agents frames, E2E key in URL fragment.

- **Why it's interesting**: proves a production static-web-over-WSS agent UI works well with **snapshot-first, events-second** streaming and a *content-blind* relay (TLS not even required at the relay for confidentiality). The frame taxonomy (durable `entry` frames separate from transient `event` frames; byte-bounded `snapshot-chunk`) is a good design reference for a future protocol.
- **Why it's not the answer here**: guests are read-mostly replicas of a *running host TUI session*; there is no multi-session control plane, no durable event store, and the relay deliberately cannot implement pi-relay semantics (it's content-blind/E2E). Adopting it would mean the web app attaches to a live interactive process per session — the opposite of pi-relay's daemon-owned session model.

---

### Options summary table

| | A tunnel | B façade bridge | C ACP | D embed core | E pi-protocol | F HTTP+SSE | G collab-style |
|---|---|---|---|---|---|---|---|
| Frontend changes | Large (protocol layer) | ~None | Total | None–Large | Total | Large | Total |
| New backend code | Tiny | Large (stateful) | Large | Large | Large | Large | N/A |
| Tree nav | Native | Translated ✅ | ✗ | Native | ✗ (protocol lacks) | =A/B/D | ✗ |
| Subagents (list/steer) | Native-ish | ✅ via attach | ✗ | Native | ✗ | =A/B/D | partial |
| Client spawns subagents | ✗ (core patch) | core patch | ✗ | buildable | ✗ | — | ✗ |
| Durable editable queue | ✗ | ✅ bridge-owned | ✗ | ✅ internal APIs | ✗ | — | ✗ |
| Durable event replay | ✗ (snapshot resync) | ✅ bridge spool | ✗ | ✅ tap+persist | ✗ (snapshots only) | ✅ spool | ✗ |
| Compaction control | Native | ✅ translated | ✗ | Native | ✗ | — | n/a |
| Workspace/MCP RPCs | ✗ unless companion | ✅ bridge fs/git; MCP partial | ✗ | Native fs; MCP via McpManager | ✗ | — | ✗ |
| Multi-profile | ✅ | ✅ | ✅ | ✅ | ✅ | ✅ | n/a |
| TLS/Origin | Bridge policy | Bridge policy (identical to today) | Bridge | Bridge | Bridge | Native HTTP | Relay-blind |
| Ops complexity | +1 stateless proc | +1 stateful svc+DB | +1/stateful + N procs | replaces daemon (+risk) | +1 + new stack | +1 | +relay +host |
| Keeps daemon supervision/recovery | ✅ | ✅ | ✗ | ✗ (must rebuild) | ✗ | =A/B/D | ✗ |

## 6. Cross-Cutting Concerns

Scoped to the three serious options (A tunnel, B façade, D embed) plus baseline pi-relay-today.

### 6.1 Event ordering & recovery

| | pi-relay today | A tunnel | B façade | D embed |
|---|---|---|---|---|
| Durable event log | Postgres `events`, paged replay | none (daemon: `event_replay_not_available`) | bridge spool (SQLite/Postgres/file) | tap `subscribe()` → persist |
| Reconnect path | `events.subscribe(after_event_id)` then targeted refetch | re-attach + full snapshot (`session_attached`, chunked) | unchanged (bridge serves replay) | free choice |
| Missed-event detection | `last_event_id` high-water vs response | sequence gaps in `session_event`; generation change voids cursors | bridge validates daemon seq continuity per session | n/a |
| Session replacement (switch/fork/navigate) | branch revisions + refetch | `session_replaced`/`session_resynced` frames | translate to pi-relay events | internal |
| Crash mid-turn | `turn.resume` RPC (checkpointed model action) | auto-resume on attach (`wasStreaming`/`shouldResume`); no explicit verb | auto + optionally synthesize `turn.resume` | buildable on checkpoint internals |

Key subtlety for B: the daemon emits per-session sequences scoped to a **generation** (`createDaemonEventMeta(generation=activeSessionId)`); a worker eviction+rehydrate or restart starts a new generation. The bridge spool must therefore record `(session_id, bridge_seq)` independently of daemon sequences and emit a synthetic resync marker when a generation change is detected, so the web client's `after_event_id` contract stays monotone.

### 6.2 Queue & delivery semantics

| | pi-relay today | A | B | D |
|---|---|---|---|---|
| follow_up durable/editable/reorderable | ✅ Postgres | ✗ (in-mem, previews) | ✅ bridge-owned store; release-at-boundary | ✅ via `ActionStore` internals |
| steer lane | immediate in-turn | native `steer` | native `steer` passthrough | native |
| per-item ids | ✅ | ✗ (`queueKey` coalescing only) | bridge assigns ids | synthesize from action ids |
| interrupt semantics | exact-session, best-effort abort handles | `abort` (exact-session) + fine-grained aborts | `abort` passthrough | native |
| modes | n/a | `set_steering_mode`/`set_follow_up_mode` (`one-at-a-time`/`all`), `interruptMode` in settings | same | same |

Double-queue hazard (B only): daemon also queues follow-ups in-memory. Mitigation: bridge *never* pre-loads the daemon queue with editable items; it delivers exactly one follow-up at turn boundary (`agent_end`/`waitForIdle`), keeping the daemon queue near-empty and the bridge store authoritative. Daemon restart then loses nothing user-visible.

### 6.3 Security: TLS, Origin, auth, process exposure

- **Today**: pi-agentd accepts exactly one canonical Origin, WSS-only remote / WS loopback-only, 8 MiB frames; single-tenant trust model.
- **A**: tunnel must re-implement Origin allowlist + TLS + (recommended) bearer token, since browser now speaks directly to daemon auth surface which assumes a trusted local client (unix socket peer creds are the whole trust model). Any daemon command becomes web-reachable — including `execute_bash`, `restart`, `shutdown`. **This is A's real cost: the daemon protocol is not designed to be internet-facing.** Mitigation: command allowlist in the tunnel.
- **B**: same exposure formally, but the façade already interprets every method, so allowlisting is inherent; pi-relay's existing acceptance policy ports verbatim. Bridge-side `workspace.*` must jail paths to the session cwd (pi-relay already has these semantics to copy).
- **D**: bridge is the only exposure; same policies as B.
- **Multi-profile**: unchanged in all — profile = URL; one bridge/tunnel per daemon host. Cloudflare Pages static app continues to hold no secrets; tokens (if added) live in `localStorage` per profile — same as today's implicit trust.

### 6.4 Versioning & drift

- pi-relay's contract is repo-local and versioned with the frontend. Prime-agent's daemon protocol is versioned (`DAEMON_PROTOCOL_VERSION=7`, `DAEMON_SCHEMA_REVISION=13`) with **hello-time capability negotiation** (client caps `attach_snapshot|event_sequence|extension_ui|slim_attach|chunked_snapshot|client_owned_sessions`; server caps add `session_input_admission`, `prompt_admission_cancellation`, `delete_rlm_subagent`, etc.).
- **A**: browser must negotiate caps per-connection; frontend becomes coupled to daemon revs.
- **B**: only the bridge negotiates (via SDK `DaemonClient`, which already handles hello/chunked snapshots); frontend pinned to pi-relay contract.
- **D**: no wire protocol to drift; SDK API drift instead (untyped semver risk, but compile-time visible in TS).

### 6.5 Failure modes & supervision

- **A**: tunnel dies → browser reconnects to daemon directly (stateless; fine). Daemon dies → all browser tabs fail until supervisor restart; sessions auto-resume on attach.
- **B**: bridge dies → replay spool persists; on restart, re-attach daemon, rebuild live state. Daemon dies → bridge surfaces per-session errors, re-attaches on supervisor return (new generation → synthetic resync). Worker crash → supervisor auto-restarts worker (daemon-supervised); bridge re-attaches that session only.
- **D**: bridge process crash = *all* sessions down, in-memory queues/actions lost unless the update-restart manifest machinery is replicated; kernel crashes are no longer isolated per worker. Highest blast radius.

### 6.6 Where each pi-relay-only semantic lands (recap)

| Semantic | A | B | D |
|---|---|---|---|
| durable event replay | dropped | bridge spool | tap+persist |
| queue edit/reorder/promote | dropped | bridge store | core internals |
| `turn.resume` | dropped (auto-resume) | synthesize or drop | buildable |
| client-spawned subagents | needs PA patch | PA patch or prompt-hack | buildable |
| `project.*`, `runtime.list` | dropped | bridge tables | bridge tables |
| `workspace.*` | needs companion svc | bridge fs/git | native fs |
| `mcp.*` mgmt | settings file + reload | settings file + reload (OAuth hardest) | `McpManager` direct |

## 7. Recommendation

### Primary: **Option B — stateful façade bridge speaking pi-relay's websocket-rpc contract, backed by the prime-agent daemon via the SDK's `DaemonClient`.**

Rationale, in order of weight:

1. **It is the only option that preserves the shipped frontend** — including the parts whose semantics have *no* prime-agent counterpart (durable `events.subscribe` replay, editable/reorderable durable queue, paged transcripts, `project.*`, `runtime.list`). Those semantics have to live *somewhere new* in every option; B puts them in exactly one new, replaceable service instead of scattering them across a browser rewrite (A), a forked runtime (D), or dropping user-facing features.
2. **It keeps prime-agent's operational core intact.** The daemon's supervisor/worker isolation, session leases, idle eviction, crash auto-resume, and update-restart manifest machinery are exactly the things option D would have to rebuild and the things that make the migration *safe* to run alongside existing prime-agent usage. B treats the daemon as a black box behind its published, versioned protocol (hello-negotiated, rev 7/schema 13, `DaemonClient` shipped in the SDK — `import { DaemonClient } from "prime-agent"`), so prime-agent upgrades are absorbed in one adapter.
3. **All risk is concentrated where it's cheapest**: a new TS service with a small local store. If B later proves too stateful, its daemon-facing half is precisely option A's server; if the frontend is later rewritten, the façade can be deleted. It is reversible in both directions.
4. The mapping table (§4) shows ~22 of 51 frontend-called methods are pure bridge-side translation/reshaping and ~10 more are direct daemon calls — i.e., the bulk of B is mechanical, not research.

**Concrete build sketch for B** (all new code, no repo changes to prime-agent; pi-relay repo gains one package):

- `packages/bridge/` Node 20+ TS service. Deps: `ws`, `prime-agent` (SDK for `DaemonClient` + protocol types), SQLite (`better-sqlite3`) or Postgres for the spool/queue/project tables, Chokidar for `workspace.watch`.
- Connection model: one WSS per browser profile (unchanged). Per active session: one daemon socket connection held `attach`ed; one supervisor-level connection for `list`/create/kill. Child sessions: one extra attach per expanded delegation row.
- State stores (bridge-local): `events(session_id, seq, kind, payload)` (spool, feeds `events.subscribe`), `queued_inputs` (durable editable queue; release-at-turn-boundary), `projects`, `sessions_meta` (pi-relay-shaped rows synthesized from `SessionSummary`+`get_state`), all namespaced by profile/daemon identity.
- Synthesis layer: daemon `AgentConnectionSessionEvent` → pi-relay event vocabulary (appendix §8.1 has the name mapping); `session.sync_active_branch` computed from `get_state().leafId` + `get_messages()` tail-diff; `transcript.*` paged over cached snapshot with turn-boundary synthesis; `workspace.*` fs/git against `get_state().cwd` with path jailing.
- Acceptance policy: copy pi-agentd's — exactly one canonical Origin, 8 MiB frames, WSS remote/WS loopback, optional per-profile token.

**Small prime-agent core patches that would make B fully feature-parity** (each is narrow and uses existing internals; none are blockers for the first cut):

1. `start_rlm_child {activeSessionId, task, name?, model?}` daemon command → admit into parent registry + `runRlmChild()` (machinery exists; covers `delegation.start_full`; fanout = loop).
2. Queue item identity: expose `queueKey`/action ids + text in `get_queue`, plus `remove_queued {queueKey}` and `reorder_queue {keys}` verbs over the existing `ActionStore.remove`/`enqueueFront`.
3. Optional: `resume_turn` verb wrapping the checkpoint re-drive internals (or accept auto-resume and drop `turn.resume` from the web UI).
4. Optional: extend daemon replay to a bounded ring buffer so `replay.status: "partial"` actually happens — would let B drop its spool for short reconnect windows.

### Fallback: **Option A — thin WS tunnel, browser speaks daemon protocol** (with a command allowlist and Origin/TLS policy in the tunnel).

Choose A instead if: the frontend is going to be substantially rewritten anyway (new protocol client, snapshot-authoritative state model), the durable-queue/editable-queue and paged-transcript features are acceptable to drop or defer, and minimizing new server-side state is the priority. A is also the natural **Phase 0** for B: the tunnel plus a browser-side spike validates daemon protocol behavior (attach/snapshot/resync, lease errors, eviction latency) against the real supervisor before the façade is built.

**Explicitly not recommended**: C (ACP) — session model too small (single session, `loadSession:false`, no tree/queue/subagent surface); E (pi-protocol) — experimental, 9 verbs, would require a from-scratch WS listener *and* service implementation *and* total frontend rewrite; F (HTTP+SSE) — orthogonal frontend-architecture change, combinable with B later; G (collab-style) — wrong authority model (host-TUI-centric, content-blind relay); D — viable end-state, wrong first step (rebuilds supervision/recovery; highest blast radius).

### Suggested phasing

- **Phase 0 (spike, days)**: stand up A's tunnel on a spare port; hand-drive `hello`/`create`/`attach`/`prompt`/`navigate_tree` from a browser console against a scratch daemon. Validates: attach snapshot size/chunking on real sessions, `session_already_active` behavior, eviction/rehydrate latency, event rate.
- **Phase 1 (façade core)**: B with session lifecycle, prompt/steer/follow_up/interrupt, event spool + `events.subscribe`, snapshot→`session.get`/`sync_active_branch`, `history.*`, compaction passthrough. Web app fully usable for root sessions.
- **Phase 2 (depth)**: durable queue editing, delegation list/steer-via-attach/read-handoff, `workspace.*`, `project.*`, `runtime.list`, MCP inventory (read-only), `turn.resume` decision.
- **Phase 3 (parity patches)**: prime-agent core patches 1–3 above; MCP login flows; then (optionally) revisit D as end-state or F for CDN-friendliness.

### Migration-era note (per repo AGENTS.md)

Old pi-relay sessions live in Postgres; prime-agent sessions are JSONL files. A one-shot migration script (run once, then delete) should: for each pi-relay session row, emit a prime-agent v3 JSONL (header + message/model_change/compaction/branch_summary entries reconstructed from transcript rows + queued_inputs→bridge store + a `custom` entry preserving the pi-relay session id for spool continuity). Entry-id mapping (pi-relay entry ids → JSONL entry ids) must be recorded in the bridge's spool so `history.*` targets remain valid.

## 8. Appendix

### 8.1 Event vocabulary mapping (pi-relay 34-event set → prime-agent daemon)

pi-relay events are *invalidation hints* carrying revision counters (`session_revision`, `queue_revision`, `transcript_revision`); the web client's `sessionEvents.ts` maps each to a refetch plan. Prime-agent's per-session `session_event` payloads are `AgentConnectionSessionEvent` = 10 core `AgentEvent` types (`agent_start/end`, `turn_start/end`, `message_start/update/end`, `tool_execution_start/update/end`) + 18 session-level types (`compaction_start/end`, `session_action_update`, `session_info_changed`, `thinking_level_changed`, `service_tier_changed`, `auto_retry_start/end`, `auth_stale`, `rlm_child_update`, `recap_update`, `goal_update`, `bash_start/output/end`, `refine_complete/failed`, `ipython_sent_agent_message`), plus supervisor-level `session_replaced`/`session_resynced`/`session_attached` frames.

| pi-relay event | prime-agent source | bridge work |
|---|---|---|
| `session.created` | `create` response (+`list` diff) | synthesize |
| `session.configured` | `set_*` responses, `session_info_changed`, `thinking_level_changed`, `service_tier_changed` | translate |
| `input.accepted` / `input.queued` | `prompt`/`follow_up`/`steer` responses; `session_action_update` (`SessionActionSnapshot.queuedCount`, lists) | synthesize/translate |
| `input.consumed` | `session_action_update` (queuedCount drop) / `turn_start` | infer |
| `input.promoted` / `input.updated` / `input.cancelled` / `input.reordered` | none (queue not editable) | bridge store emits |
| `input.ignored` | `prompt`-while-idle edge / admission results | synthesize |
| `transcript.appended` (carries entry body + tree_node + leaf) | `message_end` (+`get_messages` tail) | synthesize from snapshot diff |
| `turn.started` / `turn.finished` | `turn_start` / `turn_end` | direct |
| `assistant.message` | `message_update`/`message_end` | translate |
| `action.requested` | `session_action_update` (active phase) | translate |
| `model.requested` / `model.completed` / `model.error` | inside `message_*` lifecycle / `turn_end` stopReason / `auto_retry_*` | infer/translate |
| `tool.requested` / `tool.started` | `tool_execution_start` | translate (two pi-relay events from one) |
| `tool.completed` / `tool.error` | `tool_execution_end` (status) | translate |
| `compaction.requested` | `compact` response | synthesize |
| `compaction.completed` / `compaction.error` | `compaction_end` (reason) / failure path | translate |
| `history.switched` (carries leaf + revisions) | `navigate_tree` response / `session_resynced` | synthesize |
| `history.compacted` | `compaction_end` | translate |
| `session.work_cancelled` | `abort` response + `turn_end` (aborted) | synthesize |
| `session.recovered` | attach replay/`session_resynced` | synthesize |
| `session.idle` | `agent_end` (isTerminal) / `session_action_update` (queuedCount=0, no active) | infer |
| `subagent.spawned` / `subagent.running` / `subagent.idle` | `rlm_child_update` (status field) | direct |
| `workspace.fs_changed` (ephemeral, event_id 0) | none | bridge fs watcher |
| `mcp.tools_added` | none | bridge/settings watcher |

Net: ~12 direct/near-direct translations, ~14 bridge-synthesized (mostly from `session_action_update` + snapshot diffs + bridge-owned store transitions), ~8 pure bridge emissions (queue-edit, workspace, mcp). Nothing in the pi-relay set is unproducible.

### 8.2 Daemon protocol negotiation cheat-sheet (for the bridge)

- Connect to `defaultDaemonSocketPath()`; server greets with `daemon_hello` (protocol `prime-agent-daemon`, version 7, schema revision 13, server capabilities).
- Client should declare caps: `attach_snapshot`, `event_sequence`, `chunked_snapshot`, `slim_attach` (reduces attach payload), `client_owned_sessions` only if owning lifecycle. Server caps observed: `delete_rlm_subagent`, `heartbeat_catalog`, `heartbeat_management`, `model_catalog`, `side_question_transcript`, `transient_bash`, `session_input_admission`, `prompt_admission_cancellation`.
- `attach {activeSessionId}` → `session_attached` + `DaemonSessionSnapshot {summary, state, messages, sessionTree, sessionContext?, children, lastEventSequence/Cursor}`; large snapshots arrive as `session_snapshot_begin`/`chunk`/`end` (reassemble by stream id) or `session_snapshot_failed`.
- Every `session_event` carries `{id: "<activeSessionId>:<seq>", cursor: {generation, sequence}, emittedAt, event}`; track per-session `sequence` gaps; on any gap or generation change → re-attach (replay is `unavailable` by design).
- Commands are `{id, type, ...}` → `{id, type:"response", command, success, data|error}`; events/progress frames interleave freely; ids are client-chosen strings.
- Lease errors to handle: `session_already_active` (concurrent open), plus eviction/rehydrate latency on `attach` of idle sessions.

### 8.3 Source files consulted (all reads static; no live processes touched)

**pi-relay**: `rust/docs/websocket-rpc.md` (full contract), `packages/web/src/{rpc.ts,serverProfiles.ts,agentApi.ts,sessionEvents.ts,connectionRecovery.tsx,App.tsx}`.
**prime-agent (installed dist + repo clone)**: `.pi/prime-agent-architecture.md`; `dist/modes/daemon/daemon-protocol.{js,d.ts}` (96-command union, replay/cursor fns), `daemon-mode.js` (handlers incl. `get_queue`), `daemon-supervisor.js` (`node:net` unix server only); `dist/modes/agent-connection/types.d.ts` (`AgentConnectionState`, `…SessionEvent`, tree/child snapshot shapes); `dist/core/agent-session.d.ts` (navigateTree/steer/followUp/queue APIs), `session-action-store.d.ts` (ActionStore lifecycle), `settings-manager.d.ts` (MCP config), `core/compaction/*`; `dist/modes/rpc/rpc-mode.js`, `dist/modes/acp/acp-mode.js`; SDK exports `dist/index.d.ts` + repo `packages/coding-agent/src/index.ts` (`DaemonClient` export confirmed).
**pi-mono**: `packages/{protocol,client,server}/README.md` + `protocol/src/schemas.ts`, `server/src/types.ts`, `coding-agent/src/{client/remote-session.ts,server/create-harness.ts,agent/harness/agent-harness.ts}`.
**oh-my-pi**: `docs/{rpc.md,sdk.md,collab.md,agent-hub.md}`, `packages/coding-agent/src/collab/*`, `packages/collab-web/README.md`.
