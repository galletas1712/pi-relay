# Legacy subagent retirement plan (Phase 6)

Consolidated from a full read of the live code on branch `wf/phase-6-retire-legacy`.
Goal: fully retire the legacy REPL-subagent orchestration surface while leaving the
live `stage.*` path (runtime tools + client RPCs + barrier/handoff/steer runner +
workflow skills) byte-for-byte unaffected.

Historical note: this Phase 6 plan predates the provider-visible delegation-tool
rename. The diagram below uses the old model-facing names
(`stage_start_full`, `stage_start_readonly_fanout`, `stage_status`,
`stage_cancel`) as historical labels. Current agents should use
`delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`, and
`cancel_delegation`; the `stage.*` names remain web/client RPCs.

Decided invariants (do not relitigate):
- KEEP the `PythonRepl` *scripting* tool (arbitrary Python exec) as an escape hatch.
- REMOVE the `subagents.*` REPL orchestration module (the Python bootstrap classes
  and the host-call handlers behind them).
- KEEP `spawn_subagent` core, `resolve_skill_role`, the subagent lifecycle events
  (`SubagentSpawned`/`SubagentRunning`/`SubagentIdle`), and workspace fork/destroy —
  `stage_tools` + the barrier reuse them.

```
                      MODEL-FACING                 CLIENT RPC
  stage_start_full ─┐                          stage.start_full ───┐
  stage_start_ro_fan ┼─ run_stage_tool ─┐      stage.start_ro_fan ──┤
  stage_status ──────┤                  │      stage.status ────────┼─ stage_tools::*
  stage_cancel ──────┘                  ├──────stage.cancel ────────┤   (KEEP)
                                        │      stage.list ──────────┤
  PythonRepl (scripting) ─ run_repl_tool┘      stage.read_handoff ──┘
        │                                      subagent.list ─ subagents::subagent_list (REMOVE)
        └─ ReplRegistry.execute ─ PythonRepl child ─ handle_host_call
                                                     │
                                  subagents.spawn/wait/call/list/read/steer/interrupt
                                                     │  (REMOVE — legacy orchestration)
                                                     └─ subagent_spawn_from_active_parent ─┐
                                                                                           ├─ spawn_subagent (KEEP)
  stage_tools::{start_full_core,start_readonly_fanout_core} ─ StageSubagentSpawn ──────────┘
```

The single load-bearing fact for the whole split: `spawn_subagent`
(`subagents.rs:225`) has exactly two callers — `stage_tools` (always with a
`stage_id`, the live path, KEEP) and `subagent_spawn_from_active_parent`
(`subagents.rs:69`, REPL-only, never sets `stage_id`, REMOVE). Everything that is
reachable *only* through the second caller is removable; everything `spawn_subagent`
itself needs is shared and must stay.

---

## 1. REMOVABLE

### 1a. REPL subagents orchestration module (`agent-daemon/src/repl.rs`)

The Python REPL *transport* (`ReplRegistry`, `PythonRepl`, `repl_exec`, the exec
protocol, `kill_all`) is KEEP — it backs the `PythonRepl` scripting tool. Only the
**subagent orchestration** built on top of it is removable:

- `repl.rs:18` — `use crate::subagents::{require_known_subagent, subagent_list, subagent_spawn_from_active_parent};` (delete; transport needs none of these).
- `repl.rs:22-23` — `SUBAGENT_POLL_INTERVAL_MS`, `PARENT_CONTEXT_MAX_CHARS` consts.
- `repl.rs:276-295` — `handle_host_call` dispatch. After removal there are **no**
  host calls left; delete the function and the `host_call` arm wiring in
  `PythonRepl::execute` (`repl.rs:155-189`) that calls it, so the REPL becomes a
  pure exec loop with no host bridge.
- `repl.rs:297-737` — all subagent host-call machinery:
  - structs `SubagentCallParams` (297), `SubagentCallSpec` (308), `SpawnedCall`
    (317), `SubagentWaitTarget` (323), `SubagentWaitParams` (329),
    `SubagentsListParams` (641).
  - host fns `subagents_spawn_host` (334), `subagents_wait_host` (355),
    `subagents_call` (394), `spawn_call` (416), `spawned_handle_value` (464),
    `wait_for_children_idle` (480), `call_result` (518), `latest_assistant_text`
    (568), `parent_context_block` (580), `transcript_item_context_line` (607),
    `subagents_list_host` (646), `subagents_read_host` (667),
    `reject_read_only_control` (685), `subagents_steer_host` (705),
    `subagents_interrupt_host` (728).
- `repl.rs:739-1018` — `PYTHON_REPL_BOOTSTRAP`: strip everything from `_host_call`
  through `sys.modules["subagents"] = _subagents_module` (lines ~765-949) and the
  `"subagents": subagents` global (line 971 / 949). KEEP `_jsonish`, `_exec_cell`,
  `_handle_exec`, the read/write control plumbing, and the main exec loop — that is
  the scripting REPL. Result: a bootstrap that execs cells and captures
  stdout/stderr/result, with no `subagents` symbol.
- `repl.rs:1069-1091` — the `import subagents` assertions inside
  `python_repl_preserves_state_and_captures_last_expression`; trim them so the test
  only covers state persistence + last-expression capture (KEEP that part).
- Imports to drop after the above: `agent_vocab::TurnOutcome`, `TranscriptItem`,
  `UserMessage`, `rpc_views`, `session_has_live_tasks`, `interrupt_session`,
  `enqueue_session_input`, `SessionInputRequest`, `required_string`, `sleep`,
  `Duration` if no longer used (let `cargo build` warnings drive the final prune).

### 1b. `subagent.*` RPC + legacy entrypoints

- `types.rs:77` — `RpcMethod::SubagentList` enum variant.
- `types.rs:122` — `"subagent.list" => Some(Self::SubagentList)` parse arm.
- `types.rs:205-208` — the `subagent.list` assertion in `rpc_methods_parse_at_the_boundary`.
- `main.rs:305` — `RpcMethod::SubagentList => subagents::subagent_list(...)` dispatch arm.
- `subagents.rs:20-67` — `subagent_list` + `subagent_spawn_from_active_parent`
  (69-76) + `spawned_subagent_view` (78-85) + `SubagentListParams` (87-90): the
  RPC/REPL entrypoints. `spawn_subagent` itself (225) STAYS.

### 1c. source-ref / fork-context machinery (REPL-only)

`SubagentSpawnRequest::from_params` is the only producer of `sources`,
`initial_context`, `child_session_id`, `provider`, `display_name`, `metadata`,
default `subagent_type`. `StageSubagentSpawn` (the live producer) sets `sources:
Vec::new()`, `initial_context: None`, `subagent_type: Full|ReadOnly`. So after 1b:

- `subagents.rs:121-218` — drop `SubagentSpawnParams` (205) and
  `SubagentSpawnRequest::from_params` (137-202). Reduce `SubagentSpawnRequest`
  (121-135) to the fields `StageSubagentSpawn` actually sets:
  `parent_session_id, role, role_workspace(=None), task, subagent_type, stage_id`,
  plus the constants `provider=None`, `metadata=json!({})` used downstream. Drop
  `child_session_id`, `initial_context`, `sources`, `display_name` fields and the
  `MAX_INITIAL_CONTEXT_BYTES` const (18).
- `subagents.rs:174-187` (source parsing) + `source_session_id` (602-617) +
  `load_source_configs` (619-637) — REMOVE (only `from_params` and the
  read-only-source import use them).
- `subagents.rs:347-367` — the `SubagentType::ReadOnly` source-import block inside
  `spawn_subagent` (`load_source_configs` + `import_source_refs` + its teardown).
  Replace with `let source_refs = Vec::new();` for both arms. The RO fork itself
  (`fork_session_from_parent`, 303-313) STAYS; only the **source ref import** goes.
- `subagents.rs:739-782` — `child_initial_task_message` carries `initial_context`
  and `source_refs`. Both are now always absent. Simplify to the delegated-task +
  "fresh context" block; drop the `fork_context=True` branch (748-759) and the
  `# Source child sessions` branch (760-780). Update the `&[SourceRefSpec]` param.
- `subagents.rs:241-267` — the `existing_child_session_id` reuse path (replay of an
  existing child) is only reachable when `child_session_id` is supplied, i.e. only
  via the REPL `from_params`. With `child_session_id` removed from the request, this
  whole block is dead — REMOVE; `spawn_subagent` always mints a fresh
  `session_{uuid}`.
- `workspaces/mod.rs:50-57` — `SourceRefSpec` struct; `:239-282` —
  `import_source_refs`; `:526-546` — `source_ref_id` helper (only used by
  `import_source_refs`). REMOVE all three. `fork_session_from_parent` (160),
  `destroy_session_workspaces` (284), `remove_session_dir` (292) STAY.
- `workspaces/tests.rs:~352` — the `import_source_refs` test. REMOVE.
- `subagents.rs:902-935` — tests `child_initial_task_message_labels_forked_parent_context`
  and `child_initial_task_message_lists_source_refs_without_diff_payload`. REMOVE.
  Adjust `request_validation_trims_role_and_rejects_empty_task` (824) — it exercises
  `from_params` + `sources`; either delete or reduce to whatever validation remains.

### 1d. Dead legacy idle branch in the runtime (becomes unreachable)

After 1a/1b/1c, no subagent is ever created with a parent but no `stage_id`
(`StageSubagentSpawn` always sets one). The "non-stage subagent" branch of
`try_subagent_parent_idle_event` is then dead:

- `runtime/mod.rs:529-557` — the per-child `insert_subagent_idle_event_once` +
  `destroy_read_only_subagent_workspaces` block. After removal, a session with a
  parent always has a stage_id, so the function reduces to: build notification →
  always go through the stage barrier path. **DO NOT remove the stage branch
  (506-527) or `subagent_idle_notification` (586+).** Verify the simplification by
  asserting no remaining caller can produce a parentful, stageless subagent before
  deleting (grep `parent_session_id: Some` — only `subagents.rs:385` remains, always
  under a stage after 1c).
- This also makes `insert_subagent_idle_event_once` reachable only from the
  dispatch-failed path (`subagents.rs:558-573`, KEEP) — leave the store method.

### 1e. web: `SubagentsSection` / `listSubagents` / `getStage` / dead types

- `panels.tsx:81-141` — `SubagentsSection` component. REMOVE.
- `panels.tsx:942-949` — its `<SubagentsSection .../>` render in `Inspector`.
  REMOVE. KEEP `<RunBoard .../>` (930-941).
- `panels.tsx` Inspector props — drop `subagents`, `subagentSummaries`,
  `subagentsLoading`, `subagentsError` (854-857, 868-871). KEEP everything `stage*`.
- `panels.tsx:34,44,45` imports — drop `SubagentListResult`, `StageSubagent` is KEEP
  (used by RunBoard), `steerableSubagentId` is KEEP.
- `App.tsx:430-441` — `subagentsQuery` + `subagentIds` memo. REMOVE.
- `App.tsx:443-448` — `subagentSummariesQuery` + `subagentSummaries`. REMOVE.
- `App.tsx:920` — `invalidateQueries({ queryKey: queryKeys.subagents(...) })`. REMOVE
  (KEEP the adjacent `queryKeys.stages(...)` invalidation at 925).
- `App.tsx:2150-2153` — the four `subagents*` props passed to `<Inspector>`. REMOVE.
- `App.tsx:1100-1102` — `subagentIds` half of the desired-session loop. KEEP the
  `stageSubagentIds` half (1103-1104). Update the hook deps (1140-1141).
- `agentApi.ts:46` (interface) + `:362-367` (impl) — `listSubagents`. REMOVE.
- `agentApi.ts:50` (interface) + `:393-397` (impl) — `getStage`. REMOVE — it has
  **zero callers** (dead even before this phase; `stage.status` is reached via the
  board's poll, not `getStage`).
- `agentApi.ts:20` — drop `SubagentListResult` import.
- `types.ts:121-130` — `SubagentListItem` + `SubagentListResult`. REMOVE.
- `queryKeys.ts:9-12` — `subagents` + `subagentSummaries` keys. REMOVE.
- KEEP in web: `subagentLabel`/`subagentRunningNotice`/`subagentIdleNotice`
  (App.tsx:2294-2315) and the `subagent.running`/`subagent.idle` event-notice
  handlers (App.tsx:934-938). `subagent.running` (and spawned) STILL fire for stage
  members via `spawn_subagent`; `subagent.idle` per-child no longer fires after 1d
  but the handler is harmless and shared with the dispatch-failed crash notice.
- KEEP `steerSubagent`/`SteerSubagentParams` (agentApi.ts:186-191, 416) — the run
  board steers a stage's full subagent.

### 1f. dead tests / docs

- Tests: items already listed in 1a/1c. Additionally scan `repl` for any
  integration test that drives `subagents.spawn` via `repl.exec` and remove.
- `PI.md:69-70` — keep (already says PythonRepl is scripting-only); no change needed.
- `rust/docs/websocket-rpc.md` — remove the `subagent.list` RPC entry and rewrite the
  `### repl.exec` section (1187-~1310): strip the `subagents.spawn/wait/call/list/
  steer/interrupt`, `fork_context`, and `sources=[...]` documentation (1201,
  1229-1308); keep `repl.exec` documented as a stateful scripting REPL only. Keep the
  `subagent.{spawned,running,idle}` lifecycle-event doc (327) — those events stay.
- `rust/docs/architecture.md:145` — drop the parenthetical about legacy
  `subagents.spawn(sources=…)` git-source-ref merge.
- `rust/docs/modules/agent-daemon.md:39,47,56` — update `subagents.rs` and `repl.rs`
  descriptions to reflect: `subagents.rs` = stage spawn core only; `repl.rs` =
  scripting REPL only (no subagents module).
- `rust/docs/subagent-source-ref-merge-plan.md` — this entire design is the source-ref
  feature being retired; mark superseded or delete.
- `rust/docs/plans/build-map.md` (28 legacy hits) and `phase-0-doc-edits.md` /
  `workflow-orchestration.md` — historical planning docs; leave as historical record,
  do not retro-edit (they describe intent, not current API). Only fix any that are
  presented as *current* API reference.

---

## 2. MUST-KEEP boundary (the remover must never touch these)

Shared code the live `stage.*` path reuses. Touching any of it breaks stages.

| Symbol / location | Why it must stay |
|---|---|
| `subagents.rs:225` `spawn_subagent` | The ONE spawn primitive both stage tools call. |
| `subagents.rs:94-119` `StageSubagentSpawn` + `From<…> for SubagentSpawnRequest` | The live request producer; the trimmed `SubagentSpawnRequest` is its target. |
| `subagents.rs:220-223` `SpawnedSubagent` | `spawn_subagent` return type used by stage tools. |
| `subagents.rs:287-294` `resolve_skill_role` call | Role resolution for every stage subagent. |
| `subagents.rs:298-313` full-vs-RO workspace branch (`fork_session_from_parent`) | RO fanout's private snapshot; full's in-place dirs. |
| `subagents.rs:315-342` `subagent_metadata` + `child_system_prompt` + `ChildPromptRole` | Child config/prompt for stage members. |
| `subagents.rs:377-444` `start_prepared_session` wiring, parent spawn events, dispatch, `cleanup_failed_spawn` | Stage member lifecycle + the RO-teardown safety (`cleanup_failed_spawn` must keep the Full/ReadOnly split at 675-686 — deleting it would delete the parent's workspace). |
| `subagents.rs:446-482` `subagent_parent_spawn_events` (`SubagentSpawned`/`Running`) | Drives the run board + barrier. |
| `subagents.rs:484-535` `subagent_lifecycle_payload` + `publish_subagent_parent_running_if_child` | Parent-visible running event (called from session_start/runtime). |
| `subagents.rs:537-600` `publish_subagent_parent_dispatch_failed_event` (+ test shim) | Stage spawn-failure compensation (FIX E). |
| `subagents.rs:639-657` `require_known_subagent` | Scope check reused by stage RPCs/cancel. |
| `runtime/mod.rs:410-527,560-581,586+` idle→barrier path, `destroy_read_only_subagent_workspaces`, `try_stage_barrier`, `subagent_idle_notification` | The entire stage barrier. Only delete the **non-stage** sub-branch (1d). |
| `stage_runner.rs` + `handoff.rs` + `stage_tools.rs` (all) | The live runner/handoff/tool surface. |
| `workspaces/mod.rs:160` `fork_session_from_parent`, `:284` `destroy_session_workspaces`, `:292` `remove_session_dir` | RO snapshot create/destroy. |
| `registry.rs` `stage_*_definition` (328-470) + `python_repl_definition` (306-326) | The model-facing tool surface. (Update the PythonRepl *description* text at 308-309/319 to drop "subagent delegation"; the tool stays.) |
| `repl.rs` `ReplRegistry`, `PythonRepl` (transport), `repl_exec`, exec protocol, `kill_all`, `provider_runtime/repl_tools.rs` | The PythonRepl scripting escape hatch. |
| store: `SubagentType`, `insert_subagent_idle_event_once`, `claim_subagent_idle_once`, `list_stage_subagents`, stage tables/repo | Shared persistence. |
| web: `RunBoard`, `SubagentRow`, `steerableSubagentId`, `StageSubagent`, `Stage`, `stage.*` API methods, `steerSubagent`, `subagent.{running,idle}` notice handlers | The live run board. |

**Most dangerous mistake:** the keep-vs-remove line runs *through the middle of
`subagents.rs` and `repl.rs`*, not at file boundaries. A remover who deletes
`subagents.rs` or `repl.rs` wholesale, or who removes the `SubagentType::Full` arm of
`cleanup_failed_spawn`/`spawn_subagent` thinking it is "legacy full subagent" code,
will silently break stages — worst case `cleanup_failed_spawn` deleting the parent's
durable workspace because a Full subagent shares the parent's dirs in place.

---

## 3. The per-session `python3` REPL leak fix

Bug: `ReplRegistry::get_or_start` (`repl.rs:65-76`) spawns a `python3 -u -c …` child
per session and inserts it into `state.repls`. It is removed only on timeout
(`repl.rs:44`), protocol/exit error (`repl.rs:57-59`), or daemon shutdown
(`kill_all`, `main.rs:117`). `session_delete` (`main.rs:499-546`) tears down `active`,
the workspace dir, and `provider_connections` — but **never `state.repls`** — so every
deleted session that ever ran a `PythonRepl` cell leaks a live python3 process until
the daemon exits.

Fix (smallest change, no new state):
- Make `ReplRegistry::remove_and_kill` `pub(crate)` (currently private; `repl.rs:78`).
- In `session_delete`'s per-session loop (`main.rs:516-539`), after
  `provider_connections.remove_session(...)` add
  `state.repls.remove_and_kill(candidate_session_id).await;` so the child dies with
  the session. The loop already iterates the full hidden-subagent delete tree, so
  child REPLs are reaped too. (Subagents do not run PythonRepl today, but reaping the
  whole tree is correct and future-proof.)
- This is independent of the legacy removal and can land first.

---

## 4. ORDERED removal steps (build stays green at each step)

Repoint/strip leaf consumers before deleting their providers.

1. **REPL leak fix (3)** — self-contained; `cargo build` + repl test green.
2. **Web first (1e)** — remove `SubagentsSection`, `listSubagents`, `getStage`,
   `subagentsQuery`/`subagentSummariesQuery`, the `<Inspector>` props, dead types and
   query keys. Web has no Rust dependency, so this can't break the daemon; do it early
   so `subagent.list` has no client. Run `tsc` + `vitest` + `vite build`.
3. **Remove `subagent.list` RPC wiring (1b)** — delete the `main.rs:305` dispatch arm,
   `types.rs` enum variant + parse arm + test assertion. Then delete
   `subagent_list`/`subagent_spawn_from_active_parent`/`spawned_subagent_view`/
   `SubagentListParams` from `subagents.rs`. (No client after step 2; REPL still
   references `subagent_spawn_from_active_parent` — so do step 4's repl strip in the
   **same** change or sequence 4 before deleting `subagent_spawn_from_active_parent`.)
4. **Strip REPL orchestration (1a)** — remove `handle_host_call` + all subagent host
   fns/structs + the bootstrap `subagents` module + the host-call arm in
   `PythonRepl::execute`. This drops the last reference to
   `subagent_spawn_from_active_parent`, `subagent_list`, `require_known_subagent`
   *from repl.rs* (note `require_known_subagent` is still used by stage tools — keep
   the symbol, just drop repl's import). `cargo build`.
5. **Collapse source-ref / fork-context (1c)** — trim `SubagentSpawnRequest`, delete
   `from_params`/`SubagentSpawnParams`/`source_session_id`/`load_source_configs`,
   replace the RO source-import block with `Vec::new()`, simplify
   `child_initial_task_message`, delete the `existing_child_session_id` reuse block,
   then delete `import_source_refs`/`SourceRefSpec`/`source_ref_id` from `workspaces`.
   Fix the affected tests. `cargo build` + `cargo test`.
6. **Remove the dead non-stage idle branch (1d)** — only after 5 guarantees every
   parentful subagent has a `stage_id`. Verify with the `parent_session_id: Some` grep.
7. **Docs (1f)** — update `websocket-rpc.md`, `architecture.md`,
   `modules/agent-daemon.md`; retire `subagent-source-ref-merge-plan.md`; trim the
   PythonRepl description in `registry.rs`.
8. **Final warning prune** — remove now-unused imports flagged by `cargo build`.

---

## 5. VERIFICATION checklist

Run after each major step; all must pass and the new surface must be unaffected:

- [ ] `cargo build` (workspace) — no errors, no new warnings.
- [ ] `cargo test` (full workspace) — `subagents`, `stage_runner_tests`,
      `handoff_tests`, `stages_tests`, `workspaces::tests`, `types` all green.
- [ ] `cargo clippy --workspace -- -D warnings` (catches dead code / unused imports).
- [ ] web: `tsc --noEmit`, `vitest run` (esp. `runBoard.test.ts`), `vite build`.
- [ ] **delegation smoke test (new surface unaffected):** start a session, call
      `delegate_readonly_tasks` with 2 tasks and `delegate_writing_task` with 1, confirm
      the run board shows live subagents, the barrier fires once, the handoff dir is
      written (`index.json` + per-subagent `final_message.md`/`transcript.md`),
      `stage.read_handoff_file` reads them, `stage.cancel` cancels a running stage, and
      the completion steer lands as a parent message. The real-model e2e (Task #8)
      remains the gold check.
- [ ] grep guard: `grep -rn "subagents\.\(spawn\|wait\|call\|list\|steer\|interrupt\)\|fork_context\|subagent\.list\|import_source_refs\|SourceRefSpec" rust packages/web/src`
      returns only historical planning docs.
- [ ] Leak guard: delete a session that ran `PythonRepl`; confirm no orphan `python3`
      child survives (`pgrep -f pi-relay-repl` empty for that session).

---

## 6. RISKS / ambiguities

- **The split is intra-file.** The biggest risk is treating `subagents.rs`/`repl.rs`
  as removable wholesale. Half of each file is the live path. Enforce the §2 table.
- **`SubagentSpawnRequest` shrink vs. the `From<StageSubagentSpawn>` impl.** When you
  trim the struct, the `From` impl (`subagents.rs:102-119`) must be edited in lockstep
  or the build breaks at the boundary. Keep `provider`/`metadata` defaults that
  `spawn_subagent` reads (`subagents.rs:329`, `315`).
- **Ordering hazard at step 3/4.** `repl.rs` imports
  `subagent_spawn_from_active_parent`/`subagent_list`; deleting them before stripping
  repl breaks the build. Sequence the repl strip (4) with or before their deletion.
- **`subagent.idle` event after 1d.** Per-child idle stops firing for the now-only
  (stage) subagents (the stage path already suppressed it). The web handler stays as a
  harmless no-op also used for the dispatch-failed crash notice; confirm no test
  asserts a per-child `subagent.idle` for a stage member.
- **`insert_subagent_idle_event_once` reachability.** After 1d its only caller is the
  dispatch-failed compensation path. Keep the store method; do not "clean it up".
- **Ambiguous: the 6 named upstream reachability reports were not present as files**
  in the worktree or `/tmp`; this plan was produced by reading the live code directly
  (file:line cited throughout). If those reports surface and contradict a line range
  here, re-verify against the code before trusting either — the code is authoritative.
- **Historical planning docs** (`build-map.md`, `workflow-orchestration.md`,
  `phase-0-doc-edits.md`) intentionally describe the legacy surface as part of the
  migration narrative. Retro-editing them loses history; only fix docs that present
  the legacy surface as *current* reference (`websocket-rpc.md`, `architecture.md`,
  `modules/agent-daemon.md`).
