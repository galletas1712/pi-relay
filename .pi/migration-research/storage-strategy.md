# pi-relay → prime-agent: Session Storage & Persistence Strategy

**Author:** storage-strategy (subagent) · **Date:** 2026-08-08 · **Status:** COMPLETE

> Research + decision record for how sessions should be stored after migrating pi-relay onto a prime-agent core.
> Hard requirement (user): old sessions must keep working via **one-shot migration scripts**, NOT backwards-compat code paths.
> All claims cite concrete file paths. Sources read: pi-relay `rust/agent-store`, `agent-session`, `agent-runtime`, `runtime-protocol`, docs; prime-agent repo (`packages/coding-agent`); pi-mono repo (`packages/agent/src/harness/session`, `packages/session-backends/sqlite-node`, `packages/coding-agent`); oh-my-pi repo (`packages/coding-agent/src/session`, `docs/*`).

## Contents
1. [TL;DR & Recommendation](#1-tldr--recommendation)
2. [pi-relay Postgres schema (full reconstruction + diagram)](#2-pi-relay-postgres-schema)
3. [prime-agent session format (precise)](#3-prime-agent-session-format)
4. [pi-mono session model (v4 seam, JSONL-v4, SQLite backend)](#4-pi-mono-session-model)
5. [oh-my-pi (OMP) session model — the most-evolved reference](#5-oh-my-pi-omp-session-model)
6. [Semantic mapping table (pi-relay → prime-agent/pi-mono)](#6-semantic-mapping-table)
7. [Options analysis (A / B / C / D)](#7-options-analysis)
8. [One-shot migration script plan](#8-one-shot-migration-script-plan)
9. [Workspaces + btrfs snapshot metadata (today & post-migration)](#9-workspaces--btrfs-snapshot-metadata)
10. [Open questions & risks](#10-open-questions--risks)

---

## 1. TL;DR & Recommendation

pi-relay's durable state splits into two concern domains that point at **different** target stores:

- **The conversation graph** (messages, entry tree, compaction, fork) — maps cleanly onto *every* candidate target (prime-agent v3 JSONL, pi-mono v4 seam, OMP). Low-risk to migrate.
- **The operational control plane** (durable queue, committed-before-dispatch actions, delegation ledger, replay cursor, projects/runtimes) — has a first-class counterpart **only in the pi-mono v4 seam** (durable `records` + `lanes` + `findOpenOperations` recovery + writer-lease fencing). prime-agent keeps all of this **in memory**; OMP approximates it with `custom` JSONL entries. This is pi-relay's raison d'être ("Postgres Is Authoritative").

**Recommendation: Option C (hybrid) — write transcripts as pi-mono v4 JSONL (via the seam), keep the control plane in Postgres — with Option B1 (full relational session backend on the v4 seam) as the natural end-state.** It is the lowest-risk path that does **not** abandon pi-relay's durability guarantees, satisfies the "one-shot migrator, no backwards-compat" requirement cleanly, and keeps B1 as an *incremental upgrade* rather than a rewrite. **Option A** (prime-agent v3 JSONL as-is) is fastest but silently drops crash-resume + durable-queue, and locks to an upstream format the user doesn't control — likely too lossy given pi-relay's design center. **Avoid Option D** (deep permanent fork of prime-agent's session layer).

**Deciding question for the user:** does the post-migration system still need pi-relay's durable/resumable queue + delegation control plane? If yes → **C (→B1)**; if no → **A**.

**Workspaces/btrfs (Q#6):** unaffected. Session cwds are btrfs subvolumes under `<workspace_root>/sessions/<workspace_id>/cwd`, owned by `WorkspaceManager` on the runtime host, **independent of Postgres**. The parent→child snapshot lineage is a **filesystem-level fact in no DB table**. Only the *metadata* (`sessions.workspaces` jsonb, `workspace_id`, `parent_session_id`) migrates; the subvolumes stay put.

---

---

## 2. pi-relay Postgres schema

Source of truth: `rust/agent-store/src/postgres/schema.rs`. **There is no sequential migration framework** (`rust/migrations/` holds only a README + one deployment artifact `single-delegation-wakeup.sql`). The schema is **idempotent DDL (`CREATE TABLE IF NOT EXISTS` / `ALTER ... IF NOT EXISTS`) executed via `sqlx::raw_sql` at daemon startup** — every boot re-asserts the full current shape. Implication for migration: the "current schema" is whatever `schema.rs` last asserted; there is no version table to consult, so the migrator must introspect the live DB.

### 2.1 Reconstructed DDL (from `schema.rs`)

```sql
-- Tenancy / routing
CREATE TABLE IF NOT EXISTS projects (
  id            text PRIMARY KEY,
  -- display name, workspace definitions, etc. (columns omitted where not load-bearing)
  ...
);
CREATE TABLE IF NOT EXISTS runtimes (
  id            text PRIMARY KEY,
  ...
);

CREATE TABLE IF NOT EXISTS sessions (
  id                         text PRIMARY KEY,
  project_id                 text NOT NULL REFERENCES projects(id),
  runtime_id                 text NOT NULL REFERENCES runtimes(id),
  workspace_id               text NOT NULL,               -- on-disk dir name under <state_root>/sessions/ (docs called this outer_cwd)
  active_leaf_id             text,                        -- EXPLICIT active-branch tip (contrast prime-agent: derived from append order)
  system_prompt              text,
  provider_config            jsonb,                       -- model/provider/route snapshot
  workspaces                 jsonb,                       -- Vec<SessionWorkspace>: kind,workspace_dir,remote_url,remote_branch,source_path,base_sha,local_branch
  metadata                   jsonb,                       -- BTreeMap; carries delegation_spawn_index etc.
  parent_session_id          text REFERENCES sessions(id),  -- fork lineage
  subagent_type              text,
  delegation_id              text,                        -- circular FK -> delegations
  mcp_manifest_fingerprint   text,
  -- optimistic-concurrency revisions (three independent):
  session_revision           bigint NOT NULL,
  queue_revision             bigint NOT NULL,
  transcript_revision        bigint NOT NULL,
  created_at / updated_at    ...
);

CREATE TABLE IF NOT EXISTS daemon_config ( ... );

-- Append-only transcript forest (the conversation graph)
CREATE TABLE IF NOT EXISTS transcript_entries (
  session_id      text NOT NULL REFERENCES sessions(id),
  id              text NOT NULL,
  parent_id       text,                 -- tree link; NULL = root
  timestamp_ms    bigint NOT NULL,
  sequence        bigserial,            -- global append order (replay ordering)
  item            jsonb NOT NULL,       -- TranscriptItem (tagged union, see 2.3)
  provider_replay jsonb,                -- provider-native replay payload
  turn_id         text,
  PRIMARY KEY (session_id, id)
);
-- Active branch = recursive CTE from sessions.active_leaf_id walking parent_id,
-- EXCEPT compaction_summary entries traverse via item->>'source_leaf_id' (compaction lineage pointer).

-- Durable input queue with idempotency + optimistic concurrency
CREATE TABLE IF NOT EXISTS queued_inputs (
  id                text PRIMARY KEY,
  session_id        text NOT NULL REFERENCES sessions(id),
  client_input_id   text,               -- idempotency key
  priority          text NOT NULL,      -- 'steer' | 'follow_up'
  status            text NOT NULL,      -- queued/consuming/consumed/...
  content           jsonb NOT NULL,     -- message OR {type:'subagent_control',...} (control ledger)
  origin            jsonb,              -- carries claim_id, control_kind, control_phase
  provider_config   jsonb,              -- snapshot at enqueue time
  -- row_version via xmin::text; claim_id via origin->>'claim_id'
  UNIQUE (session_id, client_input_id)
);

-- Durable work records (committed-before-dispatch unit)
CREATE TABLE IF NOT EXISTS actions (
  id              text PRIMARY KEY,
  session_id      text NOT NULL REFERENCES sessions(id),
  kind            text NOT NULL,        -- model action, control action, compaction, ...
  status          text NOT NULL,        -- pending|blocked|running|completed|error|interrupted|stale
  payload         jsonb,                -- may carry post_compaction_dispatch = {kind:'resume_model_v1', lease:{owner_id,generation,expires_at_ms}}
  result          jsonb,
  attempt_id      text,
  provider_config jsonb,
  ...
);

CREATE TABLE IF NOT EXISTS mcp_session_manifests (
  fingerprint     text PRIMARY KEY,     -- content-addressed
  manifest        jsonb NOT NULL
);

-- Transient reconnect buffer (replay cursor source); cleared on idle
CREATE TABLE IF NOT EXISTS events (
  id              bigserial PRIMARY KEY,  -- == replay cursor for events.subscribe(after_event_id)
  session_id      text,
  payload         jsonb,
  ...
);

-- Delegation (multi-agent fan-out) with idempotent launch
CREATE TABLE IF NOT EXISTS delegations (
  id                  text PRIMARY KEY,
  parent_session_id   text NOT NULL REFERENCES sessions(id),
  workflow            text,
  label               text,
  kind                text NOT NULL,      -- 'Full' | 'ReadonlyFanout'
  status              text NOT NULL,      -- Running|Cancelling|Done|DoneWithFailures|Cancelled|Failed
  attempt_id          text,
  launch_shape        jsonb,
  teardown_target     jsonb,
  expected_subagents  integer,
  UNIQUE (parent_session_id, launch_key)  -- idempotent launch guard; only ONE running Full delegation per parent
);
-- circular FK: sessions.delegation_id <-> delegations.id
```

### 2.2 Schema diagram

```
projects 1───* sessions *───1 runtimes
                │  │  │  │
   parent_session_id┘  │  │  └─delegation_id ──► delegations (circular)
   (self-FK fork)      │  │                      ▲
                │  │  └──────────────────────────┘ (child sessions carry delegation_id)
                │  └─ active_leaf_id ──► transcript_entries (forest root walk)
                │
   ┌────────────┼─────────────┬───────────────┬──────────────┐
   ▼            ▼             ▼               ▼              ▼
transcript_  queued_       actions         events        mcp_session_
entries      inputs        (work ledger)   (replay buf)  manifests
(forest,     (idempotent   (committed-     (id bigserial
 active-     queue,        before-          = cursor)
 branch CTE) steer/follow)  dispatch)
```

### 2.3 Transcript item taxonomy & compaction lineage

`TranscriptItem` (serde `tag="type"`, snake_case) variants:
`TurnStarted` · `UserMessage` · `AssistantMessage` · `ToolCallStarted` · `ToolResult` · `TurnFinished{turn_id, outcome}` · `CompactionSummary` · `DaemonToolObservation`.
`TurnOutcome` = `Graceful | Interrupted | Crashed`.

**Compaction = a typed root, not a replacement transcript.** `CompactionSummary{source_session_id, source_leaf_id, summary, tokens_before, last_turn_id, turn_started_at_ms}`. `complete_compaction_action` inserts a **NEW ROOT** (`parent_id = NULL`) that points *back* at the pre-compaction leaf via `source_leaf_id` (and across sessions via `source_session_id`), appends continuation-suffix entries, and sets `sessions.active_leaf_id` = suffix tip. Active-branch traversal follows `parent_id` normally but follows `item->>'source_leaf_id'` for compaction rows. `CompactionScope = Boundary | MidTurn` (MidTurn blocks a model action and resumes it after compaction). Cycle detection guards `ancestry_invalid`.

### 2.4 Recovery invariant (committed-before-dispatch)

`persist_outputs` (`agent-store/.../outputs.rs`) writes transcript entries + active_leaf + queue consume/accept + action rows + completions + control-interrupt phase + all revision bumps + events **in ONE Postgres transaction**, and only returns the `PersistedAction` list to dispatch **after commit**. On boot, `mark_all_unfinished_actions_stale()` sets `status='stale'` **except**: (a) post-compaction-dispatch model actions, (b) pending actions in running delegations, (c) `pending_interrupt` scoped-subagent controls in running delegations. Post-compaction model actions survive crash via `payload.post_compaction_dispatch = {kind:'resume_model_v1', lease:{owner_id, generation, expires_at_ms}}` with **30s lease fencing**. Queue optimistic concurrency = `xmin::text` row_version + `origin->>'claim_id'`.

Design commitments (`rust/docs/design-decisions.md`): "Postgres Is Authoritative"; "committed-before-dispatch; on commit failure after live session advanced, evict live session"; "Idle Input Skips The Queue, Busy Input Stays Durable"; peek-then-consume with row-version validation. **No repository trait exists yet** (deliberate — would force the Postgres model through an imagined abstraction).

### 2.5 Delegations, control ledger, fork, config

`Delegation{...}` above. `DelegationLaunchGuard` = a **held-open PG transaction** that gates external child launch. Child sessions carry `delegation_id` + `metadata.delegation_spawn_index`. **Child-control ledger**: scoped subagent steer/interrupt are `queued_inputs` rows with `content.type='subagent_control'` (excluded from normal claim SQL) whose `origin` carries `control_kind`/`control_phase` (`pending_interrupt→interrupt_applied→ready|cancelled`). Terminal wakeup = a single `delegation-steer:{delegation_id}:{attempt_id}` queued input.

**Fork (`history_fork.rs`) = deep copy**: idle-only; INSERT a new session copying config + `active_leaf_id`, then **deep-copy all transcript_entries to the new session_id** (same entry ids). Distinct from prime-agent fork (new file) and pi-relay's *workspace* fork (btrfs snapshot, §9).

**Config on create_session**: `system_prompt`, `provider_config`, `workspaces`, `metadata`, `mcp_manifest_fingerprint`; MCP manifests content-addressed in `mcp_session_manifests`.

### 2.6 Websocket replay / recovery contract

`rust/docs/websocket-rpc.md`: **`events.subscribe(after_event_id)`** = reconnect replay from cursor, paginated 500/page with `has_more`/`next_after_event_id`. The `events` table is a **transient reconnect buffer cleared on idle**; durable state = sessions/transcript/queue/actions. Daemon-death recovery: unfinished actions→stale, crashed turn tail appended, `turn.finished`/`session.recovered` replayable, no explicit resume RPC needed. `turn.resume` restarts a crashed/interrupted terminal turn from the model-action checkpoint.

> **Doc/schema drift note:** `websocket-rpc.md` says `sessions.outer_cwd text not null`, but `schema.rs` has `workspace_id text not null`. The column is `workspace_id` (the on-disk session dir name under `<state_root>/sessions/`). See §9.

---

## 3. prime-agent session format

Sources: `repos/prime-agent/packages/coding-agent/src/core/session-manager.ts` (2324 ln), `session-lease.ts`, `session-action-store.ts`, `session-file-actions.ts`, `config.ts`; `dist/core/kernel/*`; `.pi/prime-agent-architecture.md` §1–3, §7, §8. **prime-agent's coding-agent is literally the published package `@earendil-works/pi-coding-agent@^0.7.1`** (its `package.json`), i.e. prime-agent is a downstream distribution of pi-mono's coding-agent plus Prime-specific daemon/RLM/harness layers.

### 3.1 Storage substrate: file-based, no DB
Per-session JSONL at `getSessionsDir() = ~/.prime/agent/sessions/<uuid-v7>.jsonl` (`config.ts:648`). **No SQLite/Postgres.** `CURRENT_SESSION_VERSION = 3`. On-load migrations v1→v2 (add `id`/`parentId`) and v2→v3 (`hookMessage`→`custom` role) mutate in memory; the next write rewrites the file.

### 3.2 Header (first line)
`{type:'session', version, id, timestamp, cwd, parentSession?, rlmDepth?, git?}`. `parentSession` = **source file PATH** for forks; `rlmDepth` = RLM spawn depth; `git` = GitContext snapshot.

### 3.3 Entry taxonomy (SessionEntry union, 14 types)
Every entry = `{type, id(8-hex), parentId, timestamp}`:
| type | payload | in LLM ctx? |
|---|---|---|
| `message` | `message: AgentMessage` (user/assistant/toolResult/custom/bash) | yes |
| `thinking_level_change` | `thinkingLevel` | state |
| `service_tier_change` | `serviceTier` | state |
| `model_change` | `provider, modelId` | state |
| `compaction` | `summary, firstKeptEntryId, tokensBefore, details?, fromHook?, customInstructions?` | summary |
| `branch_summary` | `fromId, summary, details?, fromHook?` | summary |
| `custom` | `customType, data?` | **no** (extension state) |
| `custom_message` | `customType, content, details?, display` | yes |
| `child_usage_attributed` | `targetId, childUsage, aggregateUsage, origin?` | no |
| `label` | `targetId, label` | no |
| `session_info` | `name?` | no |
| `session_state` | `state.status: active\|archived\|crash` | no |
| `agent_status` | `status:{summary, taskState, basedOnMessageCount}` | no |
| `git_state` | `git: GitContext` | no |

### 3.4 Tree & leaf
`id`/`parentId` forest in **one file**. `leafId` is **in-memory only**; on load `_buildIndex()` sets `leafId = last appended entry`. **Active leaf is derived from append order, NOT persisted** (contrast pi-relay's explicit `active_leaf_id`). `branch(branchFromId)` moves `leafId`; the next append becomes a child of that entry (in-place branch, same file). `resetLeaf()` → next entry is a new root (`parentId=null`). `branchWithSummary()` also appends a `branch_summary`. `createBranchedSession`/`forkFrom`/`clone` write a **new file** with `header.parentSession = source path`.

### 3.5 Compaction = inline on the same branch
`buildSessionContext()` walks leaf→root via `parentId`, reverses; extracts latest thinkingLevel/serviceTier/model (model from `model_change` OR the last assistant message's provider/model). If a `compaction` entry is on the path: emit the compaction-summary message **first**, then retained messages from `firstKeptEntryId` up to the compaction entry, then post-compaction messages. **No new root / no cross-session lineage pointer** — full history stays in the file; compaction is lossy for context only. (Contrast pi-relay's new-root + `source_leaf_id`.)

### 3.6 Persistence mechanics (`_persist`/`_appendEntry`/`_rewriteFile`)
- `_appendEntry`: push `fileEntries`, index `byId`, advance `leafId`, call `_persist`.
- `_persist`: **(a) no-assistant guard** — if no assistant message exists yet AND the entry is not `session_state`/`session_info`, skip the write and set `flushed=false` (draft sessions stay off disk until an assistant arrives; `flushNow()` forces pre-model entries durable). **(b)** if `!flushed` OR file missing → `_rewriteFile()` = **full atomic rewrite** (temp in same dir + `renameSync`, preserving mode/uid/gid). **(c)** else `appendFileSync(single line)` = incremental append-only steady state. **No fsync.** A crash mid-append leaves a partial last line; readers tolerate via per-line try/catch.

### 3.7 Concurrency: session lease
`session-lease.ts`: `proper-lockfile`. Lease dir `~/.prime/agent/session-leases/<sha256(canonical-path)>.lock/` + `owner.json {version, token, pid, processStartId, activeSessionId, sessionPath, createdAt}`. Stale leases reclaimed when the owning pid is dead. Conflict → `SessionAlreadyActiveError` (`code='session_already_active'`).

### 3.8 Actions/queue = IN-MEMORY ONLY
`session-action-store.ts` is an **in-memory FSM** (no disk writes): `SessionAction{payload: turn|session_command, lifecycle: queued|selected|preparing|committing|running|completed|failed|cancelled, delivery, wake, queueKey}`; `DeliveryRecord.durable` refers to the *message* being durable in the transcript, not an action table. **prime-agent has NO durable queue/action/delegation/event table** — steering, follow-ups, queued inputs are volatile per worker and lost on daemon restart. This is the single biggest semantic gap vs pi-relay.

### 3.9 Artifacts & kernel snapshots
`~/.prime/agent/session-artifacts/<session-id>/` holds: `harness/` (local `harness_state.json`), `scheduled-jobs.json` (per-session cron), **kernel namespace snapshots** (dill, `state-snapshot.js`; debounced ~1500 ms after execution + on shutdown; revived on resume), and **RLM child subdirs `sub-xxxxxxxx/`** (each holds the child's own `.jsonl` + nested `sub-*/`). `deleteSessionFile()` trashes the `.jsonl` and removes the artifact dir. Continual harness (prompt/memory/skill/subagent) is **file-based JSON** at `~/.prime/agent/harness/harness_state.json` (global) and `<artifact-dir>/harness/harness_state.json` (local).

---

## 4. pi-mono session model

pi-mono ships **two** session stacks. This is the central strategic fact.

### 4.A Shipping coding-agent = the SAME v3 monolithic model as prime-agent
`repos/pi-mono/packages/coding-agent/src/core/session-manager.ts` — `CURRENT_SESSION_VERSION = 3`, `firstKeptEntryId`, `hookMessage→custom` migration, direct `fs.appendFileSync/writeFileSync`. Identical lineage to prime-agent §3. **No storage seam** (fs hardcoded). This is what ships today.

### 4.B The next-gen backend seam (packages/agent harness/session) — the strategic target
`repos/pi-mono/packages/agent/src/harness/session/types.ts` (393 ln) defines a **pluggable storage interface** far closer to pi-relay than prime-agent's JSONL.

**Entry base** = `{type, id, seq(storage-assigned shared sequence), parentId(=appending lane's leaf), timestamp(ms)}`.
**Entry union (7):** `message{message,terminate?}`, `model_change{provider,modelId}`, `thinking_level_change{thinkingLevel}`, `active_tools_change{activeToolNames[]}`, `compaction{summary, retainedTail: AgentMessage[] (INLINE kept messages, NOT a pointer), tokensBefore, details?, usage?}`, `branch_summary{fromId,summary,details?,usage?}`, `custom{customType,data?}`.

**LANES** — named branch pointers (like git refs). `SessionStorage`: `getLanes()→[{lane,leafId}]`, `createLane(lane,at)`, `moveLane(lane,to)`, `appendEntry(entry,lane)` (lane.leafId becomes parentId), `appendRecord(record)`. The active branch is a lane (conventionally `"main"`). **Generalizes pi-relay's single `active_leaf_id`.**

**RECORDS (`LaneRecord`)** — a **durable append-only operational log**, separate from entries (this is exactly what prime-agent lacks):
- `operation_started{sourceLeafId, intent:{kind:run{originalPrompt,initialMessages,systemPromptOverride,resumeData} | compaction{resultEntryId,customInstructions} | navigation{targetId,summarize,...}}}`
- `abort_requested{runId}`; `operation_finished{runId, outcome:completed|aborted|failed|declined, error?}`
- `step_attempt{runId, step:assistant|branch_summary|compaction, attempt, resultEntryId, compactionReason?}`
- `tool_started{runId, assistantEntryId, toolIndex, toolCallId, toolName, effectiveArgs, resultEntryId, replay:never|safe}`
- `queue_enqueued{queue:steer|followUp|nextRun, runId, target}`; `queue_cancelled{runId?,entryId}`
- `write_deferred{runId,target}`; `usage{usage, cause:assistant|compaction|branch_summary|deferred_fetch|tool|hook|adjustment, ...}`

**Recovery:** `findOpenOperations(lane,{limit:2})` → 0=idle, 1=suspended(resume), 2=corruption. **Semantically = pi-relay's committed-before-dispatch + mark-stale**, expressed as a query over the record log.
**Facts:** `getName/setName` (global latest-wins, not branch-scoped), `getLabel/setLabel(targetId)`.
**Log:** `getLog(afterSeq)→LogItem[]{entry|record|lane|fact, seq}` = a **replay cursor** (≡ pi-relay `events.id` cursor).
**Seam:** `SessionRepo{create/open(acquires writer claim)/list(no claim)/delete/fork}`; `ForkOptions={scope:branch,entryId,position:before|at}|{scope:tree}`. `SessionError` codes: `not_found|already_exists|invalid_entry|invalid_payload|invalid_lane|invalid_query|invalid_fork_target|storage`.
**Conformance suite:** `testing/conformance.ts` — a shared behavioral contract every backend must pass.

### 4.C pi-mono JSONL-v4 backend (jsonl/)
`jsonl/codec.ts`, `jsonl/storage.ts`. **One file per session, but lines are interleaved "mutations":** `{kind:'header',version:4,id,createdAt,cwd,parentSessionId?,legacyParentSessionPath?,metadata?}` then `{kind:'entry',lane,seq,timestamp,...}`, `{kind:'record',seq,...}`, `{kind:'lane',seq,lane,leafId}`, `{kind:'fact',seq,fact:name|label,...}` — all sharing one `seq`. Append = single-line `appendFile`. **Torn-tail repair** = truncate the partial last line by atomically publishing the valid prefix. Fork = `createForkMutations()` replayed into the target file. Reads both v3 (`sourceFormat:3`, parent as path → `legacyParentSessionPath`) and v4. **So pi-mono's seam can INGEST prime-agent v3 files.**

### 4.D pi-mono SQLite backend (session-backends/sqlite-node) — reference relational impl
`sqlite-node/src/sqlite/migrations/001_initial.sql` (sequential migrations, `migrations` table; all tables `WITHOUT ROWID`):

| table | key columns | notes |
|---|---|---|
| `sessions` | `id PK, created_at, cwd, parent_session_id NULL, metadata json` | + idx `(cwd, created_at)` |
| `entries` | `session_id, seq, id, parent_id NULL, type, timestamp, payload json; PK(session_id,id); UNIQUE(session_id,seq)` | append-only forest |
| `session_sequences` | `session_id PK, next_seq` | per-session seq allocator |
| `session_stats` | `session_id PK, message_count, cached/uncached/total_tokens, cost_total` | |
| `branch_entries` | `session_id, branch_id, entry_id, entry_seq, entry_type, custom_type` | **DERIVED** branch cache; `entries.parent_id` is canonical |
| `lanes` | `session_id, lane, leaf_id NULL, open_operation_id NULL; PK(session_id,lane)` | lanes + cached open op |
| `records` | `session_id, seq, id, lane, run_id NULL, type, op_kind NULL, timestamp, payload json; PK(session_id,id); UNIQUE(session_id,seq)` | durable op log, 6 idx |
| `lane_moves` | `session_id, seq, lane, leaf_id NULL` | lane-move audit |
| `facts` | `session_id, seq, kind, key NULL, value NULL` | name/label facts |
| `branch_tips` | `session_id, branch_id, tip_id` | |
| `writer_leases` | `session_id PK, owner_id, fence INTEGER, expires_at_ms` | **per-session writer claim with fencing token** |

`appendEntry` (repo.ts): read lane head as parentId → allocate seq → insert entry row → set lane leaf → append to branch cache → bump message count → advance seq — **all in one transaction** (single-writer, fenced). `appendRecord` validates the lane, maintains `lanes.open_operation_id` on operation_started/finished, folds usage into stats. `writer_leases` acquire = `INSERT…ON CONFLICT…WHERE expired`, increments `fence` on takeover; renew checks owner+fence+not-expired (≡ pi-relay's `resume_model_v1` lease). `fork(scope:tree)` deep-copies all entries/lanes/branch-tips/name/label facts into a new session with `parent_session_id=source.id`, fresh `seq` from 1, rebuilds branch caches; `fork(scope:branch)` copies only the main-lane path up to a message target.

---

## 5. oh-my-pi (OMP) session model — the most-evolved reference

OMP is the user's own coding-agent (`repos/oh-my-pi`). Its `docs/session.md` is the source of truth; the model is a **superset of prime-agent v3** with an explicit `SessionStorage` seam that already has a **remote (Redis/SQL) backend**. This is the strongest in-repo evidence for Option B.

### 5.1 Format & layout
`~/.omp/agent/sessions/<scope>-<basename>-<sha256(canonical-cwd)>/<timestamp>_<sessionId>.jsonl` (scope=home|tmp|abs). Physical first 256 bytes = fixed-width **`title` slot**, then header, then entries. `CURRENT_SESSION_VERSION=3`. Blobs at `~/.omp/agent/blobs/<sha256>`; terminal breadcrumbs at `~/.omp/agent/terminal-sessions/<id>`; prompt history in a separate SQLite `~/.omp/agent/history.db` (+FTS5).
Header: `{type:session, version, id, timestamp, cwd, title, titleSource, additionalDirectories[], previousSessionFiles[], providerPromptCacheKey, parentSession (opaque lineage string, NOT a typed FK)}`.

### 5.2 Entry taxonomy (15 types — superset)
`message`, `thinking_level_change`, `model_change{model,role}`, `service_tier_change{serviceTier: per-family map|null}`, `compaction{summary, shortSummary, firstKeptEntryId, tokensBefore, details, preserveData, fromExtension}`, `branch_summary{fromId,summary,details,fromExtension}`, **`reset_boundary`** (`/clear` marker), `custom{customType,data}` (non-LLM, replayable), `custom_message{customType,content,display,details,attribution}` (in-LLM), `label{targetId,label}`, `title_change`, `ttsr_injection{injectedRules}`, `credential_pin{provider+sha256 acct hash}`, `session_init{systemPrompt,task,tools,outputSchema,spawns,...}`, `mode_change{mode,data}`.

**Crucially**, OMP uses reserved `custom` customTypes as a **durable operational/recovery log**: `tool_execution_start`, **`session_exit{reason, kind, pendingToolCalls[]}`** (on resume, a valid latest `session_exit` after a non-terminal tail causes the loader to append a synthetic assistant `stopReason:'aborted'` — OMP's crash-recovery mechanism), `user_todo_edit`, `vibe-session-lifecycle` (child spawn/turn/tombstone recovery), `autoresearch-control`. **OMP's `custom` entries partially fill prime-agent's durability gap without a separate records table.**

### 5.3 Tree, compaction, persistence
Append-only + mutable in-memory `leafId` (load fallback = last entry). `buildSessionContext`: latest `reset_boundary` hides everything before it; else latest compaction (summary first + `firstKeptEntryId..compaction` + post); strips dangling tool calls and unsafe aborted/error turns. Persistence: **no fsync** (crash-safe, not power-loss); lazy creation gate (memory-only until first assistant msg or `ensureOnDisk()`); incremental append; concurrent appends supersede an in-flight rewrite with an authoritative full-body rewrite; atomic rewrite = `writeTextAtomic` + commit guard (stage+rename, EPERM move-aside fallback); persistence errors latched + rethrown; `SessionPersistenceIndeterminateError` fails closed.

### 5.4 Blob externalization & session artifacts
Strings >500k truncated (`[Session persistence truncated large content]`); signature fields cleared (not truncated); image data-URLs/base64 ≥1024 chars content-addressed to `~/.omp/agent/blobs/<sha256>`, replaced with `blob:sha256:<hash>` in JSONL, resolved back on load. Session-local artifacts dir `<sessionfile-noext>/`: truncated tool outputs (`<n>.<tool>.log` → `artifact://<id>`), subagent outputs (`<id>.md` → `agent://<id>`), subagent session JSONL sidecars. Numeric session-local IDs, scan-on-resume. Fork copies the artifact dir best-effort; blobs are global so no copy.

### 5.5 The storage seam (KEY for Option B)
`session/session-storage.ts` defines `SessionStorage` (fs-like ops) with impls:
- `FileSessionStorage` (real files),
- `MemorySessionStorage` (in-memory),
- **`IndexedSessionStorage` (`indexed-session-storage.ts`)** = shared **local index** + **ordered remote publication** used by Redis/SQL-backed storage. Its `SessionStorageBackend` interface is **path-keyed blob semantics**:
  `init / loadIndex / readFull / readSlices / writeFull / append(line) / updateSessionTitle / truncate / remove / move`.
  `append()` updates the local index immediately and queues the remote publish in **call order** (per-path FIFO); `drain()` awaits all per-path queues so graceful shutdown never exits with a write still on the wire.

**Two distinct "backend seam" designs now exist in the ecosystem:**
1. **OMP `SessionStorageBackend`** — the session file is the unit of storage; a SQL/Redis backend stores whole bodies or append-lines keyed by path with a local index. Low-effort, preserves v3 JSONL + all OMP features, but only index/slice-level queryability.
2. **pi-mono `SessionRepo`/`SessionStorage`/`SessionTree`** — relational decomposition into entries/records/lanes/facts with a conformance suite + SQLite reference. High-fidelity, fully queryable, durable operational records, lanes, writer-leases. The pi-relay-like model.

---

## 6. Semantic mapping table (pi-relay → prime-agent / pi-mono / OMP)

Legend: **✓** = exists (direct counterpart) · **~** = partial / different mechanism · **✗** = absent (no durable counterpart). Migration column = what a one-shot migrator must do.

| pi-relay durable concept (Postgres) | prime-agent (JSONL v3) | pi-mono seam (v4/SQLite) | OMP (v3 + seam) | Migration action |
|---|---|---|---|---|
| **Session identity + config** (`sessions`: id, project_id, runtime_id, system_prompt, provider_config, metadata) | ✓ header `{id,cwd}` + `model_change`/`session_info` entries; system prompt NOT stored in file (lives in runtime) | ✓ `sessions` row + metadata json; model via `model_change` entry | ✓ header + `session_init` entry (systemPrompt, tools, outputSchema) — richest | Emit header + a `model_change` (and `session_init` for OMP) from `provider_config`/`system_prompt`. |
| **Multi-tenancy / routing** (`project_id`, `runtime_id`) | ✗ (flat global dir; cwd is the only scope) | ~ (`sessions.cwd`, `parent_session_id`; no project/runtime) | ~ (cwd-bucketed dirs; no project/runtime FK) | Map `project_id`/`runtime_id` into `metadata` jsonb (lossless, opaque) or drop if the new core re-owns routing. |
| **Conversation messages** (`transcript_entries.item`: UserMessage/AssistantMessage/ToolResult) | ✓ `message{message:AgentMessage}` | ✓ `message{message}` | ✓ `message` | Direct: convert each message item → `message` entry. Map roles. |
| **Tree structure** (`transcript_entries.parent_id` forest) | ✓ `parentId` forest in one file | ✓ `entries.parent_id` + lanes | ✓ `parentId` forest | Direct: preserve parent links. **Id-space caveat:** pi-relay ids are arbitrary text; target ids are 8-hex/uuid — may need id remap (keep an id-map sidecar). |
| **Active leaf pointer** (`sessions.active_leaf_id`, explicit) | ✗ — derived as **last appended line** | ✓ **lanes** (named; `main`) | ✗ — in-memory, fallback last entry | **Order matters.** Emit entries so the intended active tip is appended LAST, or (pi-mono) set lane `main` leaf = `active_leaf_id`. Inactive branches must still be written (as non-terminal lines). |
| **Compaction** (`CompactionSummary` new-root + `source_leaf_id` lineage, cross-session via `source_session_id`) | ~ inline `compaction{summary, firstKeptEntryId, tokensBefore}` — same branch, no lineage pointer | ~ `compaction{summary, retainedTail[] INLINE, tokensBefore}` — embeds kept msgs, no source pointer | ~ inline `compaction{summary, firstKeptEntryId,...}` | **Hardest semantic mapping.** Convert each pi-relay compaction into a target `compaction` entry placed on the *active* branch at the continuation point. `firstKeptEntryId` = first retained message after the compaction root. The pre-compaction branch is preserved (still in file) — matches "compaction is a root, not a replacement". Cross-session compaction (`source_session_id`) needs the source already migrated; record lineage in `details`/`custom`. |
| **Turn lifecycle** (`TurnStarted`/`TurnFinished{turn_id,outcome:Graceful|Interrupted|Crashed}`) | ✗ (no turn entries; recovery via in-memory + message stopReason) | ✓ `operation_started`/`operation_finished{outcome}` + `step_attempt` records | ~ `session_exit` custom entry → synthetic abort | Map TurnStarted/Finished → pi-mono `operation_started`/`operation_finished` (run). For prime-agent/OMP: drop turn markers, rely on assistant stopReason; write a `session_exit`-style `custom` entry (OMP) for crashed tails. |
| **Tool call lifecycle** (`ToolCallStarted`/`ToolResult` + `provider_replay`) | ~ toolResult is a `message`; no separate started; provider replay embedded in assistant message | ✓ `tool_started` record (`replay:never|safe`) + toolResult message; `provider_replay`≈record payload | ~ toolResult message + `tool_execution_start` custom | toolResult → `message`. ToolCallStarted → pi-mono `tool_started` record / OMP `tool_execution_start` custom / drop in prime-agent. `provider_replay` jsonb → keep in message.providerPayload or a `custom` entry. |
| **Daemon tool observation** (`DaemonToolObservation`) | ~ `custom_message` or `custom` | ~ `custom` entry | ~ `custom` | Map to `custom{customType:'pi-relay.daemon-tool-observation'}` (non-LLM) to preserve losslessly. |
| **Queued inputs** (`queued_inputs`: steer/follow_up, idempotent `client_input_id`, status, origin) | ✗ **in-memory only** (session-action-store; lost on restart) | ✓ `queue_enqueued{queue:steer|followUp|nextRun, target}` + `queue_cancelled` records | ~ queued steer/follow-up are in-memory; only durable if flushed into transcript | **Only pi-mono has durable queue.** For prime-agent/OMP: unconsumed queued inputs cannot be preserved as *pending* (no durable queue) — either flush them into the transcript as user messages, or drop with a warning. pi-mono: replay as `queue_enqueued` records. |
| **Actions** (`actions`: durable work records, committed-before-dispatch, lease-fenced resume) | ✗ in-memory action FSM | ✓ `operation_started`/`step_attempt`/`write_deferred` records + `findOpenOperations` recovery | ~ `session_exit`/`vibe-session-lifecycle` custom entries | Only pi-mono preserves the action ledger. prime-agent/OMP: unfinished actions → represent as crashed tail (synthetic abort / `session_exit`), no resume checkpoint. |
| **Delegations** (`delegations`: fan-out, launch guard, child-control ledger, wakeup input) | ~ RLM children = `sub-*/` dirs + `child_usage_attributed` entry; no control ledger | ~ RLM child dirs; no delegation table | ~ `vibe-session-lifecycle` custom (child spawn/turn/tombstone) | Child sessions → independent target sessions with `parentSession`/metadata link. The delegation *control plane* (launch guard, control ledger) has **no counterpart** — preserve as `custom`/`metadata` for audit only; do not expect live resume of a running delegation. |
| **Events / replay cursor** (`events` bigserial id, `subscribe(after_event_id)`) | ~ daemon event stream (transient; `cursor` field in protocol) | ✓ `getLog(afterSeq)` unified seq log | ✗ (no replay log) | pi-relay `events` is a transient buffer cleared on idle → **do not migrate** (re-derive). pi-mono `seq` gives an equivalent cursor natively. |
| **MCP manifests** (`mcp_session_manifests`, content-addressed; `mcp_manifest_fingerprint`) | ~ MCP config in `auth.json`/`mcp.toml`, not per-session | ~ per-session metadata | ~ per-session metadata | Store fingerprint/manifest in `metadata`/`custom` for audit; MCP reconnection is a runtime concern, not session replay. |
| **Optimistic-concurrency revisions** (`session_revision`/`queue_revision`/`transcript_revision`) | ✗ (lease-only) | ~ writer-lease `fence` | ✗ | Internal to pi-relay's concurrency; **not migrated** (no counterpart needed post-migration). |
| **Fork** (`history_fork.rs` deep-copies transcript to new session_id) | ✓ `forkFrom`/`clone` (new file, `parentSession`=source path) | ✓ `fork(scope:tree|branch)` (new session, `parent_session_id`) | ✓ `fork()` (new file, `parentSession`=source id) | Represent pi-relay forks as target forks: set `parentSession`/`parent_session_id` = parent session id. (pi-relay copies rows; target copies file/rows — equivalent result.) |
| **Workspaces + btrfs** (`sessions.workspaces` jsonb, `workspace_id`, subvolumes) | ✗ (no workspace concept in session file; cwd only) | ✗ (cwd only) | ~ (`additionalDirectories[]` in header; no btrfs) | **Out of band** (§9): workspace dirs live on the runtime host, independent of session storage. Migrate `workspaces` jsonb → header `additionalDirectories`/`metadata`; **the btrfs subvolumes themselves stay put**. |

### 6.1 The three takeaways from the mapping
1. **The conversation graph (messages + tree + compaction + fork) maps cleanly to all three targets.** This is the bulk of session data and is low-risk.
2. **pi-relay's *operational durability* (durable queue, committed-before-dispatch actions, delegation control ledger, turn/resume checkpoints) has a first-class counterpart ONLY in the pi-mono seam** (`records`/`lanes`/`findOpenOperations`). prime-agent and OMP keep this in memory or approximate it with `custom` entries. Choosing prime-agent JSONL as-is (Option A) **silently drops pi-relay's crash-resume and durable-queue guarantees.**
3. **Compaction representation differs structurally**: pi-relay = new-root + `source_leaf_id` lineage pointer (cross-session capable); prime-agent/OMP = inline `firstKeptEntryId`; pi-mono = inline `retainedTail` (embeds kept messages). The migrator must convert the lineage-pointer form into the inline form, which requires resolving the active branch *through* each compaction.

---

## 7. Options analysis (A / B / C / D)

**Scoping fact that shapes everything:** prime-agent's coding-agent **is** `@earendil-works/pi-coding-agent@^0.7.1`, and its session layer is the **v3 monolithic `SessionManager` with `fs` hardcoded — no storage seam** (verified: `repos/prime-agent/.../session-manager.ts` imports `appendFileSync`/`writeFileSync` from `fs`). The **v4 backend seam** (lanes/records/facts + `SessionRepo` + conformance) lives in pi-mono's **`packages/agent` (harness)** package and is depended on **only by `packages/agent`, not by the shipping coding-agent**. So "migrate onto a prime-agent core" today means v3 JSONL *unless* we deliberately adopt the newer seam.

Evaluation axes: **durability/recovery**, **concurrency**, **replay**, **query-ability**, **effort**, **risk**.

### Option A — prime-agent JSONL as-is + one-shot migrator
Adopt the v3 `SessionManager` wholesale; write a migrator from pi-relay Postgres → v3 `.jsonl` files + artifact dirs.
- **Durability/recovery:** append-only, no fsync (crash-safe, not power-loss). **Loses pi-relay's committed-before-dispatch, durable queue, and turn/resume checkpoints** (all in-memory in prime-agent). Recovery = reload file, leaf=last line; crashed turns are not reconstructed (no turn markers). *Weaker than pi-relay.*
- **Concurrency:** per-file session lease (`proper-lockfile`); single writer; no multi-host. Fine for a sole user.
- **Replay:** no replay log in the file; daemon event cursor is transient. pi-relay's `events.subscribe(after_event_id)` durability is dropped.
- **Query-ability:** poor — parse JSONL; cross-session queries need a full scan; no SQL.
- **Effort:** core = **lowest** (use as-is). Migrator = **medium-high** (compaction-lineage conversion, id remap, active-leaf append ordering, fork links, workspace metadata).
- **Risk:** low *technical* risk to the new core, **high *semantic* risk** (silently drops the crash-resume/durable-queue guarantees pi-relay exists to provide), and locks to an upstream format the user doesn't control.
- **Fit with "old sessions must work":** ✓ via migrator (one-shot). Good.

### Option B — Postgres backend behind a session seam
Two distinct seams exist (§5.5):
- **B1 — pi-mono v4 relational seam (`SessionRepo`/`SessionStorage`) over Postgres.** Highest fidelity: lanes, durable records, `findOpenOperations` recovery, writer-lease fencing, `getLog(afterSeq)` cursor, full SQL query-ability, conformance suite + SQLite reference to copy. **But the shipping coding-agent doesn't use this seam** → adopting B1 means *also* moving the agent core onto the v4 seam. Effort **high**. This is the only option that fully preserves pi-relay's operational durability in a relational store ("Postgres Is Authoritative" retained).
- **B2 — OMP `SessionStorageBackend` (path-keyed blob) over Postgres.** Preserves the v3 format and all OMP features; but it's file-blob-level durability (no relational records/queue), and prime-agent's `SessionManager` would need OMP's seam refactor ported in. Effort **medium**. Query-ability limited to index/slice.
- **Concurrency:** B1 = `writer_leases` fencing (≡ pi-relay's `resume_model_v1`). B2 = lease.
- **Risk:** B1 high (young seam, upstream divergence, core port); B2 medium. Both keep Postgres authoritative (matches existing backup/ops posture).

### Option C — hybrid (transcript in files, control plane in Postgres)
Keep the **conversation graph** (messages/tree/compaction) in JSONL (prime-agent/pi-mono format), keep the **control plane** (projects, runtimes, delegations, durable queue, actions, replay events) in a slimmed-down Postgres. This mirrors the natural concern split: prime-agent already stores RLM children as files + runs a supervisor daemon; pi-relay's daemon control plane is exactly the part prime-agent lacks.
- **Durability/recovery:** transcript = JSONL (crash-safe); control plane = Postgres (full durability, committed-before-dispatch retained where it matters: queue/actions/delegations).
- **Concurrency:** file lease for transcripts; Postgres for control plane.
- **Replay:** control-plane replay cursor stays in Postgres (`events`); transcript replay is file-based.
- **Query-ability:** control plane full SQL; transcripts file-based (limited) — acceptable since cross-session analytics are usually about the control plane, not message bodies.
- **Effort:** **medium.** Keep prime-agent `SessionManager` for transcripts; keep a reduced agent-store for control plane; migrator splits each pi-relay session into (JSONL transcript) + (PG control rows).
- **Risk:** medium (two stores), but they are *different concern domains* → low coupling, no dual-writes of the same fact.

### Option D — keep agent-store (Postgres), graft prime-agent core
Replace prime-agent's storage with pi-relay's existing committed-before-dispatch Postgres layer behind a `SessionManager`-compatible adapter.
- **Durability/recovery:** full pi-relay semantics retained.
- **Effort / risk:** **highest.** prime-agent's `SessionManager` is monolithic + fs-hardcoded; forcing it behind an interface is precisely the premature-abstraction move pi-relay's `design-decisions.md` warns against — here it's forced, and it forks prime-agent's session layer deeply, making upstream updates costly.

### 7.1 Decision matrix

| Axis | A (JSONL as-is) | B1 (PG via v4 seam) | B2 (PG via OMP blob seam) | C (hybrid) | D (keep agent-store) |
|---|---|---|---|---|---|
| Durability/recovery vs pi-relay | ✗ loses queue/resume | ✓ full | ~ file-level | ✓ control-plane full, transcript file | ✓ full |
| Concurrency | lease | fenced lease | lease | lease + PG | PG |
| Replay cursor | ✗ | ✓ `getLog(seq)` | ✗ | ✓ (PG events) | ✓ |
| Query-ability | ✗ | ✓ full SQL | ~ index/slice | ~ control-plane SQL | ✓ full SQL |
| Core effort | lowest | high | medium | medium | highest |
| Migrator effort | medium-high | medium | medium-high | medium | low (no format change) |
| Upstream divergence | none | high (young seam + core port) | medium | low-medium | highest (deep fork) |
| Keeps "Postgres authoritative" | ✗ | ✓ | ✓ | ~ (control plane) | ✓ |

### 7.2 Recommendation

**Primary: Option C (hybrid), with the transcript written in the pi-mono v4 JSONL format (via the seam), not the legacy v3.**
- It is the lowest-risk path that **does not abandon pi-relay's raison d'être**: the durable queue / actions / delegation control plane / replay events stay in Postgres where they belong and where the user's backup/ops posture already lives.
- Writing transcripts through the **pi-mono v4 seam** (instead of the v3 `SessionManager`) future-proofs the transcript store: v4 JSONL is upstream-supported, ingests v3, and has a relational backend + conformance suite — so **B1 becomes an incremental upgrade, not a rewrite**, if file transcripts later prove limiting.
- It satisfies the hard requirement cleanly: one-shot migrator splits pi-relay → (v4 transcript file) + (PG control rows). No backwards-compat code paths.

**When to prefer A instead:** if the goal is strictly "get onto prime-agent fastest and I accept losing crash-resume/durable-queue" — then A, and the migrator is the whole job. Given pi-relay's design center is exactly those guarantees, A is likely **too lossy**.

**When to prefer B1:** if you want transcripts queryable in Postgres and are willing to port the agent core onto the v4 seam now. Highest fidelity, highest effort/risk. Treat as the **end-state** the hybrid can grow into.

**Avoid D** unless you are prepared to maintain a deep, permanent fork of prime-agent's session layer.

> **Decision hinges on one question for the user:** *Does the post-migration system still need pi-relay's durable, resumable queue + delegation control plane, or is prime-agent's in-memory/volatile model acceptable?* If yes → C (→B1). If no → A.

---

## 8. One-shot migration script plan

**Hard requirement honored:** old pi-relay sessions keep working **only** because this script converts them into the new format. The new code contains **no** backwards-compat/conditional paths. The script is **run once, then deleted**. It must be **idempotent and re-runnable** (safe to run twice) and **additive** (never deletes the source Postgres until the operator explicitly verifies).

### 8.0 Inputs / outputs
- **Source:** pi-relay Postgres (read-only connection): `sessions, transcript_entries, queued_inputs, actions, delegations, events, projects, runtimes, mcp_session_manifests, daemon_config`.
- **Filesystem source:** `<workspace_root>/sessions/<workspace_id>/cwd` btrfs subvolumes (left untouched), `workspace-bases/`, `mcp-oauth-credentials.json`.
- **Target (recommended = Option C):** one **pi-mono v4 JSONL** per session at the new sessions dir + **Postgres control-plane rows** (projects/runtimes/delegations/queue/actions trimmed). (For Option A target, emit **v3 JSONL** instead; for B1, insert relational rows directly.)

### 8.1 Migration order (topological)
Build a dependency graph over `sessions` using **`parent_session_id`** (forks) and **`CompactionSummary.source_session_id`** (cross-session compaction). Migrate **parents before children** (topo sort). Sessions with no deps migrate first. This guarantees that when a child references a parent (fork lineage or compaction lineage), the parent's target id is already known.

### 8.2 Per-session algorithm
1. **Load the full forest:** all `transcript_entries` for the session, ordered by `sequence`. Build `by_id` and `children` adjacency from `parent_id`.
2. **Id remap:** pi-relay entry ids are arbitrary text; target ids are 8-hex (v3/OMP) or storage-assigned (v4). Generate a fresh target id per source id; record `{source_id → target_id}` in an **id-map sidecar** (per session, kept for audit/rollback). Remap every `parent_id`, `active_leaf_id`, `firstKeptEntryId`, fork `parent_session_id`, and compaction `source_leaf_id` through the map.
3. **Resolve the active branch:** start at `sessions.active_leaf_id`, walk `parent_id` to root, **following `item->>'source_leaf_id'` when the entry is a `CompactionSummary`** (pi-relay's compaction lineage hop, possibly into `source_session_id`). This yields the *effective* active path.
4. **Convert compactions:** for each `CompactionSummary` on the active path, emit a target `compaction` entry:
   - v3/OMP: `compaction{summary, tokensBefore, firstKeptEntryId = <target id of first retained message after the compaction root>}`.
   - v4: `compaction{summary, tokensBefore, retainedTail = <the inline kept messages>}`.
   - The pre-compaction branch **stays in the file** (matches "compaction is a root, not a replacement"); record `source_leaf_id`/`source_session_id` lineage in `details`/`custom` for audit.
5. **Convert entries in dependency order**, writing the **active-branch tip LAST** so the target's derived leaf (last-appended line, v3/OMP) or `main` lane leaf (v4) equals `active_leaf_id`. Inactive/abandoned branches are still written (as earlier lines / sibling branches) to preserve the full tree.
   - `UserMessage`/`AssistantMessage`/`ToolResult` → `message`.
   - `ToolCallStarted` → v4 `tool_started` record / OMP `tool_execution_start` custom / **dropped** in v3.
   - `TurnStarted`/`TurnFinished{outcome}` → v4 `operation_started`/`operation_finished` records / **OMP** `session_exit` custom for crashed tails / **dropped** in v3 (rely on assistant `stopReason`).
   - `DaemonToolObservation` → `custom{customType:'pi-relay.daemon-tool-observation', data}` (lossless, non-LLM).
   - `provider_replay` jsonb → keep in the assistant `message.providerPayload` (v3/OMP) or a paired record (v4).
6. **Header:** emit `{id (uuid-v7 or source session id), cwd, parentSession = remapped parent_session_id, timestamp}`. Fold pi-relay-only config (`project_id, runtime_id, system_prompt, provider_config, workspaces, metadata, mcp_manifest_fingerprint`) into header `metadata` (v4) or an initial `custom` entry (v3) so **nothing is silently lost**. Emit a `model_change` (and `session_init` for OMP) from `provider_config`/`system_prompt`.
7. **Control plane (Option C / B only):**
   - `projects`/`runtimes` → keep as-is (control plane stays in PG).
   - `delegations` → keep running/queued control rows in PG; **do not** attempt to live-resume an in-flight delegation in the new core. Represent historical delegations as `metadata`/child-session links.
   - `queued_inputs` with `status='queued'` (unconsumed): **v4** → replay as `queue_enqueued` records; **v3/OMP (no durable queue)** → either flush into the transcript as user messages **or** drop with a logged warning (operator's choice; default = flush so no user input is ever silently lost).
   - `actions` unfinished → v4 `operation_started` (suspended) records; v3/OMP → crashed-tail representation.
   - `events` → **not migrated** (transient reconnect buffer, cleared on idle in pi-relay; the target re-derives its own cursor).
8. **Workspaces (see §9):** copy `sessions.workspaces` jsonb → header `additionalDirectories`/`metadata`. **Do not touch the btrfs subvolumes.** Keep `workspace_id`↔dir mapping in a sidecar so the runtime layer can re-attach.
9. **Write atomically:** build the target file in a temp path, then `rename` into place (mirror the targets' own atomic-write discipline).

### 8.3 Verification (before declaring success)
- **Counts:** per-session entry count in source forest == entries written (modulo intentionally-dropped turn markers); total sessions == total files.
- **Active-branch parity:** reconstruct the target's active branch (its own `buildSessionContext`) and diff the message sequence against pi-relay's active-branch CTE result — must match role-by-role, content-by-content.
- **Compaction parity:** for sessions with compactions, verify the target's reconstructed context equals pi-relay's post-compaction context.
- **Fork/lineage:** every `parentSession`/`parent_session_id` resolves to an existing migrated session.
- **Report** a per-session PASS/FAIL manifest; **fail closed** (leave source PG intact) on any mismatch.

### 8.4 Rollback
The script is **additive**: it writes new files/rows and never mutates/deletes pi-relay Postgres or the btrfs subvolumes. Rollback = delete the new sessions dir + control rows; pi-relay remains fully operational. Only after the operator verifies parity and runs the new core against the migrated sessions do they (manually, in a *separate* step) retire pi-relay Postgres.

### 8.5 Deliverables of the script
- `migrate.ts` (or `.py`) — the one-shot converter (idempotent, re-runnable, `--session <id>` for single-session dry-run, `--verify`, `--limit N`).
- Per-session **id-map sidecar** (`<new-session>.idmap.json`) for audit/rollback.
- A **verification manifest** (`migration-report.json`) with per-session PASS/FAIL + counts.
- (Deleted after successful run, per the repo's "run once and delete" rule.)

---

## 9. Workspaces + btrfs snapshot metadata (today and post-migration)

### 9.1 Today (pi-relay)
- **Where it lives:** entirely on the **runtime host**, owned by `WorkspaceManager` (`agent-runtime/src/workspaces/mod.rs`), rooted at **`state_root = Config.workspace_root`** — a **required, absolute, per-runtime** value from `$XDG_CONFIG_HOME/pi-relay/runtime/config.toml` (`agent-runtime/src/main.rs:152-198`). There is **no built-in default**; startup asserts the root supports btrfs (`validate_root()`, `main.rs:225-229`). **The daemon's Postgres does not own workspace bytes.**
- **Layout:**
  - Session workspaces: `<workspace_root>/sessions/<workspace_id>/cwd` — each a **btrfs subvolume**. `sessions.workspace_id` (DB) **is** the `<workspace_id>` directory name. (The `websocket-rpc.md` "outer_cwd" name is doc drift; the column is `workspace_id`.)
  - Workspace bases: `<workspace_root>/workspace-bases/<project_id>/<workspace_dir>/` + `WORKSPACE_BASE_METADATA`; materialized into a session cwd via `cp -a --reflink=always`; destructively refreshed.
  - MCP OAuth credentials: `<workspace_root>/mcp-oauth-credentials.json` (host-local; never leaves the runtime).
  - `.pi-handoff/` = daemon-owned dir inside a session cwd for delegation handoff artifacts.
- **Fork (workspace-level):** `btrfs subvolume snapshot` (CoW) of the parent cwd into the child's `<workspace_id>/cwd`, then **removes `.pi-handoff`** and re-points `local_branch`. 
- **Destroy:** `btrfs subvolume delete`.
- **What is/isn't in Postgres:** the DB stores only **metadata** — `sessions.workspaces` jsonb (`kind, workspace_dir, remote_url, remote_branch, source_path, base_sha, local_branch`), `workspace_id`, and `parent_session_id` (fork lineage). **The btrfs parent→child snapshot lineage is a FILESYSTEM-LEVEL fact (subvolume parentage) recorded in NO table.** (Confirmed during research: the DB has `sessions.workspaces` + `parent_session_id`; the subvolume parent relationship exists only in btrfs itself.)

### 9.2 Post-migration
- **The btrfs subvolumes and their snapshot lineage survive any session-storage migration unchanged** — they are independent of the daemon's DB. Whichever option (A/B/C/D) is chosen, the workspace layer (`WorkspaceManager`, `workspace_root`, subvolumes, bases, OAuth file) **stays exactly as-is**; only the *session metadata* that references it moves.
- **What the migrator must carry across:** the `sessions.workspaces` jsonb + `workspace_id` (so the new core can re-attach the right cwd) → header `additionalDirectories`/`metadata`. Keep a `workspace_id → <workspace_root>/sessions/<workspace_id>/cwd` sidecar so the runtime layer re-binds.
- **Fork lineage:** pi-relay records workspace forks via `parent_session_id` (DB) while btrfs holds the actual CoW link. Post-migration, `parent_session_id` → target `parentSession`/`parent_session_id`. The btrfs link needs no migration; only the *logical* session-parent link does.
- **Risk:** low. The only hazard is if the new core assumes `cwd` is a plain directory and tries to manage/delete it — it must continue to treat `<workspace_id>/cwd` as a btrfs subvolume managed by `WorkspaceManager`. Keep `WorkspaceManager` (or its port) as the sole owner of workspace lifecycle.

---

## 10. Open questions & risks

1. **Durable control plane — keep or drop?** (The deciding question, §7.2.) Does the post-migration system need pi-relay's durable/resumable queue + delegation control plane, or is prime-agent's volatile model acceptable? → C(→B1) vs A.
2. **v3 vs v4 target format.** prime-agent ships v3 (monolithic, no seam); pi-mono's v4 seam is the aligned, upstream-supported, future-proof model but requires the core to run on it. Recommend v4 for the transcript writer even under Option C. **Confirm the parent agrees before building the migrator.**
3. **Id-space remap.** pi-relay entry ids are arbitrary text; targets use 8-hex/uuid. Remap is mechanical but must be total (parent links, active_leaf, firstKept, fork, compaction lineage). Keep id-map sidecars.
4. **Cross-session compaction ordering.** `CompactionSummary.source_session_id` means a session's active branch can reference *another* session's entries. Migrator must topo-sort and, when the source is a different session, decide: copy the referenced retained messages inline (v4 `retainedTail` handles this naturally) or link. **v4 handles this more cleanly than v3.**
5. **Unconsumed queued inputs & in-flight actions/delegations** have no v3/OMP counterpart. Default policy: flush queued inputs into the transcript as user messages (never silently drop user input); represent unfinished actions as crashed tails; do **not** attempt live-resume of in-flight delegations. Needs an explicit operator sign-off on the drop/flush policy.
6. **`events` replay cursor** is transient in pi-relay (cleared on idle) → not migrated. Confirm no external consumer depends on replaying pre-migration events.
7. **Revisions (`session/queue/transcript_revision`)** are internal optimistic-concurrency tokens with no target counterpart → dropped. Confirm nothing external reads them.
8. **Doc drift:** `websocket-rpc.md` `outer_cwd` ≠ `schema.rs` `workspace_id`; `rust/migrations/` is not a real migration framework. Trust `schema.rs` (idempotent DDL) and introspect the live DB at migration time rather than trusting docs.
9. **prime-agent format ownership.** Under Option A, the user locks to an upstream JSONL format they don't control; upstream v3→v4 shifts could strand migrated files. Mitigation: write v4 via the seam (Option C), which the user *can* own.
10. **btrfs assumption.** The new core must keep treating session cwd as a `WorkspaceManager`-owned btrfs subvolume. If the migration ever moves to a host without btrfs, workspace fork/destroy semantics break (out of scope for the session-storage migration, but a real deployment constraint).

---

## Appendix A — source files read (evidence base)
**pi-relay:** `rust/agent-store/src/postgres/schema.rs` (+ `outputs.rs`, `mod.rs`), `rust/docs/design-decisions.md`, `rust/docs/websocket-rpc.md`, `rust/docs/runtime.md`, `rust/docs/modules/agent-daemon.md`, `rust/crates/agent-session/…/history_fork.rs`, `rust/crates/agent-runtime/src/workspaces/mod.rs`, `rust/crates/agent-runtime/src/main.rs`.
**prime-agent:** `repos/prime-agent/packages/coding-agent/src/core/session-manager.ts`, `session-lease.ts`, `session-action-store.ts`, `config.ts`; `.pi/prime-agent-architecture.md`.
**pi-mono:** `repos/pi-mono/packages/coding-agent/src/core/session-manager.ts`, `repos/pi-mono/packages/agent/src/harness/session/types.ts`, `…/jsonl/codec.ts`, `…/jsonl/storage.ts`, `…/testing/conformance.ts`, `repos/pi-mono/packages/session-backends/sqlite-node/src/sqlite/migrations/001_initial.sql`, `…/repo.ts`.
**oh-my-pi:** `repos/oh-my-pi/docs/session.md`, `docs/session-tree-plan.md`, `docs/blob-artifact-architecture.md`, `docs/session-operations-export-share-fork-resume.md`, `packages/coding-agent/src/session/session-storage.ts`, `…/indexed-session-storage.ts`.
