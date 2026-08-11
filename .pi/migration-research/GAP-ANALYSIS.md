# Gap Analysis — pi-relay → prime-agent-core Migration

> Parent-authored synthesis. Sources: the per-codebase reports in this directory
> (`pi-relay-rust-backend.md`, `pi-relay-frontend-contract.md`, `prime-agent-deep.md`,
> `omp-deep.md`, `omp-batteries.md`, `pi-mono-deep.md`, `bridge-transport-options.md`,
> `storage-strategy.md`). Every row cites the report section where the detail lives.
>
> **Coverage key:** ✅ first-class · 🟡 partial / needs adapter · 🧩 available as
> extension/plugin/battery to adopt · ❌ absent · 🆕 net-new capability the target adds.

**Abbreviations:** PA = prime-agent (installed 0.7.0-beta.458 / repo v0.7.1),
PM = pi-mono upstream (@earendil-works, v0.84.1), OMP = oh-my-pi (can1357).

---

## 1. Core agent loop & model dispatch

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 1.1 | Agent loop semantics | Deterministic turn FSM (Idle→RunningModel→RunningTools→ReadyToContinue), tool-call batching, steer splice at ReadyToContinue (`agent-core`) | RLM/IPython: model writes Python into persistent kernel; steer = turn-boundary stop + re-prompt via action store | Tool-call loop, hookable (`AgentLoopConfig`: convertToLlm/transformContext/shouldStopAfterTurn…) | Same loop hardened; `prepareProviderCall` with 5 injected seams | **Adopt PA's RLM core** (the point of the migration). Steer semantics map 1:1 (both are turn-boundary). |
| 1.2 | Provider adapters | Only 2: OpenAI **Codex-subscription** transport (hardcoded base_url, thread_id cache cohort), Anthropic Messages w/ OAuth, attribution header, dual cache breakpoints (`agent-provider`) | pi-ai fork: 30+ providers, OAuth, `createProvider` seam | Same, upstream | pi-ai hardened: multi-credential round-robin, owned-dialect in-band tool calling, harmony-leak retry | Use PA's pi-ai for Codex+Claude first; verify subscription transport parity (pi-relay's adapter behavior is the spec; port headers/thread_id pinning if missing). |
| 1.3 | Token accounting & compaction gating | Remote `count_tokens` (Claude) / usage-anchored estimation (OpenAI); proactive gate per dispatch; provider-native compaction; post-compaction dispatch lease fencing; **cache engineering**: attribution-header fingerprint (cross-session prefix reuse), dual transcript breakpoints (tail+~18 deep), 1h prefix TTL, thinking cache-neutral | Threshold trigger (contextWindow−16384) + provider-overflow regex battery + model-requested `compact.run`; summary + ~20k retained tail; **no remote count_tokens gate**; cache = upstream's 3-breakpoint pattern (system/last-tool/last-user) BUT agent layer never sets `cacheRetention` (5m TTL default) and compaction lacks upstream's post-fork `cacheRetention:"none"`+fresh-sessionId isolation (cherry-pick, upstream 9b3a2059) | `session_before_compact`/`session_compact` extension events; fixed-schema summary entry; summarization isolated from cache (retention none + fresh key) | TTSR + snapcompact (PNG bitmap) + classic; OTel spans capture full requests | Keep PA triggers; port: remote count_tokens gate (host-side pre-dispatch), 1h prefix retention plumbing, deep breakpoint, compaction cache isolation (2-line upstream port). Details: CONTEXT-ENGINEERING.md §10.2. |
| 1.4 | Tool surface | shell/apply_patch/text_editor/web tools declared per provider; 10k-token head/tail truncation ×3 bounding layers | Single `ipython` tool + pre-imported Python skill modules (edit, compact, websearch, agent_message…) | Default tools (read/write/edit/bash) pure data: name+schema+execute | 29 builtin tool factories (factory-level gating) + LSP/DAP/hashline edit format | PA model **is** the plan. pi-relay's web tools → PA websearch skill or pi plugin. Truncation discipline → port as kernel-side output bounding. |
| 1.5 | Crash recovery of turns | Crash-tail repair: open turns closed with synthesized crashed tool results; actions stale-marked on boot | Supervisor/worker crash recovery, recovery journals, orphan journals, auto-resume on attach; kernel dill snapshots | Session JSONL reload; v4 `findOpenOperations()` | JSONL-backed revive | PA's is thorough at process level; turn-tail repair becomes unnecessary (in-kernel execution state is dill-snapshotted). ✅ better. |

## 2. Control plane (daemon, RPC, events)

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 2.1 | Browser-facing transport | WSS, Origin allow-list, 8 MiB frames, 54-method JSON RPC + interleaved events (`agent-daemon`) | **Unix socket only**, JSONL, ~96 verbs, v7/schema 14, capability negotiation; "future gateway can wrap or proxy" is an *intended* seam | pi-protocol (CBOR) + pi-server (pluggable listeners, WS envisioned) — **experimental, unwired** | RPC stdio (~45 cmds), collab AES-GCM WS relay, ACP | **Bridge service** (new TS package) terminating WSS exactly like pi-agentd, driving PA via SDK `DaemonClient` (bridge report Option B). |
| 2.2 | Event durability & replay | Durable events table = reconnect buffer; `events.subscribe(after_event_id)` replay; 34-event catalog; per-session monotonic ids | Events carry `{generation, sequence}` cursors; **replay explicitly unavailable** (worker restart = new generation = cursors void); snapshots authoritative | AgentEvent stream is the only loop↔UI contract | collab snapshot-chunk + live frames | Bridge owns an events spool (in-flight window only — pi-relay's own buffer also drains on idle). Synthesize the 34-event vocabulary from daemon events (~12 direct, ~14 synthesized, ~8 bridge-emitted; mapping in bridge report §8.1). |
| 2.3 | Durable input queue | PG ledger: client idempotency keys, edit/reorder/promote/cancel, steer priority, atomic consume-with-transcript-transition | **In-memory only**: action store queue (steering/followUp text previews, no ids, no reorder API) | v4 seam has `queue_enqueued` records + lanes | reserved `custom` JSONL entries | Bridge owns durable queue; releases into PA at turn boundaries (steer/follow_up verbs). Double-queue mitigation in bridge report §6.2. |
| 2.4 | Idempotency keys | session ids client-generated; `client_input_id`, `client_control_id`; replays return prior state | Session ids client-generated at create ✅; **no input/control idempotency keys** | n/a | n/a | Bridge dedupes (its store sees the keys first). Load-bearing for the frontend's RpcTransportError reconcile flows — **must preserve exactly**. |
| 2.5 | Projects & runtimes | `project.*` CRUD; `runtime.list` (10 s poll); control-plane/runtime-plane split (daemon brokers to pi-runtime hosts) | ❌ no project entity; single-host daemon | ❌ | agent-hub concept (multi-host, early) | Bridge-local tables (projects, runtime registry). If pi-runtime is kept as execution sidecar during transition, bridge brokers to it (runtime protocol is reusable verbatim — rust report seam 6). |
| 2.6 | Supervision | Docker container, single process | Supervisor→worker tiers, leases, idle eviction/passivation, crash auto-resume, coordinated self-update with mutation draining | ❌ (embed yourself) | ❌ | PA's is strictly better; keep as-is. The bridge is a stateless-ish façade in front. |

## 3. Sessions, history, storage

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 3.1 | Transcript model | Postgres **forest** (parent pointers, active leaf, branch-aware paging, turn cards) | Append-only tree JSONL v3 (uuidv7 files, parentId branching, compaction entries) | v3 JSONL tree; **v4 seam** (lanes, records, writer leases, conformance suite) | v3 JSONL + IndexedSessionStorage path-keyed backend seam | Transcripts → JSONL files (storage report: write **v4** via the PM seam for portability + relational upgrade path). Bridge serves `transcript.*` paging over snapshots/JSONL. |
| 3.2 | Control-plane durability | Queue, actions CAS, delegation ledger, events — all PG | All in-memory per worker ❌ | v4 records cover it ✅ | 🟡 custom entries | **Postgres stays** for the control plane (storage Option C): bridge + a slim store service own queue/projects/delegation-control/events-spool tables. |
| 3.3 | Compaction representation | New root + `source_leaf_id` cross-session lineage; daemon-appended delegation ledger | Inline `firstKeptEntryId`; summary-first rebuild | v3: fresh session id entry; v4: inline `retainedTail` | inline | Migrator resolves active branch THROUGH compactions; converts lineage→inline (storage report §8). |
| 3.4 | History ops | `history.switch/fork/tree/targets/context`; turn.resume for Interrupted/Crashed | `navigate_tree{summarize,label}`, `fork{position}`, `get_session_tree`, auto-resume on attach; **no explicit turn.resume verb** | `/tree`, `/fork` first-class | ACP fork/resume semantics | Direct mapping (bridge report §4.2). turn.resume: drop from UI or small PA patch wrapping checkpoint re-drive. |
| 3.5 | Old-session compatibility | — (AGENTS.md: one-shot scripts, no compat paths) | — | v1→v3 auto-migrate on load (same philosophy) | v3↔v4 ingest (`sourceFormat:3`) | **One-shot migrator**: PG → JSONL, id-remap sidecar (pi-relay arbitrary text ids → 8-hex/uuid), topo-sorted by parent/fork/compaction lineage, append active tip LAST, flush unconsumed queued inputs as user messages, unfinished actions → crashed tails, verify by diffing reconstructed active-branch context. (storage report §8.) |

## 4. Delegation / subagents

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 4.1 | Spawn model | Daemon tools: 1 full-writer + 8 read-only slots, PG admission invariants (partial unique index) | `rlm()` from kernel: children **in the parent's worker process**, own session JSONL + kernel + artifact dir, daemon-registered with own activeSessionId; depth cap (chat > inherited > global > env > default 1); admission-only handle (never the answer) | Subprocess-per-agent **extension** (examples/extensions/subagent, agents/*.md catalogs) | In-process `createAgentSession` per child, prompt splicing, git-worktree isolation, 200-request soft budget, depth 2 | **Adopt PA rlm()** (the user's stated target). Frontend spawn surface (`delegation.start_full`) needs a small PA daemon patch: `start_rlm_child` command (machinery exists; bridge report §7 patch 1). |
| 4.1b | Child lifecycle | Delegation rows terminal via barrier CAS; handoff artifacts on disk | Children **linger**: retained in-memory for message/observe; idle-passivated ≤2/worker/sweep after 90 min (rehydrated on demand); `rlm-subagents.jsonl` registry append-only, unbounded, delete = tombstone (transcript + artifacts stay on disk) | — | 7-min idle TTL, output caps | PA model is workable; add ops hygiene: artifact-dir retention policy; bridge `delegation.list` reads the registry. |
| 4.2 | Parent notification | Single wakeup per delegation: barrier CAS → handoff files under `.pi-handoff/<id>/` → ONE DaemonToolObservation with file refs (never inlined transcripts) | Child replies via `agent_message` → delivered as user-role message; premature-end wakeups; completion = message, not artifact | Subagent extension returns output text | Executor returns final message | Keep pi-relay's **file-artifact + explicit reply** doctrine (proven superior in this very project — see harness memory). Implement as harness policy/skill convention on PA messaging. |
| 4.3 | Workspace isolation for children | Full-writer shares parent workspace; read-only fanout gets **disposable btrfs snapshots** | Children share the host; **no filesystem isolation** (user decision: btrfs moves to top-level per-session) | Worktree per subagent (extension) | git-worktree isolation runner | Per-session top-level btrfs subvolume; children operate inside it. Read-only fanout snapshots become optional (drop or keep via btrfs snapshot in workspace lib). |
| 4.4 | Roles catalog | 9 roles (explore/implementer/merger/monitor/planner/reviewer/tester/verifier/worker) as runtime-mounted SKILL.md files + prompt contracts | Subagent specs in continual harness (`rlm.harness.create_subagent`); stable child naming | agents/*.md catalogs | task/structured-subagent policies | Port roles → PA harness **subagent specs** + skills. Progressive disclosure: specs as compact harness entries, full role SKILL.md loaded on demand. |
| 4.5 | Observability | `delegation.list` rows w/ per-subagent status/outcome/handoff refs; `subagent.*` events | `rlm_child_update` events (activity/token/answer previews) + `watchSession` read-only live views + `agent_observe` | Subprocess status | Per-agent budgets (inject→force-yield→abort) | Bridge maps PA child events → `subagent.*` event vocabulary; `delegation.list` synthesized from PA roster/registry. Per-agent request budgets: adopt OMP-style as PA harness config (optional). |

## 5. Workspaces & filesystem

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 5.1 | Workspace lifecycle | pi-runtime `WorkspaceManager`: btrfs clones of base trees, per-session subvolumes, multiple workspace dirs per session, git worktrees, materialization progress | ❌ none — kernel runs on host cwd | ❌ (cwd-based) | 🟡 worktrees for subagents | **Port WorkspaceManager to Python** as a kernel-side library (or a small host service): top-level btrfs subvolume per session (user constraint), multi-workspace dirs kept, clone/materialize on session.start with progress events. |
| 5.2 | Workspace RPCs | `workspace.list_dir/read_file/watch/git_status/git_diff` (paged, merge-base compare, PR links), `workspace.fs_changed` events | ❌ none | ❌ | ❌ | Bridge implements against session cwd(s) with path jailing (mechanical; bridge report §3.4). fs_changed via Chokidar. |
| 5.3 | Runtime hosts | Outbound-connecting pi-runtime workers; control/runtime plane split | Single-host (workers are local processes) | ❌ | agent-hub (early) | Transition: keep pi-runtime as execution sidecar (its protocol is vocab-native and reusable — rust report seam 6) **or** collapse to single-host (simplest; matches PA model). Decide per deployment needs. |

## 6. MCP

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 6.1 | MCP client | rmcp-based, runtime-hosted, streamable_http, OAuth flows (slack/linear/outlook/nvcarps in prod), fingerprint-validated per-session manifest | Python `mcp_base.py` `McpIntegration` (settings.json mcpServers, host-side OAuth) | ❌ deliberately not built in — "MCP bridge = small registerTool extension" (upstream doctrine) | Full battery in coding-agent `src/mcp/` | **Requirement: must not regress.** PA's integration is the base; port pi-relay's OAuth lifecycle UX (mcp.login/complete/cancel/logout from system browser) into bridge+PA host. |
| 6.2 | Inventory & token estimates | `mcp.inventory(provider, runtime_id, session_id?)` with per-tool `context_token_estimate`, revisions, health | ❌ no inventory RPC | 🧩 extension | ✅ inventory + gating | Bridge serves inventory from PA MCP integration state; token estimates via count_tokens (OMP natives or PA tokenizer). |
| 6.3 | Per-session selection | `mcp.add` to idle session w/ conflict error; `mcp.tools_added` event | settings.json (global) | 🧩 | ✅ per-session | Bridge/PA host: per-session server overlay on top of global settings; emit `mcp.tools_added`. |

## 7. Prompt & context engineering

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 7.1 | System prompt | PI.md minijinja template rendered **at authoring time, persisted on session row**, reused verbatim (cache-stable prefix); blocks: project AGENTS.md, workspace markdown, tool specs, MCP index lines, skills index JSON, delegation docs | `buildRlmPrompt` fixed order: identity/cwd/depth → child doctrine → recursion block → IPython control → subagent guidance → harness menu (≤6 entries/kind, 180-char clips) → AGENTS.md → skills index (metadata-only) → host append; **wholesale rebuild at triggers**, not per turn | base + per-tool snippets + appendSystemPrompt + `<project_context>` + skills INDEX; extensions can replace | `string[]` blocks end-to-end; splice-first-class; `systemPrompt` accepts function over default array | Merge the two doctrines: PA prompt builder gains pi-relay's persisted-prefix discipline (render once per session config, cache breakpoints) — or accept PA's rebuild-trigger model (cheaper to adopt). **Detail in CONTEXT-ENGINEERING.md.** |
| 7.2 | `system.prompt` observability | `system.prompt(session_id)` → {template, rendered}; invalidated on mcp.tools_added | `get_system_prompt` ✅ | — | — | Direct bridge mapping. Must stay faithful (frontend's only context-observability surface). |
| 7.3 | Progressive disclosure | index lines → declarations → LoadSkill bodies → handoff file refs; handoffs **referenced not inlined** | Systematic: skill bodies, model catalog, child transcripts, tool outputs, harness bodies, history — all pulled on demand via kernel/host | Skills INDEX + read tool; large outputs spill to files (`fullOutputPath`); `!!` bash excluded from context | Same + TTSR | Doctrines align. Port LoadSkill → PA skill mechanism (SKILL.md bodies read on demand); handoff-refs-not-prose as policy. |
| 7.4 | Continual learning | ❌ none (dead `dynamic_context` plumbing) | 🆕 **Continual harness**: dual-scope JSON store (Python-authoritative), prompt notes/memories/skills/subagent specs, `/refine`, harness menu in prompt | ❌ (appendEntry + skills approximations) | 🧩 pi-mnemopi (SQLite memory + MCP server export) | Adopt PA harness wholesale — **the biggest net-new capability gain**. Optionally add pi-mnemopi as MCP memory server later. |

## 8. Frontend contract (must-serve surface)

| # | Capability | Detail | Migration plan |
|---|-----------|--------|----------------|
| 8.1 | 54 RPC methods (51 called) | Full inventory + per-method mapping in `bridge-transport-options.md` §4: ~10 direct daemon calls, ~22 bridge-mechanical, ~10 need PA patches or redesign, project.*/runtime.list bridge-local | Bridge façade. Facade-only methods (`delegation.start_*`, `read_handoff_file`, `history.tree/context`) must exist even though prod UI barely calls them (tests + deferred execution UI). |
| 8.2 | 34-event catalog | `sessionEvents.ts`; unknown events fail safe → additive-safe | Bridge synthesizes; mapping table bridge report §8.1. |
| 8.3 | Idempotency + error taxonomy | RpcRequestError codes the UI matches (`session_not_found`, `history_changed`, MCP conflict) vs RpcTransportError reconcile (45 s uncertain-start poll) | Bridge preserves verbatim — **highest-risk item to get subtly wrong**; write contract tests from `agentApi.ts` facade. |
| 8.4 | Electron shell | ~200 lines, loads deployed URL | Unchanged. |
| 8.5 | CSP | `connect-src 'self' wss: + loopback ws:` | Bridge must be reachable via WSS (Tailscale Serve keeps working). |

## 9. Extensibility & batteries (the "pi-modularity" requirement)

| # | Capability | pi-relay today | PA | PM | OMP | Migration plan |
|---|-----------|----------------|----|----|-----|----------------|
| 9.1 | Extension/plugin system | ❌ none (skills only) | Skills (markdown + Python) + harness specs | ✅ 25-event ExtensionAPI, jiti-loaded TS, Pi Packages distribution, 80+ examples | ✅ 45-event ExtensionAPI + hooks + plugin marketplace + **upstream-compat machine** (Babel import-rewriting runs pi-mono plugins unmodified) | User constraint: "use pi plugins for gaps." PA loads pi-style skills natively; for true pi *extensions* (TS lifecycle hooks), either (a) port the PM extension runner to the bridge/core edge, or (b) run selected OMP/PM extensions in a sidecar. MCP is the main gap and is covered natively (§6). |
| 9.2 | Skills | Runtime-mounted SKILL.md catalog + LoadSkill | ✅ markdown skills + pre-imported Python skills | ✅ Agent Skills spec | ✅ + capability discovery | Port existing pi-relay skills/roles verbatim (format-compatible). |
| 9.3 | Batteries to adopt | — | 🆕 harness, heartbeats, goals, refine | 🧩 plan-mode, permission-gate, todo, ssh/sandbox ops, custom providers | 🧩 hashline edits, snapcompact, mnemopi memory, LSP/DAP, task framework, collab protocol | Adopt incrementally post-migration from PM examples (clean-room small) or OMP `src/<battery>/` dirs (bigger, entangled — see omp-deep §13 lift difficulty). |

## 10. The ten sharpest gaps (ranked by risk × effort)

1. **Durable editable queue + event replay** — PA has neither; bridge must own both. (§2.2, §2.3)
2. **Workspace subsystem (btrfs, multi-workspace, materialization)** — must be ported to Python/host service; pi-relay's is 4.7k LoC of battle-tested Rust. (§5.1)
3. **MCP OAuth UX parity** — production Slack/Linear/Outlook/NVCarPs flows must not regress. (§6.1)
4. **Idempotency/error-taxonomy fidelity** — subtle; frontend reconcile flows depend on exact semantics. (§8.3)
5. **Client-initiated delegation spawn** — needs a small PA daemon patch (`start_rlm_child`). (§4.1)
6. **Provider subscription-transport parity** — Codex thread_id cohort + Anthropic attribution header must be verified/ported into PA's pi-ai. (§1.2)
7. **Transcript paging over JSONL** — bridge serves `transcript.*` efficiently (turn-card synthesis); watch memory on huge sessions. (§3.1)
8. **Persisted-prompt cache strategy** — pi-relay's authoring-time persistence vs PA's rebuild-at-triggers; decide and keep Anthropic breakpoints hot. (§7.1)
9. **Old-session migration** — one-shot PG→JSONL migrator with id remap + topo ordering; verification by context reconstruction diff. (§3.5)
10. **`turn.resume` + compaction event vocabulary** — small PA patch or UI concession. (§3.4)

## 11. What the target adds that pi-relay never had (🆕)

- Continual harness (memories, prompt notes, skill CRUD, subagent specs, `/refine`) — dual-scope, Python-authoritative JSON store.
- RLM recursion: children spawned from inside the kernel with full family messaging/observe.
- Heartbeats (agent-owned scheduled jobs), goals with budgets.
- 30+ providers via pi-ai (vs 2 hand-rolled adapters).
- Compaction on demand from the model itself (`compact.run`).
- Kernel state persistence (dill snapshots) — resume mid-computation after crash.
- A designed UI boundary (`AgentConnection`, ~60 methods, documented adapter model) to build the bridge against.

## 12. Battery adoption inventory (from omp-batteries.md §36 ranking)

Post-migration, adoptable in priority order. **Tier 1 — steal soon (small, clean):**

| Battery | What it is | Where it plugs in |
|---|---|---|
| hashline edit format | self-contained edit grammar + parser + recovery + `prompt.md` | kernel-side edit skill upgrade |
| secrets obfuscation | reversible HMAC placeholders; restore-in-tool-args, re-obfuscate-on-replay | PA convertToLlm boundary |
| approval tiers | read/write/exec + arg-dependent decisions + user overrides | bridge/PA host permission gate |
| magic keywords | tokenizer-safe prompt scan → per-turn hidden notices | bridge or PA host pre-turn hook |
| skill-index discipline | name+desc in prompt, bodies via on-demand load, compaction-protected reads | already PA doctrine; adopt OMP's protection bits |
| checkpoint/rewind | model-driven manual compaction (branchWithSummary) so exploratory turns never reach the next provider call | PA compaction skill |

**Tier 2 — pattern-level, high value for the RLM core:** OMP's `eval` tool internals
(NDJSON runner protocol, MIME precedence, status side-channel, matplotlib Agg
auto-PNG, env denylist stripping API keys, dead-kernel replace+retry,
SIGINT→KeyboardInterrupt→5s escalation, **timeout suspension during bridge calls —
ref-counted pause/resume, exactly what kernel-side rlm() needs**, `agent()` prelude
helper = OMP's literal rlm() equivalent returning resumable `agent://` handles);
system-prompt block-array with cache-stable prefix first; subagent prompt splice +
yield contract; compaction prompt skeletons (steal verbatim); buildSessionContext
emission-boundary algorithm (drop dangling tool calls + aborted turns); xd://
discoverable tools (zero-prompt-token tool schemas); advisor sidecar-reviewer state
machine; vibe director/worker split.

**Tier 3 — defer:** internal-URL read surface, blob/artifact OutputSink stores, RPC
control protocol details, TTSR stream interception, MCP 250 ms fast-startup gate +
circuit breaker.

**Skip:** OMP Rust natives, collab Go relay, auth broker/gateway, LSP/DAP,
marketplace/plugin-manager, notebook JSON round-trip.

**Cross-cutting patterns worth adopting regardless of tier:** every tool gets a
versionable model-facing prompt file separate from implementation; orchestration
policy lives IN the system prompt (concurrency caps, fan-out rules); declared-
authoritative XML injection-tag vocabulary for all runtime→model injections.
