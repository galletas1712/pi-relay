# Workflow-Orchestration Build Map

Authoritative implementation map for `rust/docs/plans/workflow-orchestration.md`, consolidated from 11 verified
code-seam reports. This file replaces the need to re-read the code. **All daemon/store/tool paths are under
`rust/crates/`** (the spec writes them as `agent-daemon/src/...`; prefix `rust/crates/`). All cited line numbers
verified post-rebase unless flagged as drift.

---

## Table of contents
1. Areas (key_locations / build_seams / gotchas / spec-drift), per area
2. Net-new pieces #1–#7 → seam mapping
3. Phase build order with file touch-list
4. Provider credentials for the fresh-DB real-model e2e
5. Open risks / unverified

---

# 1. Areas

## 1.1 spawn-path

A subagent spawn is a parent/child session fork. `subagent_spawn_from_active_parent` (the entry) deserializes into
`SubagentSpawnRequest`, then calls `spawn_subagent`, which loads the parent `SessionConfig`, requires a project,
resolves the role skill via `resolve_skill_role`, **ALWAYS forks** the parent workspace via
`fork_session_from_parent` (child `outer_cwd` + branched `workspaces`), builds child config + system prompt, imports
git source-refs, then starts via `start_prepared_session` (Deferred dispatch) → `start_session_outputs_with_parent`
(the INSERT). Parent-scoped `SubagentSpawned`/`SubagentRunning` events fire, then the child dispatches. **Only
`subagent.list` and `subagent.spawn` are real RPC/REPL entries; wait/read/steer/interrupt are Python-REPL host
functions in `repl.rs`, not daemon RPCs.** Subagent identity is **metadata-only** today — there is NO `subagent_type`
or `stage_id` column.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-daemon/src/subagents.rs | spawn_subagent | 191-376 | Core spawn path. Net-new #1 (skip fork for full) lands here. |
| rust/crates/agent-daemon/src/subagents.rs | fork_session_from_parent call site | 264-273 | THE fork call (verified exact). Returns (outer_cwd, workspaces). For full, skip + reuse parent's. |
| rust/crates/agent-daemon/src/subagents.rs | subagent_spawn_from_active_parent | 69-76 | RPC/REPL entry; wraps spawn_subagent. |
| rust/crates/agent-daemon/src/subagents.rs | SubagentSpawnRequest / Params / from_params | 92-184 | Request shape + validation. Add `subagent_type` here. |
| rust/crates/agent-daemon/src/subagents.rs | subagent_metadata | 579-623 | Builds child metadata (subagent, role_name, hidden, task, role_file_path). |
| rust/crates/agent-daemon/src/subagents.rs | subagent_list | 20-67 | Only subagent control RPC (RpcMethod::SubagentList, main.rs:290). Reads role from metadata. |
| rust/crates/agent-daemon/src/subagents.rs | require_known_subagent | 541-559 | Scope check: child.parent_session_id matches. Reuse for stage.* scoping. |
| rust/crates/agent-daemon/src/subagents.rs | cleanup_failed_spawn | 561-577 | Teardown on spawn failure. Model for RO snapshot GC (#2). |
| rust/crates/agent-daemon/src/provider_runtime/skills.rs | resolve_skill_role | 107-151 | Resolves role name(+workspace)→ResolvedSkillRole; falls back to packaged subagent-roles. |
| rust/crates/agent-daemon/src/repl.rs | subagents_list/read/steer/interrupt_host | 646-712 | Control surface — Python REPL host fns, NOT RPCs. steer enqueues InputPriority::Steer. |
| rust/crates/agent-daemon/src/repl.rs | wait_for_children_idle | 480-516 | The busy-wait `subagents.wait`; spec replaces with park+steer barrier. |
| rust/crates/agent-daemon/src/repl.rs | parent_context_block | 580 | Spec's handoff-render example (NOT directly reusable; see transcript-render). |
| rust/crates/agent-daemon/src/session_start.rs | start_prepared_session / _with_driver | 139-259 | Creates child row (via start_session_outputs_with_parent 205-218). Called from spawn_subagent:322, Deferred. |
| rust/crates/agent-daemon/src/session_start.rs | ensure_session call | 192-196 | start_prepared_session_with_driver ALWAYS calls workspaces.ensure_session(id, cwd, workspaces). |
| rust/crates/agent-store/src/postgres/sessions.rs | start_session_outputs_with_parent | 231-287 | THE INSERT. 9 cols: id, project_id, outer_cwd, workspaces, active_leaf_id, system_prompt, provider_config, metadata, parent_session_id. No subagent_type/stage_id. |
| rust/crates/agent-store/src/postgres/session_links.rs | session_parent_id / list_child_session_ids / set_session_parent | 10-53 | Parent/child linkage queries. |
| rust/crates/agent-daemon/src/workspaces/mod.rs | fork_session_from_parent | 150-217 | Snapshot mechanism RO keeps, full skips. |
| rust/crates/agent-daemon/src/workspaces/mod.rs | remove_session_dir | 260-266 | remove_dir_all; does NOT reclaim btrfs subvolumes (#2). |
| rust/crates/agent-daemon/src/main.rs | enqueue_session_input / interrupt_session | 924, 1240 | Steer delivery + interrupt primitives reused by control RPCs + barrier→steer. |

### build_seams
**Net-new #1 — skip the fork for full.** Single fork call site `subagents.rs:264-273`; returned `(outer_cwd, workspaces)`
flow into child `SessionConfig` at 284-291. Add `subagent_type` enum `{ Full, ReadOnly }` (wire `full | read_only`) to
`SubagentSpawnRequest`/`Params` (92-184). Branch at 264-273:
- `ReadOnly` → keep `fork_session_from_parent` verbatim (the disposable snapshot).
- `Full` → `let (outer_cwd, workspaces) = (parent_config.outer_cwd.clone(), parent_config.workspaces.clone());` (run
  against parent dirs in place). VERIFY `ensure_session` (session_start.rs:192) only validates, does NOT re-fork/wipe
  when cwd already belongs to parent. Do NOT call `import_source_refs`/teardown that assume a private child dir.

`source_refs` import (subagents.rs:303-313) is spec-rejected; full stages carry no `sources` — unreachable for full.

**Persisting the type:** cleanest = new `subagent_type` column threaded through `start_prepared_session` →
`start_session_outputs_with_parent` (sessions.rs:231-268), add `subagent_type text null` to INSERT + a `.bind`.
Interim it can also stamp into `metadata` via `subagent_metadata`, but spec wants a column for `stage.*` querying.

**Control RPCs:** stage.* must promote steer/interrupt to real RPCs reusing `enqueue_session_input(InputPriority::Steer)`
(main.rs:924) + `interrupt_session` (main.rs:1240). RO subagents reject steer/interrupt by `subagent_type`.

### gotchas
- `fork_session_from_parent` is the ONLY fork call (subagents.rs:264). No second/conditional fork.
- Control surface is split: subagent.list + subagent.spawn are real RPCs; wait/read/steer/interrupt are Python-REPL
  host fns (repl.rs:646-712), busy-wait at wait_for_children_idle:480. Promoting to RPCs is net-new wiring, not rename.
- For a full subagent reusing parent's outer_cwd, `cleanup_failed_spawn`/`remove_session_dir` MUST NOT delete the
  parent's workspace — full needs a teardown that skips workspace removal.
- list() reads role_name from metadata (45-56). If subagent_type→column, list/status must read the column.
- source-refs / fork_context / initial_context are spec-REJECTED for stages (fresh context). Full stages never receive
  sources; do not extend that path.
- `resolve_skill_role` falls back to packaged `subagent-roles`; workflows go under `workflows/` via a SEPARATE
  `load_global_skills_from_dir` (NOT resolved by resolve_skill_role).
- `require_known_subagent` gates every control path on parent match — reuse for stage.*.

### spec-drift
- Spec table lists wait/read/steer/interrupt as if uniform RPCs — they are Python-REPL host fns. Only list+spawn are real.
- `subagents.wait` = busy-wait `wait_for_children_idle` (repl.rs:480), not a barrier.
- Sessions table has 9 columns, NO subagent_type/stage_id — net-new for Phases 1/2.
- `enqueue_session_input` is at main.rs:924 (spec said ~925, off by one — harmless).

---

## 1.2 workspaces-btrfs

`WorkspaceManager` owns a per-host state root (`$XDG_STATE_HOME/pi-relay` or `~/.local/state/pi-relay`). Each session →
`state_root/sessions/<id>/`, with `cwd/` = outer_cwd; workspaces at `cwd/<workspace_dir>`. Project bases cached at
`state_root/workspace-bases/<project_id>/<dir>/`, instantiated via btrfs snapshot → reflink → copy (`instantiate.rs`).
Forking (`fork_session_from_parent`) replicates parent cwd via `materialize_tree_from_source_exact`. Teardown today is
plain `remove_dir_all`, which does NOT reclaim btrfs subvolumes — that is net-new #2.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-daemon/src/workspaces/mod.rs | session_root | 289-293 | session_id → state_root/sessions/<id>; outer_cwd = +/cwd. Destroy walks this. |
| rust/crates/agent-daemon/src/workspaces/mod.rs | materialize_session | 83-113 | Creates root, root/cwd, cwd/<dir> per workspace. The layout destroyer reverses. |
| rust/crates/agent-daemon/src/workspaces/mod.rs | fork_session_from_parent | 150-217 | RO snapshot creation; each git workspace_dir may be its own subvolume. |
| rust/crates/agent-daemon/src/workspaces/mod.rs | remove_session_dir | 260-266 | CURRENT teardown — plain remove_dir_all. Does NOT btrfs-delete. |
| rust/crates/agent-daemon/src/workspaces/mod.rs | create_workspace_dir | 381 | May create the cwd root as a subvolume (via instantiate.rs:8). |
| rust/crates/agent-daemon/src/workspaces/instantiate.rs | try_btrfs_subvolume_snapshot | 83-105 | `btrfs subvolume snapshot`. Needs `subvolume delete`, not rm. |
| rust/crates/agent-daemon/src/workspaces/instantiate.rs | try_btrfs_subvolume_create | 107-123 | `btrfs subvolume create`. Mirror its exec/NotFound pattern in destroy fn. |
| rust/crates/agent-daemon/src/workspaces/instantiate.rs | materialize_tree_from_source_exact | 29-56 | Fork replicator: snapshot → subvol+reflink → copy. Determines which paths are subvolumes. |
| rust/crates/agent-daemon/src/subagents.rs | spawn_subagent fork call | 264-273 | Forks every subagent today (#1 skips for full). Forked child = RO snapshot the destroy reclaims. |
| rust/crates/agent-daemon/src/subagents.rs | cleanup_failed_spawn / import error path | 310, 568 | Existing remove_session_dir callers; new destroy added at RO terminal. |
| rust/crates/agent-daemon/src/state.rs | AppState.workspaces | 35 | How runtime reaches WorkspaceManager to call destroy. |
| rust/crates/agent-daemon/src/session_start.rs | materialize_session caller | 75 | Where a full session's outer_cwd (parent cwd) is established; where .pi-handoff lives. |

### build_seams
**Net-new #2 — subvolume-aware destroy.** Add `destroy_workspace_tree(root: &Path) -> Result<()>` in `instantiate.rs`
next to `try_btrfs_subvolume_create` (107). Walk depth-first; recurse into dirs first; attempt
`btrfs subvolume delete <path>` (treat `ErrorKind::NotFound` = btrfs absent → Ok(false), exactly like try_btrfs_subvolume_create:107-123);
finally `tokio::fs::remove_dir_all(root)` for reflink/copy remainder. (Alt: `btrfs subvolume delete -R <root>` then
remove_dir_all.) Depth-first is required because `btrfs subvolume delete` refuses a subvolume containing child
subvolumes. NO new dependency.

Add public method on `WorkspaceManager` (mod.rs, next to remove_session_dir:260):
```rust
pub(crate) async fn destroy_session_workspaces(&self, session_id: &str) -> Result<()> {
    let root = self.session_root(session_id);
    if root.exists() { destroy_workspace_tree(&root).await?; }
    Ok(())
}
```
Name verbatim (spec line 548). Import via the `use self::instantiate::{...}` block (mod.rs:30-32). **Redirect
`remove_session_dir` to delegate to the same primitive** (no-backward-compat-shims rule). Call
`destroy_session_workspaces(child)` at the RO-only subagent-lifecycle terminal path.

**Handoff placement:** `.pi-handoff` is a SIBLING of workspace dirs under `outer_cwd`
(`<session_root>/cwd/.pi-handoff/<stage_id>/...`), outside every git repo/subvolume. CRITICAL: the durable handoff lives
under the PARENT's cwd. But the fork (materialize_tree_from_source_exact:29) copies the WHOLE parent cwd including
`.pi-handoff` — builder must **exclude `.pi-handoff` from the fork**, else the child gets a stale copy (harmless, since
durable copy is under parent, and destroy reclaims it).

### gotchas
- NO `btrfs subvolume delete` anywhere today — entire reclaim primitive is net-new.
- Nested subvolumes: cwd root AND each cwd/<dir> may be separate subvolumes → MUST delete depth-first or use `-R`.
- `remove_dir_all` LEAKS subvolume metadata on btrfs (exact leak spec calls out, lines 493-494).
- btrfs binary may be absent — destroy must still remove_dir_all the reflink/copy remainder.
- Two existing remove_session_dir callers (subagents.rs:310,568; main.rs:505 session delete) should route through the
  new primitive to avoid divergent teardowns.

### spec-drift
- None. mod.rs:150 / 260 are EXACT. `destroy_session_workspaces` is net-new (does not exist yet).

---

## 1.3 steer-enqueue

Deliver a steer by calling free async `enqueue_session_input(state: &AppState, request: SessionInputRequest)`
(main.rs:924). `SessionInputRequest { session_id, priority: InputPriority, content: agent_vocab::UserMessage, +3
Options }`. `InputPriority` = `{ FollowUp, Steer }`. Build with `priority: Steer`, `session_id` = PARENT id,
`content: UserMessage::text(msg)`, optionals `None`. This is exactly `subagents_steer_host` (repl.rs:682) but targeting
the parent. A Steer-priority queued input is later consumed by `take_next_queued_steer_input` so it lands in the parent
transcript ahead of follow-ups. The lifecycle-hook seam is `SessionDriver::subagent_parent_idle_event` /
`notify_subagent_parent_idle_if_needed` (runtime/mod.rs:391-515).

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-daemon/src/main.rs | enqueue_session_input | 924-927 | The async entrypoint. `pub(crate) async fn(state:&AppState, request:SessionInputRequest)->Result<Value,RpcError>`. |
| rust/crates/agent-daemon/src/main.rs | SessionInputRequest | 915-922 | Struct: session_id:String, priority:InputPriority, content:UserMessage, client_input_id/base_leaf_id/expected_active_leaf_id:Option. |
| rust/crates/agent-store/src/lib.rs | InputPriority | 17-21 | text_enum FollowUp=>"follow_up", Steer=>"steer". |
| rust/crates/agent-daemon/src/repl.rs | subagents_steer_host | 682-702 | COPY-FROM reference. Differs only in targeting child + require_known_subagent. |
| rust/crates/agent-daemon/src/state.rs | AppState | 24-37 | Clone handle; .repo, .events, .active. &AppState required to call enqueue_session_input. |
| rust/crates/agent-daemon/src/runtime/mod.rs | subagent_parent_idle_event / notify_subagent_parent_idle_if_needed | 391-515 | LIFECYCLE HOOK SEAM. SessionDriver owns cloned AppState in self.state → call enqueue_session_input(&self.state,..) directly. |
| rust/crates/agent-store/src/postgres/events.rs | insert_subagent_idle_event_once | 60-99 | Idempotency primitive: dedupe by notification_key. Model the once-per-stage guard on this. |
| rust/crates/agent-store/src/postgres/session_links.rs | session_parent_id / list_child_session_ids | 36-56 | parent resolution + stage subagent enumeration for barrier. |
| rust/crates/agent-store/src/postgres/queue.rs | take_next_queued_steer_input | 168-169 | Consumer: Steer-priority input surfaces ahead of follow-ups. |

### build_seams
Build the request exactly as subagents_steer_host but with PARENT id (priority Steer, UserMessage::text(message),
optionals None). Hook alongside `subagent_parent_idle_event` (mod.rs:438): (1) resolve parent via
`self.state.repo.session_parent_id(&self.session_id)`; (2) barrier: enumerate stage subagents, confirm all terminal;
(3) single-flight guard modeled on insert_subagent_idle_event_once (events.rs:60); (4) deliver one steer. Only
`&AppState` needed (already in `self.state`). New module needs
`use crate::{enqueue_session_input, SessionInputRequest}; agent_store::InputPriority; agent_vocab::UserMessage`.

### gotchas
- `enqueue_session_input` is `pub(crate)` → steer runner MUST live in agent-daemon crate (not agent-store).
- TWO functions named enqueue_session_input: the daemon free fn (main.rs:924, the steer path) vs the in-memory engine
  method `agent_session::Session::enqueue_session_input` (runtime/mod.rs:589). Do NOT confuse them.
- Idempotency is MANDATORY: enqueue_session_input does NOT dedupe (client_input_id is the only replay guard, steer host
  passes None). Reuse the once-by-key pattern or a deterministic client_input_id.
- The existing idle event fires per single child; "ALL stage subagents terminal" is net-new.
- enqueue_session_input returns Result<Value,RpcError>; from a fire-and-forget hook (returns ()), log the error
  (mirror try_subagent_parent_idle_event eprintln, mod.rs:425-436).

### spec-drift
- enqueue_session_input at main.rs:924 (spec ~925, off by one).

---

## 1.4 store-schema

agent-store is the durable Postgres source of truth. **NO migrations dir / sqlx-migrate / refinery** — the entire schema
is one embedded `SCHEMA_SQL` string in `schema.rs`, applied idempotently via `sqlx::raw_sql` in `schema::migrate(pool)`,
called by `PostgresAgentStore::migrate()` (mod.rs:39). New objects = edit the string with
`create table if not exists` / `alter table ... add column if not exists`. `sessions` already has
`parent_session_id text null references sessions(id) on delete set null` + index. **"Existing subagent workspace-fork
metadata" = the `workspaces jsonb` column (Vec<SessionWorkspace>: source_path/base_sha/local_branch) + `metadata jsonb`
— NOT dedicated columns.** Three session reads use EXPLICIT column lists and must update in lockstep.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-store/src/postgres/schema.rs | SCHEMA_SQL / migrate | 20-128 | THE migration mechanism. sessions 30-46; parent_session_id 41/50; index 55-57. Add stages table + 2 alters here (before closing `"#` ~123). |
| rust/crates/agent-store/src/postgres/mod.rs | PostgresAgentStore::migrate | 39-41 | Sole entrypoint. Add `mod stages;` to module list (1-16). |
| rust/crates/agent-store/src/lib.rs | SessionConfig/SessionWorkspace/SessionSummary/SessionSnapshot/EventType | 95-103,152-199,287-302,428-447,58-92 | Row structs. SessionWorkspace(152)=fork metadata carrier. EventType text_enum(58) has SubagentSpawned/Running/Idle. |
| rust/crates/agent-store/src/postgres/sessions.rs | create_session / start_session_outputs_with_parent / list_sessions / load_session_config | 43-77,231-298,419-496,498-526 | Inserts + reads. Explicit selects at 428-442, 500-510. |
| rust/crates/agent-store/src/postgres/snapshots.rs | session_snapshot | 17-99 | Explicit column read (24-25). EASY TO MISS — different file. |
| rust/crates/agent-store/src/postgres/session_links.rs | set_session_parent / session_parent_id / list_child_session_ids | 6-61 | Closest analog for new stages repo methods. |
| rust/crates/agent-store/src/postgres/actions.rs | action_can_complete / mark_action_running_and_event | 138-193 | attempt_id/CAS fence + single-flight; reuse for stage barrier CAS. |
| rust/crates/agent-store/src/postgres/sql.rs | lock_session_tx | 63-67 | `select id from sessions where id=$1 for update`. Stage barrier uses same FOR UPDATE on stages row. |

### build_seams
**Migration:** append inside the `r#"..."#` literal (after events block, before closing):
```sql
create table if not exists stages (
    id text primary key,
    parent_session_id text not null references sessions(id) on delete cascade,
    workflow text null, label text null,
    kind text not null, status text not null,
    attempt_id text not null,
    created_at timestamptz not null default now(),
    updated_at timestamptz not null default now()
);
create index if not exists stages_parent_created_idx on stages(parent_session_id, created_at, id);
alter table sessions add column if not exists stage_id text null references stages(id);
alter table sessions add column if not exists subagent_type text null;
```
**ORDER MATTERS:** create `stages` BEFORE the `alter ... references stages(id)`.

**Rust structs (lockstep):** `text_enum!` in lib.rs (58-92): `SubagentType { Full=>"full", ReadOnly=>"read_only" }`,
`StageKind { Full=>"full", ReadonlyFanout=>"readonly_fanout" }`,
`StageStatus { Running, Done, DoneWithFailures, Cancelled, Failed }`. Inserts default null (no change needed). To SET
values at spawn, either extend start_session_outputs_with_parent or add a setter mirroring set_session_parent
(session_links.rs:7-33). The three explicit reads add `s.stage_id, s.subagent_type` only if surfaced: list_sessions
(428), load_session_config (500 + struct lib.rs:95), session_snapshot (snapshots.rs:24 + SessionSnapshot lib.rs:428).

**New stages repo** (`postgres/stages.rs`, register `mod stages;`): `create_stage`, `list_stage_subagents`,
`finish_stage_if_ready` (the barrier CAS — FOR UPDATE lock + `update ... where id=$1 and attempt_id=$3 and
status='running'`), `sweep_running_stages`.

### gotchas
- NO migrations dir/runner — only the embedded SCHEMA_SQL string. Do not add a migrations/ dir.
- schema.rs ordering significant (stages before the alter referencing it).
- NO dedicated fork columns exist — fork lineage is in `workspaces jsonb`/`metadata jsonb`. Don't invent columns.
- Three EXPLICIT-column reads (not `select *`): list_sessions, load_session_config, session_snapshot.
- Wire values: `read_only` (subagent_type, underscore) but `readonly_fanout` (stages.kind, NO underscore) — deliberately
  different strings. stages.status has 5 values, kind has 2.
- Barrier finish MUST be CAS on attempt_id + status='running' (actions.rs:138/171 idiom), else double-steer.

### spec-drift
- Spec asserts "existing subagent workspace-fork metadata" columns to reuse — they DON'T exist (jsonb only).

---

## 1.5 attempt-cas

A single well-factored attempt-fence + single-flight + recovery pattern around the `actions` table is the exact template
for the stage barrier. Single-flight two ways: in-process per-session mutex (`SessionDriver::acquire`) AND DB row lock
(`lock_session_tx` = `select ... for update`). CAS fence = `UPDATE ... WHERE id=$row AND attempt_id=$attempt AND
status IN (...)` guarded by `rows_affected()`. `attempt_id` = `Uuid::new_v4().to_string()` minted at insert
(outputs.rs:213). Startup recovery = a sweep (`mark_all_unfinished_actions_stale`, main.rs:59) + per-session
`recover_if_needed`. Cross-session completion signal already exists: a terminal child mints a `notification_key` and
`insert_subagent_idle_event_once` fires the parent once.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-store/src/postgres/outputs.rs | attempt_id minting | 212-236 | `let attempt_id = Uuid::new_v4().to_string();`. Stages mint the same way. |
| rust/crates/agent-store/src/postgres/actions.rs | action_can_complete | 138-160 | CAS read-fence template for stage_can_finish. |
| rust/crates/agent-store/src/postgres/actions.rs | mark_action_running_and_event / claim_pending_model_action | 162-193, 267-289 | CAS write-fence in row-locked tx, `rows_affected()!=1 → no-op`. EXACT template for finish_stage. |
| rust/crates/agent-store/src/postgres/sql.rs | lock_session_tx / action_is_unfinished | 32-75 | Row-lock helper + terminal-status predicate. Need lock_stage_tx + stage_subagents_all_terminal. |
| rust/crates/agent-daemon/src/main.rs | mark_all_unfinished_actions_stale (boot sweep) | 59-62 | Add `repo.sweep_running_stages()` next to this. |
| rust/crates/agent-store/src/postgres/actions.rs | mark_all_unfinished_actions_stale (query) | 18-40 | Template for a stages-table sweep CTE. |
| rust/crates/agent-daemon/src/runtime/mod.rs | SessionDriver::acquire / session_driver_lock | 79-104 | In-process per-session mutex (NOT the stage single-flight). |
| rust/crates/agent-daemon/src/runtime/mod.rs | recover_if_needed / reconcile_abandoned_boundary_session | 147-221, 397-423 | Per-session lazy recovery; recovered subagent reaching terminal = when barrier re-evaluates. |
| rust/crates/agent-daemon/src/runtime/mod.rs | subagent_parent_idle_event / try_subagent_parent_idle_event | 425-515 | THE barrier signal source. Called from drive_until_blocked(359), recover_if_needed(211), reconcile(420). |
| rust/crates/agent-store/src/postgres/events.rs | insert_subagent_idle_event_once | 60-99 | Metadata-CAS once-fire; stage equivalent is the stage-row status CAS. |
| rust/crates/agent-store/src/postgres/schema.rs | actions table DDL | 97-112 | Shape stages table mirrors. |

### build_seams
**finish_stage CAS** (new `postgres/stages.rs`): tx → `select id from stages where id=$1 for update` (no-op return false
if missing) → `update stages set status=$3, updated_at=now() where id=$1 and attempt_id=$2 and status='running'` →
`Ok(rows_affected()==1)`. Gives all four properties: single-flight (row lock), idempotent (status guard), attempt-fenced,
rows_affected gate (identical to claim_pending_model_action:284). **Barrier predicate** `stage_subagents_all_terminal`
mirrors has_unfinished_actions (58-67) over `sessions WHERE stage_id=$1`. **Hook** at the three call sites of
try_subagent_parent_idle_event — add one `try_stage_barrier(&self)` on SessionDriver. **Steer** after finish_stage→true.
**Startup sweep** at main.rs:59: `sweep_running_stages_all_terminal` runs the same CAS (crash mid-barrier re-completes once).

### gotchas
- Two single-flight mechanisms: SessionDriver::acquire (in-process, keyed by session_id) vs DB row lock. **Per-stage
  single-flight MUST be the DB stage-row lock** (cross-process, survives restart); in-process mutex does NOT serialize
  concurrent terminal children of the same stage.
- CAS idempotency depends ENTIRELY on the `status='running'` predicate + rows_affected check. Omitting the status guard
  re-fires the steer.
- attempt_id minted with Uuid::new_v4 (NOT sequence/hash). v1 mints once; re-mint only if implementing stage retries.
- Barrier signal is per-child today; spec requires ONE steer per stage. The barrier must SUPPRESS the per-child idle
  notification for stage members and emit a single stage steer — don't let both fire. Branch in
  try_subagent_parent_idle_event on whether the child has a stage_id.
- Recovery semantics DIFFER: crashed actions get re-DRIVEN (marked stale); a crashed-mid-barrier stage is still
  'running' with all subagents terminal → recovery is to COMPLETE it (run the CAS), not mark stale. Don't blindly copy
  the stale-marking sweep.
- lock_session_tx bails "session not found" if missing — a stage-lock helper should treat a missing stage as benign
  no-op (return false), not crash a late lifecycle event.

### spec-drift
- None for cited symbols. stages table + sessions.stage_id/subagent_type are net-new as expected.

---

## 1.6 lifecycle-events

PR #150's parent-visible child lifecycle is built on the per-session event log + a single in-process
`tokio::sync::broadcast::Sender<EventFrame>`, NOT a dedicated bus. Three event TYPES exist: `subagent.spawned`,
`subagent.running`, `subagent.idle`. **There are NO terminal types (done/failed/cancelled/crashed)** — the spec's
`subagent.{done,failed,cancelled,crashed}` set does NOT exist. The single terminal signal is ONE `subagent.idle`
carrying an `outcome` (`TurnOutcome`: Graceful/Interrupted/Crashed only) + summary_preview/role. Made exactly-once by
`insert_subagent_idle_event_once` keyed on notification_key. "session_links" = the `parent_session_id` column, no link
table.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-store/src/lib.rs | EventType | 58-92 | Only 3 subagent strings: SubagentSpawned="subagent.spawned"(64), Running(65), Idle(66). No terminal variants. |
| rust/crates/agent-vocab/src/transcript_item.rs | TurnOutcome | 6-11 | Graceful, Interrupted, Crashed. The `outcome` carried by subagent.idle — classify success/failure from THIS. |
| rust/crates/agent-daemon/src/state.rs | AppState.events: broadcast::Sender<EventFrame> | 30 | THE channel. Single process-wide, capacity 1024 (main.rs:64). Not durable, not per-session. |
| rust/crates/agent-daemon/src/runtime/events.rs | publish_events | 6-10 | Only publish path: state.events.send per frame. |
| rust/crates/agent-daemon/src/main.rs | handle_socket / events_rx loop | 137-208 | Consumer pattern: subscribe(139), recv(176), filter by session_id(195), Lagged replay(179-191). |
| rust/crates/agent-daemon/src/main.rs | events_subscribe | 833-865 | events.subscribe RPC handler. |
| rust/crates/agent-daemon/src/runtime/mod.rs | try_subagent_parent_idle_event / subagent_parent_idle_event | 425-515 | Canonical "child reached terminal" producer. outcome+notification_key derived 462-480; once-insert 496-513. |
| rust/crates/agent-daemon/src/runtime/mod.rs | drive loop terminal exits | 211-213, 359-361 | EXACT hook points (normal 359-361; recovery 211-213). |
| rust/crates/agent-daemon/src/runtime/mod.rs | recover_if_needed | 147-221 | Re-emits idle event for abandoned now-idle child. Crash-safe analogue of the startup sweep. |
| rust/crates/agent-daemon/src/runtime/mod.rs | notify_subagent_parent_idle_if_needed | 391-394 | Public wrapper used by repl.rs:504. |
| rust/crates/agent-store/src/postgres/events.rs | insert_subagent_idle_event_once | 60-99 | Exactly-once: dedupe on notification_key in CHILD metadata; inserts on PARENT session only. |
| rust/crates/agent-store/src/postgres/session_links.rs | set_session_parent/session_parent_id/list_child_session_ids | 6-61 | "session_links" = parent_session_id column. |
| rust/crates/agent-daemon/src/subagents.rs | subagent_parent_spawn_events / publish_subagent_parent_running_if_child / dispatch_failed | 378-466,469+ | Full producer surface for spawned/running/idle triad. |

### build_seams
**(A) In-process hook at producer (RECOMMENDED).** When try_subagent_parent_idle_event returns Some (exactly-once
guaranteed), call `self.state.stage_runner.on_subagent_terminal(&self.session_id, outcome)`. Runner does the row-locked
stage CAS (workflow-orchestration.md:385-403). **(B)** Subscribe to broadcast channel and filter SubagentIdle —
duplicates Lagged/replay handling, racy across restarts; (A) is strictly simpler. **Classification:** read `outcome`
(mod.rs:462-467) — Graceful→success, Interrupted/Crashed→failure. Do NOT add new EventType variants; carry status in
the stage row + handoff JSON. **Enumerate** via list_child_session_ids ∩ stages mapping; child terminal iff
`activity==Idle` AND not in `state.active` (mirror mod.rs:210). **Startup sweep** modeled on recover_if_needed. **Wire**
the stage runner handle into AppState next to events (state.rs:30).

### gotchas
- Terminal event names `subagent.{done,failed,cancelled,crashed}` DO NOT EXIST — only spawned/running/idle. Read
  `outcome` instead.
- TurnOutcome has only 3 variants (Graceful/Interrupted/Crashed) — no Failed/Cancelled/Done.
- "session_links" is NOT a table — it's the parent_session_id column. A stage→subagent table doesn't exist yet.
- The broadcast channel is single, in-process, NOT durable. Subscribers must handle RecvError::Lagged (replay from
  events_after). Hook the producer (A), don't subscribe.
- The terminal idle event is inserted on the PARENT session_id (events.rs:86-87) — `event.session_id` is the parent;
  filtering by child id misses it.
- recover_if_needed only re-emits idle when `!should_continue && activity==Idle`. A recoverable crash is DRIVEN to
  continue, not reported terminal — don't treat transient crashes as terminal; wait for genuine idle.

### spec-drift
- Spec path agent-daemon/src/subagents.rs → rust/crates/agent-daemon/src/subagents.rs; runtime/ is a module dir
  (events.rs, mod.rs, dispatch.rs, model.rs).
- No subagent.{done,failed,cancelled,crashed} types — terminal is solely subagent.idle + outcome.

---

## 1.7 rpc-tools

TWO distinct tool surfaces the spec conflates. **(1) Daemon client RPC methods** = hand-maintained `RpcMethod` enum
(types.rs:44-122), `parse` from strings, dispatched by a match in main.rs:255-293. NOT shown to the model; external
JSON-RPC API. **`subagent.spawn` is NOT registered here — only `subagent.list`.** **(2) Model-facing function tools** =
separate `ToolRegistry` built by `FirstPartyToolExtension::register` (registry.rs:335-353): Edit, Bash, Grep, WebSearch,
WebFetch, LoadSkill, PythonRepl. The model reaches `subagent.spawn` ONLY by writing Python in PythonRepl → host fns in
repl.rs → subagent_spawn_from_active_parent (repl.rs:452). `run_tool_turn` (runtime/tool.rs) special-cases LoadSkill/web
tools/PythonRepl BY NAME before `ToolRegistry::execute`. LoadSkill/PythonRepl are "runtime tools" — declaration, NO
executor (`register_runtime_tool`). **This is the exact seam for `stage.*`: runtime tools intercepted by name.**

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-daemon/src/types.rs | RpcMethod + parse | 44-122 | Client RPC registry. subagent.list at 116; NO subagent.spawn. Add Stage* if exposing as client RPCs (recommended for UI/tests). |
| rust/crates/agent-daemon/src/main.rs | dispatch match | 255-293 | RpcMethod→handler. SubagentList→subagent_list at 290. |
| rust/crates/agent-daemon/src/main.rs | tools_list | 296-321 | tools.list RPC; stage.* appear automatically once registered. |
| rust/crates/agent-tools/src/registry.rs | FirstPartyToolExtension::register / register_runtime_tool | 335-376 | DECLARE model-facing stage.* here. register_runtime_tool adds declaration, no executor, both providers. |
| rust/crates/agent-tools/src/registry.rs | ToolRegistry::execute / canonical_tool_name_for_provider | 248-268 | Fallthrough for tools WITH executors. stage.* (no executor) must be intercepted before here. |
| rust/crates/agent-daemon/src/runtime/tool.rs | run_tool_turn | 58-90 | THE dispatch seam. if/else by name: LoadSkill(58), web(66), PythonRepl(75) before state.tools.execute(78). Add stage arm. |
| rust/crates/agent-daemon/src/provider_runtime/prompt.rs | tool_specs / prompt_context | 59-79 | ProviderTool→ToolSpec for prompt+wire. stage.* flow automatically. |
| rust/crates/agent-daemon/src/subagents.rs | spawn_subagent / subagent_spawn_from_active_parent / SubagentSpawnRequest | 69-376 | The engine stage.start_* reuses. Fork at 264-273 (skip for full). sources/initial_context/fork_context (147-167,625-668) = legacy spec drops. |
| rust/crates/agent-daemon/src/subagents.rs | subagent_list / require_known_subagent | 20-67, 541-559 | Pattern for stage.status listing + per-parent ownership guard. |
| rust/crates/agent-daemon/src/repl.rs | subagents_steer_host / spawn host wrapper | 434-462, 682-712 | Steer path + spawn params. RO steer/interrupt must be REJECTED by subagent_type. |
| rust/crates/agent-store/src/postgres/session_links.rs | session_parent_id / list_child_session_ids | 35-44 | one-stage-per-parent guard + stage.status enumeration. |
| rust/crates/agent-store/src/postgres/sessions.rs | activity | 528 | SessionActivity (Running/Idle) for terminal detection. |

### build_seams
**Make stage.* model-facing** (runtime-tool pattern): add four `*_definition()` fns mirroring `python_repl_definition`
(registry.rs:306-326) with Appendix A schemas, register via register_runtime_tool in FirstPartyToolExtension::register:
```rust
register_runtime_tool(registry, "stage.start_full", "stage", stage_start_full_definition());
register_runtime_tool(registry, "stage.start_readonly_fanout", "stage", stage_start_readonly_fanout_definition());
register_runtime_tool(registry, "stage.status", "stage", stage_status_definition());
register_runtime_tool(registry, "stage.cancel", "stage", stage_cancel_definition());
```
Appendix A schemas (workflow-orchestration.md:630-653):
- start_full: in `{role,prompt,workflow?,label?}` req `[role,prompt]`; out `{stage_id, subagent_session_id}`
- start_readonly_fanout: in `{tasks:[{role,prompt}],workflow?,label?}` req `[tasks]`; out `{stage_id, subagent_session_ids:[]}`
- status: in `{stage_id}`; out `{stage_id, kind, status, subagents:[{id,status}], handoff_dir}`
- cancel: in `{stage_id}`; out `{cancelled:bool}`

**Dispatch:** extend run_tool_turn (tool.rs:58-90) BEFORE state.tools.execute:
`} else if is_stage_tool_name(&tool_call.tool_name) { run_stage_tool(&state, &session_id, &dispatch.config, &tool_call).await }`.
Add module exporting `is_stage_tool_name`/`run_stage_tool -> ToolResultMessage`, mirroring repl_tools.rs (14-60). Handler
parses args_json, runs guards, calls engine, returns ToolResultMessage::success/error. session_id IS the parent.
**Handler must NOT block** — return stage_id immediately; completion arrives via barrier→steer.

**Reuse spawn engine:** run_stage_tool builds the params subagent_spawn_from_active_parent consumes, calls spawn_subagent
per subagent, tagging stage_id + subagent_type. For full, skip fork (#1). Drop legacy sources/initial_context.

**Three guards** (in run_stage_tool, surfaced as ToolResultMessage::error):
- Homogeneity + single-full: structural per-tool (start_full takes one scalar role/prompt; fanout forces read_only).
- One-stage-per-parent: query `parent_has_running_stage(parent_session_id)`, reject `stage_already_running` if any
  stage row for this parent is running.
- Reject RO steer/interrupt: in subagents_steer_host/interrupt_host (repl.rs:682-712) + any stage steer path, reject if
  child subagent_type == read_only.

**Error surfacing:** model path → ToolResultMessage::error (repl_tools.rs:26-39). If also client RPCs → RpcError
(types.rs:131-139). Recommend core logic returns Result<Value,RpcError>, run_stage_tool adapts → ToolResultMessage::error.

### gotchas
- subagent.spawn is NOT in RpcMethod and NOT a model-facing tool — reached ONLY via PythonRepl. Build stage.* as
  ToolRegistry runtime tools (LoadSkill/PythonRepl pattern), not only as RpcMethod.
- Two parallel surfaces; tools.list reflects only ToolRegistry. Decide deliberately if stage.* also needs client RPC
  entries (likely yes for UI run board + deterministic tests).
- register_runtime_tool gives NO executor; ToolRegistry::execute returns UnknownTool if reached — MUST intercept in
  run_tool_turn first.
- Interception matches the WIRE name (tool_call.tool_name); for stage.* wire==canonical (no alias), dots are fine.
- Registry sorts by name (registry.rs:274); tests `provider_tools_use_provider_facing_names` (516) and
  `definitions_for_provider_expose_only_that_provider` (476) assert EXACT tool-name lists and WILL break — update them.
- Completion result is NOT the tool result — stage.start_* returns immediately; outcome arrives async as a Steer.
- spawn_subagent unconditionally forks (264-273); full must skip — in subagents.rs, but tool must pass subagent_type.

### spec-drift
- Spec says subagent.spawn "is a regular daemon RPC tool the model calls" — FALSE; it's a REPL host fn only. Model-facing
  registry the spec doesn't name = rust/crates/agent-tools/src/registry.rs.
- All spec paths need rust/crates/ prefix.

---

## 1.8 transcript-render

Rendering a child transcript to greppable markdown for the handoff writer. Durable transcript is in Postgres via
`PgStore::active_branch(session_id) -> HistoryTree` (transcript.rs:328), `TranscriptEntryBodyMode::Ui` (the spec's "UI
body mode"). Each `TranscriptEntryRecord.item` is a `TranscriptItem` enum carrying UserMessage, AssistantMessage,
ToolCallStarted, ToolResult, turn markers, CompactionSummary. The ONLY render example, `parent_context_block` +
`transcript_item_context_line` (repl.rs:580/607), DROPS tool calls/results (`_ => String::new()`) — structural template
ONLY. The handoff writer must write a NEW exhaustive renderer.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-store/src/postgres/transcript.rs | PgStore::active_branch | 328-342 | PRIMARY READ: HistoryTree{entries}, Ui body mode. Durable — works after RO snapshot GC. |
| rust/crates/agent-store/src/lib.rs | HistoryTree / TranscriptEntryRecord | 561-566, 465-474 | entries ordered branch; .item, .timestamp_ms, .sequence. |
| rust/crates/agent-vocab/src/transcript_item.rs | TranscriptItem | 15-31 | Match while rendering: UserMessage, AssistantMessage, ToolCallStarted{tool_call}, ToolResult, TurnStarted/Finished, CompactionSummary. |
| rust/crates/agent-vocab/src/message.rs | AssistantMessage / AssistantItem / ToolCall | 90-210 | .text()(103), .tool_calls()(96)→ToolCall{id,tool_name,args_json}. args_value()(207) parses to Value. |
| rust/crates/agent-vocab/src/message.rs | ToolResultMessage / ToolResultStatus | 233-293 | {tool_call_id,tool_name,output:String,status:Success|Error|Interrupted|Crashed}. |
| rust/crates/agent-vocab/src/message.rs | UserMessage / ContentBlock | 8-75 | content Vec<ContentBlock> (Text|Image); [image] for images. |
| rust/crates/agent-daemon/src/repl.rs | parent_context_block / transcript_item_context_line | 580-639 | TEMPLATE to mirror (NOT reuse): drops tool calls/results, truncates at PARENT_CONTEXT_MAX_CHARS. |
| rust/crates/agent-daemon/src/repl.rs | latest_assistant_text | 568-578 | Final-message extractor concept (over TurnCardRecord). |
| rust/crates/agent-daemon/src/main.rs | load_session_config / SessionConfig.outer_cwd | 819-824 | Get parent outer_cwd to build handoff path. |

### build_seams
The handoff writer (#5, Phase 3) runs inside the barrier after all stage subagents terminal. For each child, write two
files under `<parent.outer_cwd>/.pi-handoff/<stage_id>/<subagent>/`. All reads from durable Postgres via state.repo.

**transcript.md** — `let history = state.repo.active_branch(child_session_id).await?;` then walk `history.entries` with
an EXHAUSTIVE match (do NOT call transcript_item_context_line):
- UserMessage → `## User` + text blocks (`[image]` for images).
- AssistantMessage → `## Assistant` + `m.text()`; tool calls via `m.tool_calls()`.
- ToolCallStarted{tool_call} → `### Tool call: {tool_name}` + fenced json of `args_value()` (raw args_json on parse err).
- ToolResult(r) → `### Tool result: {tool_name} [{status}]` + fenced `r.output`. ALWAYS render Error/Interrupted/Crashed.
- CompactionSummary(s) → `## Compaction summary` + s.summary.
- TurnStarted/Finished → skip or thin `---`.
Use stable greppable headings. Do NOT truncate.
**DEDUP:** ToolCall appears BOTH in AssistantMessage.items AND as a separate ToolCallStarted entry — pick ONE source
(inspect a real Ui-mode branch first) or each renders twice.

**final_message.md** — reverse-scan the same history.entries:
```rust
let final_message = history.entries.iter().rev()
    .find_map(|e| match &e.item {
        TranscriptItem::AssistantMessage(m) => { let t=m.text(); (!t.trim().is_empty()).then_some(t) }
        _ => None,
    }).unwrap_or_default();
```

**Path:** `state.repo.load_session_config(parent_session_id).await?.outer_cwd`, then
`Path::new(&outer_cwd).join(".pi-handoff").join(stage_id).join(subagent_id)`; create_dir_all; write the two .md files.
Handoff dir rooted at PARENT's outer_cwd. **Factoring:** pure free fns `render_transcript_markdown(&HistoryTree)->String`
and `extract_final_message(&HistoryTree)->String` (unit-testable with synthetic records).

### gotchas
- transcript_item_context_line DROPS ToolCallStarted/ToolResult — write a new exhaustive match.
- Double-render tool calls: inspect Ui projection, pick one source.
- Do NOT truncate (parent_context_block caps at PARENT_CONTEXT_MAX_CHARS for model context; handoff must be complete).
- Ui body_mode → provider_replay empty; do NOT switch to Full mode.
- Handoff root is PARENT's outer_cwd, not the child snapshot — survives RO GC.
- ToolResultStatus 4 variants; always emit Error/Interrupted/Crashed for complete failed-subagent transcripts.
- args_value() returns Result — fall back to raw args_json, don't unwrap.
- parent_context_block does acquire+recover_if_needed before reading. Barrier runs after terminal, but confirm the
  child's tail is recovered to a turn boundary or a crashed tail misses its final assistant message.

### spec-drift
- Spec frames parent_context_block as reusable — it is NOT (omits tool calls/results); structural example only.

---

## 1.9 skills-prompt

Skills = `SKILL.md` (frontmatter name/description + body) scanned under specific dirs. TWO loading paths, NOT unified.
**(1) PROMPT INDEX:** `prompt_context` → `load_prompt_skills` → `load_skills_for_session_workspaces` scans ONLY
`$HOME/.agents/skills/*/SKILL.md` (global) + `<outer_cwd>/<workspace_dir>/.agents/skills/*/SKILL.md` (tagged) →
`PromptContext.skills` → `skills_index_xml` → `{{ skills.index }}` in PI.md. **(2) PACKAGED-ROLE:** `resolve_skill_role`
falls back to `load_packaged_role_skills` → `load_global_skills_from_dir(prompt_root.join("subagent-roles"))` ONLY when no
workspace skill matches. `subagent-roles/*` are DELIBERATELY EXCLUDED from the index (test prompt.rs:367) and from
LoadSkill. `prompt_root` computed once at startup by `find_prompt_root` (main.rs:107) walking ancestors for PI.md =
repo root `/home/schwinns/pi-relay-wf`.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-daemon/src/provider_runtime/skills.rs | load_packaged_role_skills / packaged_role / resolve_skill_role | 107-161 | Packaged-role loader. load_packaged_role_skills(153-155) = scan call site to add workflows next to. |
| rust/crates/agent-daemon/src/provider_runtime/skills.rs | load_skill_output_with_home | 35-83 | LoadSkill resolution. NO packaged/workflows fallback today. Must extend for LoadSkill to load workflow-*. |
| rust/crates/agent-daemon/src/provider_runtime/prompt.rs | load_prompt_skills / load_skills_for_session_workspaces / load_global_skills_from_dir | 81-83, 105-116 | Prompt-index loader. load_prompt_skills(81-83)=call site to extend for index visibility. |
| rust/crates/agent-daemon/src/provider_runtime/prompt.rs | prompt_context | 39-62 | skills: load_prompt_skills(config) at 60. Does NOT receive prompt_root today (gotcha). |
| rust/crates/agent-daemon/src/provider_runtime/prompt.rs | subagent_role_defaults_are_not_prompt_skills (test) | 367-394 | Asserts roles NOT in index. Add analogous test asserting workflows ARE. |
| rust/crates/agent-prompt/src/lib.rs | skills_index_xml | 236-264 | Renders <available_skills> XML; workflow-* render as untagged global. |
| rust/crates/agent-prompt/src/lib.rs | Skill::global / Skill::workspace / struct Skill | 36-80 | {workspace:Option,name,description,file_path}. |
| rust/crates/agent-daemon/src/main.rs | find_prompt_root | 66, 107-117 | Computes prompt_root = /home/schwinns/pi-relay-wf. Stored in AppState.prompt_root (state.rs:36). |
| /home/schwinns/pi-relay-wf/PI.md | ## Subagent delegation | 40-64 | REPLACE wholesale with Appendix B (spec 660-697). |
| /home/schwinns/pi-relay-wf/PI.md | ## Skills ({{ skills.index }}) | 66-79 | Workflow-* surface here once index wired. |
| /home/schwinns/pi-relay-wf/subagent-roles/ | explore..worker | n/a | Packaged roles at repo root. New workflows/ sits alongside. |
| /home/schwinns/pi-relay-wf/rust/docs/plans/workflow-skills/ | workflow-{explore,implement-review,implement-review-test,kubernetes-e2e} | n/a | Source drafts to copy to <prompt_root>/workflows/<name>/SKILL.md. |

### build_seams
**1. Install (data move):** copy each draft to `/home/schwinns/pi-relay-wf/workflows/<name>/SKILL.md` (repo root, NOT
under rust/). Frontmatter already valid.

**2. Wire loader — TWO call sites (README understates as one):**
- Role-spawn fallback (the README's single line) in load_packaged_role_skills (skills.rs:153): add
  `load_global_skills_from_dir(&prompt_root.join("workflows"))`. This alone does NOT make them index-visible/LoadSkill-able.
- **(a) Index:** extend load_prompt_skills (prompt.rs:81), thread prompt_root (available via AppState.prompt_root):
  ```rust
  fn load_prompt_skills(prompt_root: &Path, config: &SessionConfig) -> Vec<Skill> {
      let mut skills = load_skills_for_session_workspaces(&PathBuf::from(&config.outer_cwd), &config.workspaces);
      skills.extend(load_global_skills_from_dir(&prompt_root.join("workflows")));
      skills
  }
  ```
  call at line 60 as `load_prompt_skills(&state.prompt_root, config)`.
- **(b) LoadSkill:** extend load_skill_output_with_home (skills.rs:62) — when no workspace/home skill matches and
  `workspace` arg is None, also search `load_global_skills_from_dir(prompt_root.join("workflows"))`. This fn does NOT
  receive prompt_root today — thread it from the LoadSkill dispatch call site (search runtime dispatch for
  `load_skill_result`).
Keep `subagent-roles` index-hidden (only add `workflows/`).

**3. Rewrite PI.md** lines 40-64 (entire `## Subagent delegation`, ending before `{% if skills.index %}` at 66) verbatim
with Appendix B body (spec 665-696, drop fences). Leave the `## Skills` block intact.

**4. Tests:** mirror subagent_role_defaults_are_not_prompt_skills but asserting workflows/<name> DOES appear; LoadSkill
test follows load_skill_result_loads_content_once (skills.rs:191).

### gotchas
- README/spec line 284 claim a SINGLE loader line makes workflows index-visible + LoadSkill-able — FALSE. The single
  line only touches role-spawn fallback. Index (prompt.rs:81) and LoadSkill (skills.rs:35) have NO packaged fallback.
- load_skill_output_with_home AND load_prompt_skills do NOT receive prompt_root — both must be threaded it.
- workflows/ must live at the actual repo root /home/schwinns/pi-relay-wf/workflows/, NOT under rust/. Drafts at
  rust/docs/plans/workflow-skills/ are deliberately outside any scanned dir.
- `workflow-` prefix avoids shadowing role skills (explore vs workflow-explore). Keep the prefix.
- load_global_skills_from_dir scans one level: <dir>/<child>/SKILL.md. Flat workflows/SKILL.md ignored.
- Workflow skills render as GLOBAL (untagged) skills.
- Appendix B references stage.* tools — do NOT apply PI.md rewrite before Phases 1-3 land (spec line 662).

### spec-drift
- Spec line 477 locates resolve_skill_role in agent-daemon/src/subagents.rs — it's actually in
  provider_runtime/skills.rs:107 (MOVED/misattributed).
- README/spec single-loader-line claim is FALSE (see gotchas).
- workflows/ dir does not yet exist at repo root.

---

## 1.10 harness-and-creds

The "dev harness" is NOT a fake provider — it's a per-session flag + two RPCs. A session opts in via metadata
`{"harness": true}` at session.start. When the runtime would dispatch a model action, `spawn_model_dispatch` checks
`session_uses_harness(&config)` and if true returns WITHOUT a provider call, leaving the action `pending`. The test
client then calls `harness.model.complete` (synthetic assistant message) or `harness.model.fail`. NO testcontainers — a
real Postgres DB is created per test from `PI_RELAY_TEST_DATABASE_URL`. **For the real-model e2e: NO connections table,
NO auth/login RPC. `Credentials::load()` is called FRESH per request from the daemon PROCESS env + filesystem** (verified
auth.rs:21-35). A fresh empty DB needs NOTHING seeded for auth.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| rust/crates/agent-daemon/src/runtime/mod.rs | session_uses_harness | 771-777 | Reads config.metadata["harness"] bool, default false. The single gate. |
| rust/crates/agent-daemon/src/runtime/dispatch.rs | spawn_model_dispatch | 28-57 | Harness short-circuit at 34 (`if session_uses_harness {return;}`) leaves action pending. |
| rust/crates/agent-daemon/src/main.rs | harness_model_complete / harness_model_fail | 1662-1760 | The two harness RPC handlers. complete: session_id, action_row_id, assistant. |
| rust/crates/agent-daemon/src/types.rs | RpcMethod::HarnessModelComplete / Fail | 78-79, 117-118 | "harness.model.complete" / "harness.model.fail". |
| rust/crates/agent-store/src/postgres/actions.rs | load_harness_model_action / claim_pending_model_action | 69-88, 267 | Store fns the harness RPCs use. |
| rust/crates/agent-daemon/src/auth.rs | Credentials::load | 21-35 | THE credential source. No DB, no RPC. (Verified.) |
| rust/crates/agent-daemon/src/auth.rs | read_claude_code_config_api_key_from_home / read_codex_auth | 58-125 | Anthropic key must start sk-ant-; checks ~/.claude/config.json then ~/.claude.json. Codex token at ~/.codex/auth.json /tokens/access_token. (Verified.) |
| rust/crates/agent-daemon/src/auth.rs | refresh_codex_credentials | 141-189 | On Codex 401, refresh via OAuth refresh_token, rewrite auth.json. |
| rust/crates/agent-daemon/src/provider_runtime/connections.rs | ProviderConnectionRegistry / provider_for_config | 41-157 | In-memory per-session cache (HashMap), NOT a DB connections table. OpenAi needs codex_access_token; Anthropic needs anthropic_api_key. |
| rust/crates/agent-daemon/src/provider_runtime/requests.rs | complete_model_request | 65-74 | Real dispatch: Credentials::load() then provider_for_config then complete_with_auth_retry (creds re-read every time). |
| rust/crates/agent-daemon/src/config.rs | Config::from_env_and_args | 11-46 | --database-url (or DATABASE_URL, required), --bind (or PI_AGENTD_BIND, default 127.0.0.1:8787). NO API-key flag. |
| rust/crates/agent-daemon/src/main.rs | main | 55-82 | Boot: Config → connect → migrate → ProviderConnectionRegistry::new → TcpListener::bind. WebSocket RPC server. |
| rust/crates/agent-daemon/src/model_metadata.rs | context_window / supported_reasoning_efforts | 6-72 | Model IDs (below). |
| rust/crates/agent-store/src/postgres/sessions_tests.rs | test_store / TestDb | 14-63 | Integration-test convention: gate PI_RELAY_TEST_DATABASE_URL, create unique DB, migrate, drop. |
| rust/crates/agent-store/src/postgres/schema.rs | migrate | 21-125 | Idempotent create-table-if-not-exists. Fresh DB fully provisioned by store.migrate(); no auth tables. |
| rust/crates/agent-vocab/src/provider.rs | ProviderConfig | 79-85 | session.start: {kind:ProviderKind(OpenAi|Claude), model, reasoning_effort, max_tokens}. No default model. |

**Model IDs (model_metadata.rs:9-18):** OpenAI `gpt-5.5, gpt-5.1, gpt-5.1-codex-max, gpt-5.1-codex-mini, gpt-5.2,
gpt-5.2-codex, gpt-5.3-codex`. Claude `claude-opus-4-8` (1M), `claude-opus-4-7` (1M), `claude-sonnet-4-5` (200k).

### build_seams
**Deterministic harness tests:** session.start with `metadata:{"harness":true}` → runtime stops at spawn_model_dispatch:34
leaving action pending → discover action_row_id via events/transcript/actions → `harness.model.complete` with an
`assistant` carrying a tool-call for `stage.start_full`/`subagent.spawn` (parsed by parse_assistant_message, main.rs:1668)
→ scripting a sequence drives the barrier/handoff/steer state machine deterministically. Phase 2 stage.* tools must be
dispatchable from a harness-supplied assistant tool-call the same way subagent.spawn is.

**Integration scaffolding:** reuse test_store() (sessions_tests.rs:34-63): gate PI_RELAY_TEST_DATABASE_URL,
create db, migrate, drop. New `stages` table in schema::migrate is picked up automatically. Run: `cargo test` (workspace
at /home/schwinns/pi-relay-wf/rust). Postgres tests SKIP (not fail) when env unset.

**Real-model e2e — see Section 4.**

### gotchas
- Harness is a metadata flag + two RPCs, NOT a mock provider type. No FakeProvider struct exists.
- No CLI/env to flip the whole daemon into harness mode — strictly per-session metadata.
- Credentials::load() runs FRESH on EVERY model request — process env + HOME only. No DB connections table.
- ProviderConnectionRegistry is an in-memory cache, NOT the "connections table".
- Anthropic key rejected unless it starts sk-ant- (silently dropped otherwise → "ANTHROPIC_API_KEY not found").
- Postgres tests require PI_RELAY_TEST_DATABASE_URL pointing at a role that can CREATE/DROP DATABASE; tests SILENTLY SKIP
  when unset — confirm exported in e2e/CI or workflow tests pass while doing nothing.
- No testcontainers/pg-embed anywhere.
- Daemon is a WebSocket RPC server (ws://<bind>), not HTTP/stdio — test client speaks WS RPC frames.

### spec-drift
- Task hypothesized a "connections table seeded via auth/login RPC" — does NOT exist; auth is purely env+HOME.

---

## 1.11 web-ui

Single React app (`packages/web/src/App.tsx`) over one WebSocket via `AgentRpcClient` (rpc.ts) wrapped by
`AgentApiClient` (agentApi.ts). All RPCs `request<T>(method,params)`; all live updates arrive as
`EventFrame{event_id,event,session_id,data}` through one `onEvent`. Today subagents are a flat read-only list:
`subagent.list → {parent_session_id, subagents:[{child_session_id, activity}]}` rendered by `SubagentsSection` in
`Inspector` (panels.tsx). Polls subagent.list every 2s while parent running. **No stage notion, no grouping, no handoff
links, no per-subagent controls.** A run board = a NEW stage-grouped view (its own stage.* RPC family + events) beside
the per-session inspector.

### key_locations
| path | symbol | lines | role |
|---|---|---|---|
| packages/web/src/rpc.ts | AgentRpcClient.request / onEvent / handleMessage | 34-191 | WS transport. New stage.* RPCs need no transport change. |
| packages/web/src/agentApi.ts | AgentApi interface + AgentApiClient | 31-68,310-314,419-421 | Add stageStart*/stageStatus/stageCancel mirroring listSubagents(310) + interrupt(419). |
| packages/web/src/types.ts | SubagentListItem / Result / EventFrame / SessionSummary | 110-125,31-45 | Wire types. Add Stage/StageSubagent/StageStatus + stage_id/subagent_type fields. EventFrame.data is Record so payloads need no schema change. |
| packages/web/src/panels.tsx | SubagentsSection + Inspector | 60-116,624-734 | Existing subagent rendering. RunBoard replaces/extends SubagentsSection or sibling section grouped by stage. |
| packages/web/src/App.tsx | subagentsQuery / subagentIds / subagentSummariesQuery | 427-445 | Data fetch + 2s poll. Clone for stage.status. |
| packages/web/src/App.tsx | handleSessionEvent | 884-907 | subagent.* handling; refreshList invalidates subagents query(887). Wire stage.* here. |
| packages/web/src/App.tsx | event subscription effect (desiredSessionIds) | 1055-1100 | Subscribes parent + each child. Board's subagent ids must feed this set. |
| packages/web/src/App.tsx | Inspector render site | 2040-2054 | Where subagents/summaries pass into Inspector. Thread run board props here. |
| packages/web/src/App.tsx | stopActiveTurn / submitComposer | 1522-1536,1687-1712 | stopActiveTurn=api.interrupt (basis for cancel stage / interrupt full). submitComposer always follow_up. |
| packages/web/src/sessionEvents.ts | SESSION_LIST_REFRESH_EVENTS / KNOWN_SESSION_EVENTS / refreshPlanForEvent | 8-68 | Event allowlists. Add stage.* names or they force redundant syncSelected. |
| packages/web/src/queryKeys.ts | queryKeys.subagents / subagentSummaries | 9-11 | Add queryKeys.stages / stage. |
| packages/web/src/sessionList.ts | sessionDisplayActivity / roleLabel | 44-50 | Activity→idle/running for status rails. Role from snapshot.metadata.role_name. |
| rust/crates/agent-daemon/src/subagents.rs | subagent_list | 20-65 | Backend producing current SubagentListResult. stage.* net-new alongside. |
| rust/crates/agent-daemon/src/types.rs | RpcMethod::parse | 82-121 | NO stage.* method today (only subagent.list). |

### build_seams
Add the run board as a section inside Inspector (panels.tsx:624) or a new sibling, reusing subagent data-flow.
**1. Types** (types.ts after 125): StageKind=`"full"|"readonly_fanout"`, StageStatus=`"running"|"done"|
"done_with_failures"|"cancelled"|"failed"`, SubagentType=`"full"|"read_only"`, StageSubagent
{id,role?,status,suggested_next?,final_message_path?,transcript_path?,activity?}, Stage
{stage_id,parent_session_id,workflow?,label?,kind,status,handoff_dir?,subagents[],created_at?,updated_at?},
StageListResult{parent_session_id,stages[]}. Extend SessionSummary/Snapshot with stage_id?, subagent_type?.
**2. Facade** (agentApi.ts): listStages/getStage("stage.status")/startFullStage/startReadonlyFanout/cancelStage. Spec
names exactly stage.start_full/start_readonly_fanout/status/cancel; **there is NO stage.list — recommend backend add a
per-parent listing for the board** (spec mandates only per-id stage.status).
**3. Query+poll** (App.tsx, mirror 427-445): stagesQuery with refetchInterval 2_000 while parent running. Add
queryKeys.stages/stage.
**4. Event refresh** (sessionEvents.ts + App.tsx): add new stage event names to SESSION_LIST_REFRESH_EVENTS +
KNOWN_SESSION_EVENTS; in handleSessionEvent invalidate queryKeys.stages. **Completion steer is a normal user message in
the parent transcript** — existing transcript pipeline renders it; board just flips the stage row via the query refresh.
**5. Render+controls** (panels.tsx): RunBoard modeled on SubagentsSection; per subagent row link to child session; cancel
stage → api.cancelStage; steer full subagent → api.queueFollowUp BUT at STEER priority (new path); re-run → startFull/
fanout (confirm with backend whether re-run is fresh stage.start_* or turn.resume).
**6. Inspector wiring** (App.tsx:2040-2054 + panels.tsx:624): thread stages + callbacks; board subagent session_ids must
feed desiredSessionIds (App.tsx:1062-1064).

### gotchas
- Composer ALWAYS submits follow_up priority (submitComposer→queueFollowUp, only 'follow_up' literal at App.tsx:1382).
  "Steer the full subagent" needs a NEW steer-priority path — does not exist today.
- subagent.list returns ONLY {child_session_id, activity}; role/title/model fetched per-child. Prefer embedding in
  stage.status to avoid N round-trips per fan-out.
- NO stage.* RPC registered yet — coordinate exact param/result JSON with Phase 2 builder.
- **Handoff files live on the daemon host filesystem; NO RPC reads arbitrary files from the web client today.** "Links to
  handoff files" cannot be live links unless the builder adds a file-read RPC; else show paths as text. BACKEND DEPENDENCY.
- Completion notification is a STEER into the parent transcript, not a special event — don't build a separate renderer.
- Keep the 2s poll fallback while parent running (survive missed events).
- transcript.tsx has zero subagent/stage awareness — run board is purely Inspector/panels.
- New stage event names MUST go in KNOWN_SESSION_EVENTS or they force redundant active-branch syncs.

### spec-drift
- Spec's stage.status is per-id only — a per-parent stage list (to populate the board) is unspecified; Phase 2 backend
  should add it.

---

# 2. Net-new pieces #1–#7 → seam mapping

| # | Net-new item | Exact seam(s) / files |
|---|---|---|
| **#1** | Skip the workspace fork for `full` subagents | `subagents.rs:264-273` (the ONLY fork call) — branch on new `subagent_type`: ReadOnly keeps `fork_session_from_parent`, Full reuses `parent_config.outer_cwd/workspaces`. Add `subagent_type` enum to `SubagentSpawnRequest`/`Params` (subagents.rs:92-184). VERIFY `ensure_session` (session_start.rs:192) is no-op against parent cwd. Full teardown must NOT delete parent workspace (cleanup_failed_spawn:561, remove_session_dir:260). |
| **#2** | Subvolume-aware destroy path | New `destroy_workspace_tree(root)` in `workspaces/instantiate.rs` (next to try_btrfs_subvolume_create:107, depth-first + remove_dir_all fallback) + public `destroy_session_workspaces(session_id)` on WorkspaceManager (`workspaces/mod.rs`, next to remove_session_dir:260). Redirect remove_session_dir to delegate. Call at RO terminal lifecycle. Exclude `.pi-handoff` from the fork (materialize_tree_from_source_exact:29). |
| **#3** | Durable `stages` table + `sessions.stage_id`/`subagent_type` columns + stages repo | `schema.rs:20-128` SCHEMA_SQL (create stages BEFORE alters), text_enums in `lib.rs:58-92`, new `postgres/stages.rs` (register `mod stages;` mod.rs:1-16): create_stage, list_stage_subagents, finish_stage CAS, sweep_running_stages. Set values via start_session_outputs_with_parent (sessions.rs:252) or a setter mirroring set_session_parent. Update 3 explicit reads (list_sessions:428, load_session_config:500, session_snapshot snapshots.rs:24) only if surfaced. |
| **#4** | `stage.*` model-facing tools + dispatch + 3 guards | Declare in `agent-tools/src/registry.rs:335-376` (register_runtime_tool, 4 `*_definition()` fns). Intercept by name in `runtime/tool.rs:58-90` (is_stage_tool_name/run_stage_tool). Engine reuses spawn_subagent (subagents.rs:191). Guards in run_stage_tool: homogeneity/single-full (structural), one-stage-per-parent (`parent_has_running_stage`), reject RO steer/interrupt (subagent_type). Update broken registry tests (476,516). Optionally register in RpcMethod (types.rs:82-121) for UI/tests. |
| **#5** | Handoff writer (transcript.md + final_message.md + index.json) | New module (e.g. `agent-daemon/src/handoff.rs`): `render_transcript_markdown(&HistoryTree)` + `extract_final_message(&HistoryTree)`, reading `active_branch(child)` (transcript.rs:328, Ui mode). Path `<parent.outer_cwd>/.pi-handoff/<stage_id>/<subagent>/` via load_session_config(parent).outer_cwd. Run inside the barrier (Section #6). index.json carries role/status/suggested_next/paths per subagent. |
| **#6** | Barrier → steer hook in subagent-lifecycle path | Stage runner `on_subagent_terminal`/`try_stage_barrier` on SessionDriver, called at the three try_subagent_parent_idle_event call sites (`runtime/mod.rs:211,359,420`). Barrier: lock stage row (stages.rs CAS, FOR UPDATE) → all-subagents-terminal predicate → finish_stage CAS (running→done|done_with_failures from TurnOutcome) → render handoff (#5) → ONE `enqueue_session_input(&self.state, Steer)` to parent (main.rs:924). SUPPRESS per-child idle notification for stage members. Wire stage_runner into AppState (state.rs:30). |
| **#7** | Workflow skills installed + discoverable + PI.md rewrite | Copy 4 drafts to `/home/schwinns/pi-relay-wf/workflows/<name>/SKILL.md`. THREE loader edits: load_packaged_role_skills (skills.rs:153, role fallback), load_prompt_skills (prompt.rs:81, index — thread prompt_root), load_skill_output_with_home (skills.rs:62, LoadSkill — thread prompt_root). Rewrite PI.md:40-64 with Appendix B (AFTER Phases 1-3). |

---

# 3. Phase build order with file touch-list

**Phase 1 — subagent_type + skip-fork + destroy path (foundation)**
- `agent-store/src/postgres/schema.rs` — add `alter table sessions add column if not exists subagent_type text null` (stages table can wait to Phase 2, but ordering is fine to add together).
- `agent-store/src/lib.rs` — `SubagentType` text_enum.
- `agent-daemon/src/subagents.rs` — add subagent_type to SubagentSpawnRequest/Params (92-184); branch the fork at 264-273; full-safe teardown.
- `agent-daemon/src/session_start.rs` — verify/thread subagent_type into start_prepared_session; verify ensure_session no-op for full.
- `agent-store/src/postgres/sessions.rs` — bind subagent_type in start_session_outputs_with_parent (252) (or setter).
- `agent-daemon/src/workspaces/instantiate.rs` — `destroy_workspace_tree`.
- `agent-daemon/src/workspaces/mod.rs` — `destroy_session_workspaces`; redirect remove_session_dir; exclude `.pi-handoff` from fork (mod.rs:187 / materialize path).
- Tests: per-test fresh DB (sessions_tests.rs pattern).

**Phase 2 — stages table + repo + stage.* tools**
- `agent-store/src/postgres/schema.rs` — stages table + stages_parent_created_idx + `alter ... stage_id` (after stages).
- `agent-store/src/lib.rs` — StageKind, StageStatus text_enums.
- `agent-store/src/postgres/mod.rs` — `mod stages;`.
- `agent-store/src/postgres/stages.rs` (new) — create_stage, list_stage_subagents, finish_stage CAS, sweep_running_stages, parent_has_running_stage.
- `agent-tools/src/registry.rs` — 4 `*_definition()` + register_runtime_tool; FIX tests 476/516.
- `agent-daemon/src/runtime/tool.rs` — is_stage_tool_name/run_stage_tool arm (58-90).
- `agent-daemon/src/` (new stages module) — run_stage_tool + 3 guards, reusing spawn_subagent.
- Optionally `agent-daemon/src/types.rs` + `main.rs:255-293` — RpcMethod::Stage* for UI/tests.
- `agent-daemon/src/repl.rs` — reject RO steer/interrupt (682-712).

**Phase 3 — barrier → steer + handoff writer**
- `agent-daemon/src/state.rs` — stage_runner handle in AppState.
- `agent-daemon/src/runtime/mod.rs` — try_stage_barrier at 211/359/420; suppress per-child idle for stage members.
- `agent-daemon/src/handoff.rs` (new) — render_transcript_markdown, extract_final_message, index.json.
- `agent-daemon/src/main.rs:59` — sweep_running_stages on boot.
- Deterministic harness tests scripting harness.model.complete sequences.

**Phase 4 — workflow skills + PI.md**
- Copy drafts → `/home/schwinns/pi-relay-wf/workflows/<name>/SKILL.md`.
- `agent-daemon/src/provider_runtime/skills.rs` — load_packaged_role_skills (153) + load_skill_output_with_home (62, thread prompt_root).
- `agent-daemon/src/provider_runtime/prompt.rs` — load_prompt_skills (81, thread prompt_root) + call site (60).
- `PI.md` — rewrite lines 40-64 with Appendix B (ONLY after Phases 1-3).
- Tests: index visibility + LoadSkill.

**Phase 5 — web run board**
- `packages/web/src/types.ts` — Stage types + stage_id/subagent_type on SessionSummary/Snapshot.
- `packages/web/src/agentApi.ts` — stage facade methods.
- `packages/web/src/queryKeys.ts` — stages/stage keys.
- `packages/web/src/App.tsx` — stagesQuery + poll (427); handleSessionEvent stage.* (884); desiredSessionIds (1062); Inspector wiring (2040); steer-priority path.
- `packages/web/src/panels.tsx` — RunBoard + Inspector mount (624,692).
- `packages/web/src/sessionEvents.ts` — stage.* event names.
- BACKEND DEPENDENCY: per-parent stage.list RPC + a file-read RPC for handoff links (else paths as text).

---

# 4. Provider credentials for the fresh-DB real-model e2e (riskiest unknown — RESOLVED)

**There is nothing to seed in the database for auth.** Verified directly against `auth.rs:21-35`: `Credentials::load()`
runs FRESH on every model request and reads creds ENTIRELY from the daemon PROCESS's environment + HOME filesystem. There
is NO connections table, NO auth/login RPC, NO `--api-key` CLI flag (config.rs:11-46 has only `--database-url`/`--bind`).
`ProviderConnectionRegistry` (connections.rs) is an in-memory cache, not a DB table.

**To let a brand-new daemon (fresh empty DB) make real GPT + Claude calls:**
1. **Launch:** `pi-agentd --database-url postgres://.../<fresh_db> [--bind 127.0.0.1:<port>]`. `store.migrate()` fully
   provisions the empty DB (idempotent create-table-if-not-exists); no auth tables involved. Daemon is a WebSocket RPC
   server (ws://<bind>).
2. **GPT / OpenAI:** ensure the daemon process sees a Codex ChatGPT OAuth token — either `~/.codex/auth.json` with
   `/tokens/access_token` (present after `codex login`; the live machine has this), OR env `CODEX_ACCESS_TOKEN`.
   Optional `~/.codex/installation_id` and `/tokens/account_id` are read for headers. On a 401 the daemon auto-refreshes
   via `/tokens/refresh_token` (or `CODEX_REFRESH_TOKEN`) against `https://auth.openai.com/oauth/token` and rewrites
   auth.json — so a stale token self-heals if the refresh token is present.
3. **Claude / Anthropic:** set env `ANTHROPIC_API_KEY`, OR have `~/.claude/config.json` (then `~/.claude.json`) with
   `primaryApiKey` **starting with `sk-ant-`** (keys without that prefix are silently dropped → surfaces as
   "ANTHROPIC_API_KEY not found").
4. **Start the e2e session WITHOUT the harness flag** (omit `metadata.harness` or set false) so real dispatch runs. Pick
   model IDs from model_metadata.rs — e.g. OpenAI `gpt-5.2-codex`/`gpt-5.5`, Claude `claude-opus-4-8`/`claude-sonnet-4-5`.
   session.start provider = `{ "kind": "open_ai"|"claude", "model": "<id>" }` (verify exact ProviderKind wire casing at
   `agent-vocab/src/provider.rs:9` before hardcoding).

If a required cred is absent, the per-session provider handle constructor errors ("~/.codex ChatGPT token not found" /
"ANTHROPIC_API_KEY not found"), surfaced as a model error — that is the signal the host creds aren't set up. **NOTE for
CI:** Postgres integration tests SKIP silently when `PI_RELAY_TEST_DATABASE_URL` is unset (must point at a role that can
CREATE/DROP DATABASE) — confirm it is exported or the workflow tests pass while doing nothing.

---

# 5. Open risks / unverified

- **[#1, must verify]** `ensure_session` (workspaces/mod.rs near 130, called session_start.rs:192) must be a no-op /
  validate-only when a full subagent reuses the parent's existing outer_cwd. NOT yet confirmed it doesn't re-fork/wipe.
  Read it before implementing the full branch.
- **[#1, must verify]** Full-subagent teardown must skip parent workspace removal. cleanup_failed_spawn (561) and
  remove_session_dir (260) both remove_dir_all the session root — for full, the session root IS shared with the parent's.
  Confirm the spawn path establishes a distinct session_root for full or guard the destroy by subagent_type.
- **[#2]** Whether to exclude `.pi-handoff` from the fork or accept a stale child copy reclaimed on destroy — spec says
  handoff is "never forked"; pick exclusion to match intent. Exclusion point is materialize_tree_from_source_exact:29 /
  fork_session_from_parent:187.
- **[#5, must verify]** Tool-call double-render: ToolCall appears BOTH in AssistantMessage.items AND as a separate
  ToolCallStarted entry under Ui body mode. Inspect ONE real Ui-mode branch before finalizing transcript.md to pick a
  single source.
- **[#5, should verify]** Whether the child's tail is already recovered to a turn boundary when the barrier runs (it runs
  after terminal, but a crashed tail could miss its final assistant message). parent_context_block does acquire+recover
  first; confirm the barrier path does too or add a recover.
- **[#4 / web]** Exact stage.* param/result JSON shapes must be agreed between the Phase 2 backend builder and the web
  builder. Spec Appendix A is the contract.
- **[web]** NO RPC reads arbitrary host files from the web client. Handoff-file links require a NEW backend file-read RPC,
  or the board shows absolute paths as text. Unowned backend dependency.
- **[web]** Spec defines stage.status per-id only; a per-parent `stage.list` (needed to populate the board) is
  unspecified — Phase 2 backend should add it.
- **[creds, CI]** The real-model e2e depends on the daemon's HOME/env having live Codex + Anthropic creds; on a fresh CI
  machine these are absent. Confirm `codex login` has run (or CODEX_ACCESS_TOKEN/CODEX_REFRESH_TOKEN set) and
  ANTHROPIC_API_KEY/`primaryApiKey` (sk-ant-) is present. Stale Codex token self-heals ONLY if a refresh token exists.
- **[spec-drift, doc only]** Spec misattributes resolve_skill_role to subagents.rs (it's provider_runtime/skills.rs:107)
  and claims subagent.spawn is a daemon RPC tool (it's a REPL host fn only) and claims a single loader line makes
  workflows index-visible (needs three edits). These are documentation errors, not blockers.
