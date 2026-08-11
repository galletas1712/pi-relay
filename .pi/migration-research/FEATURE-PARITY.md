# Feature Parity Matrix — prime-agent functionality on upstream-pi extensions

> Master checklist for "port everything, verify everything" (owner directive 2026-08-09).
> Every row must reach ✓ with a real-model (GLM-5.2) trace before the extension stack is
> declared PA-complete. Scope note: this matrix covers PA-parity EXTENSION functionality
> only. Product-layer pieces (bridge/supervisor, workspace-lib, MCP, frontend, migrator)
> are tracked in IMPLEMENTATION-PLAN.md.

Legend: ✓ verified · 🔨 in progress · ☐ scheduled · — deferred/out-of-scope

## A. Kernel & ipython tool (package: prime-rlm)

| Feature | PA source | Status | Verification |
|---|---|---|---|
| Persistent per-session kernel | kernel/bootstrap.ts | ✓ M1 | V4 (var persists across calls) |
| `%%bash` cells (+ command prefix) | tools/ipython.ts | ✓ M1 | V2 (BASH_TOKEN_42) |
| Output truncation | tools/truncate.ts | ✓ M1 | ported with tool |
| Kernel provisioner (venv, uv, overrides) | kernel/bootstrap-cli.ts | ✓ M1 | stamp-file provisioner (simplified; PRIME_RLM_KERNEL_* overrides) |
| Kernel-level env/cwd persistence (`%env`,`%cd`) | ipython native | ✓ M1 | free with persistent kernel |
| **State snapshots / restore (dill)** | kernel/state-snapshot.ts | ✓ M3 | S1 (m3-s1.jsonl: GLM vars → SIGKILL host → resume → `S1-RESTORED EMERALD-FALCON-88 [3, 1, 4, 1, 5, 9, 2, 6]`; kernel-level SIGKILL/restore also in m3-s2.jsonl) |
| **Busy-kernel queueing** (cell while busy) | kernel queue in index.ts | ✓ M3 | S2 (m3-s2.jsonl: cell B submitted 1s into 6s cell A, resolved 3ms after A, both ok) |
| fork-server fast provisioning | kernel/fork-server.ts | — | optional perf; not parity-blocking |
| Attachments / MIME-rich results (images) | kernel/index.ts rendering | ✓ M4 | A1 (m4-a1-attach.jsonl: attachment display → tool result `{"type":"image","data":...}` image/png block; attach-image skill vision-gate refusal parity) |

## B. RLM subagents (package: prime-rlm)

| Feature | PA source | Status | Verification |
|---|---|---|---|
| `rlm.run` spawn (in-process child session) | core/rlm-runtime.ts | ✓ M3 async | V3-equivalent via m3-c3 (v3-async.txt: FIB12-RESULT 144, child session file); blocking API removed (PA parity: async-only) |
| `rlm.list_subagents` registry | rlm-runtime.ts | ✓ M1 | V3 (3 listed, completed) |
| Depth guard (RLM_MAX_DEPTH) | rlm-runtime.ts | ✓ M1 | V5 (error path) |
| **Admission-async `rlm.run`** (handle now, result later) | rlm-runtime.ts admission path | ✓ M3 | R1 (m3-r1.jsonl: 2 children spawned in one cell, handles returned immediately, parent settled) |
| **Result delivery as message/wakeup** | agent_message + rlm completion | ✓ M3 | R1 (both `[from child:fibN]` wakeups → `R1-DONE F10=55 F12=144 SUMSQ=2870`); C3 (m3-c3.jsonl: rlm-only fallback notice `rlm_child_result` with full final text) |
| **Multi-turn children / follow-up steer** | rlm-runtime + agent_message | ✓ M3 | R2 (m3-r2.jsonl: steer delivered mid-run to sleeping child; `WROTE:COUNT-BASE+COUNT-EXTRA` reply; file has both lines) |
| `rlm.delete_subagent` | rlm-runtime.ts | ✓ M3 | R3 (m3-r3.jsonl: deleted completed+running children, registry empty, session dirs removed, no zombie reply after 150s) |
| `rlm.find_models` | host handler find_models | ✓ M4 | R4 (m4-r4-models.jsonl: `find_models()` + `find_models("glm")` → FIND_MODELS_ALL/FIND_MODELS_GLM from models.json) |
| Child usage attribution (P1) | fork patch | — | DEFERRED by owner 2026-08-09 |

## C. Continual harness (package: prime-harness) — M2

| Feature | PA source | Status | Verification |
|---|---|---|---|
| Memory CRUD (local/global) | core/refinement + harness storage | ✓ M2 | H1 |
| Skill CRUD | same | ✓ M2 | H1/H2 |
| Subagent-spec CRUD | same | ✓ M2 | H1 |
| Prompt-note CRUD + injection into prompt | same | ✓ M2 | H1 |
| Prompt assembly (stable prefix + harness section + volatile suffix) | prompts/rlm.ts + before_agent_start | ✓ M2 | H1 (new session loads entries) |
| Post-compaction reinjection of harness section | agent-session compaction path | ✓ M2 | H3 |
| **/refine FULL**: two-phase plan(background)/apply(turn boundary) | core/refinement/refinement.ts (1017 lines) | ✓ M2 | R1/R2 |
| /refine: smallest-edit planner reading trajectory + history | planRefinement | ✓ M2 | R1 |
| /refine: evidence recording (trigger+outcome) | appendGlobalRefinement etc. | ✓ M2 | R1 |
| /refine: rollback by ID | rollbackProposal | ✓ M2 | R3 |
| /refine: auto-refine triggers | reviewAutoRefine wiring | ✓ M2 | R4 |
| `refine.run(focus)` / `refine.status()` kernel API | skills/refine | ✓ M2 | R1/R2 |
| `/refine` command | registerCommand | ✓ M2 | R1 |

## D. Inter-agent comms (package: prime-comms) — M2

| Feature | PA source | Status | Verification |
|---|---|---|---|
| `agent_message.send` (parent↔child↔sibling scoping) | core/agent-messages.ts | ✓ M2 | C1 |
| `agent_message.list_agents` (family roster) | same | ✓ M2 | C1 |
| `agent_observe.get_agent` / `recent_messages` | same | ✓ M2 | C1 |
| Message wakeups delivered to recipient | same | ✓ M2 | C1 |
| **Durable outbox** (survives kill -9) | (P2 stand-in; PA daemon-owned) | ✓ M2 | C2 |
| Modularity: prime-rlm works standalone | — | ✓ M2 | C3 |

## E. Skills system (package: prime-harness) — M2

| Feature | Status | Verification |
|---|---|---|
| Markdown skills auto-discovered + listed in prompt (~/.prime/agent/skills + project) | ✓ M2 | H2 |
| `python_import` skills pre-imported into kernel | ✓ M2 | H2 |
| Content skills ride along: linear, notion, prime-intellect, skill-creator, edit, attach-image, websearch, goal, compact, rlm-heartbeat | ✓ M4 | m4-skills.jsonl: SKILLS_IMPORT_OK + edit.run round-trip; 10 skills listed in system prompt (jiti loadHarnessSkills check) |

## F. Autonomy & lifecycle (M4; some pieces may live in supervisor — design call)

| Feature | PA source | Status | Verification |
|---|---|---|---|
| Goals: start/complete/status, budget tracking, re-prompt loop | core/goals.ts + goal skill | ✓ M4 | G1 (m4-g1-goals.jsonl: set → goal_context continuation → work → complete → loop stops; thread_goal_state persisted), budget (m4-g1b: token_budget=100 → budget_limited context + status), restart restore (m4-g1c: SIGKILL → resume → active + re-prompt), `--goal` seed (m4-g1d) |
| Heartbeats: interval-scheduled injections (create/list/update/delete) | rlm-heartbeat skill + scheduling | ✓ M4 | G2 (m4-g2-heartbeat.jsonl: 15s interval fired HB-TICK while idle, follow_up delivery, delete stops ticks) |
| Autonomous mode: continuation + gates + max-continuations/turns/tokens/timeout | agent-session autonomous (8 CLI flags) | ✓ M4 | G3 (m4-g3-autonomous.jsonl: gate failed attempt 1 → model fixed → gate passed → stop; m4-g3b-cap.jsonl: always-failing gate → exactly max-continuations=2 continuations then stop) |
| `compact.status()` + `compact.run()` kernel API | skills/compact | ✓ M4 | G4 (m4-g4-compact.jsonl: CSTATUS tokens/window/percent/scheduled printed, CRUN True, compaction_start manual, compaction entry in session file) |
| Compaction works + harness reinjection | upstream compact + prime-harness | ✓ M2 (H3) | — |

## G. Observability events for the future frontend (M4)

| Feature | Status | Verification |
|---|---|---|
| Subagent lifecycle events surfaced via rpc custom messages | ✓ M4 | O1 (m4-o1-o2.jsonl: `rlm_child_lifecycle` session entries phase=admitted/completed; pi can't extend AgentSessionEvent so these are custom session entries — rpc-visible via get_state/messages) |
| `ipython_sent_agent_message`-equivalent trace events | ✓ M4 | O2 (m4-o1-o2.jsonl: entry in parent session after PING_STATUS delivered; also in child session file) |
| Refinement events queryable (get_harness_state / history) | ✓ M4 | O3 (m4-o3.jsonl: record_refinement → get_harness_state().refinements round-trip, O3_LAST trigger echoed) |

## H. Subagent roles (pi-relay parity; prime-harness + prime-rlm) — M7

| Feature | pi-relay source | Status | Verification |
|---|---|---|---|
| Role files (SKILL.md + model/effort/max_tokens/preload frontmatter), global+project origins | agent-prompt SubagentRole; skills pipeline SkillKind::SubagentRole | ✅ M7 (+ harness-stored roles, precedence-ordered origins) | R3 green (m7-r3.jsonl) |
| Role catalog in parent prompt | subagent_role_catalog_json | ✅ M7 (depth<maxDepth gate) | R1 green (m7-r1.jsonl; ROLES_SEEN=11 roles) |
| `rlm.run(role=...)` → child prompt = contract + role body + preloaded skills | subagents.rs::child_system_prompt | ✅ M7 (nested delegation allowed — deliberate) | R1 green (R1-PONG-42, HAS_PRELOAD_SECTION=yes) |
| Role model/effort override (role → caller → parent default) | select_subagent_provider | ✅ M7 | R2 green (m7-r2.jsonl + shim log: deepseek vs glm) |
| Registry/tree records role | — (new; pi-relay had it in DB) | ✅ M7 (registry.role, list_subagents, comms subagentRole) | R3 green (r3greeter:greeter …) |
| pi-relay's actual role content + PI.md subagent profile ported | operator files | ✅ M7 (9 roles verbatim + provenance; monitor name repaired) | R1 green |

| AGENTS.md / project context files in prompt | InstructionScope Global/Project/Workspace; upstream contextFiles | ✅ prime-harness src/context-files.ts (global agentDir + cwd→root walk, 32KB/file + 96KB total caps, root-most-first) | deterministic render verified 2026-08-09 (fixture incl. walk-up); live-session check in M8 |

## I. Prompt-text parity vs PA rlm.ts (found via line-diff 2026-08-09)

| Line(s) | Owner | Status |
|---|---|---|
| Skills preamble: pre-imported list, introspection, CLI `--help`, edit-skill hint | prime-harness skills.ts | ✅ restored + tsc-clean + render-verified |
| Harness CRUD paragraph + Terminology | prime-harness store.ts doctrine | ✅ restored (PA-verbatim) |
| Pre-installed packages = PA's 12 (provision parity) | prime-rlm provision.ts | ✅ EXTRA_REQUIREMENTS + PREINSTALLED_PACKAGE_LABELS; kernels re-provision on next boot (stamp hash) |
| Conversation log line; pre-installed packages lines; # Delegating to sub-agents block | prime-rlm prompt.ts | ✅ restored + 23/23 render checks (peer-guarded; rlm-only clean) |

## Milestone order
M1 ✓ (kernel+rlm blocking) → **M2 ✓ (harness + comms, full /refine — all 10 scenarios green, traces m2-*.jsonl)** → **M3 ✓ (RLM semantics:
async run + wakeups + multi-turn + delete + snapshots + busy-queue)** → **M4 ✓ (goals + heartbeats +
autonomous + compact API + find_models + content skills + A1 attachments + O1/O2/O3 observability —
12 m4-* scenarios green + m3-r1 regression green; see M4-AUTONOMY.md)**.
Then: bridge/supervisor (product layer, IMPLEMENTATION-PLAN.md §3).
