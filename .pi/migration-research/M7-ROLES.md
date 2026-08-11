# M7 — Subagent roles (pi-relay parity)

Status: **complete** (all four verifications green on real GLM-5.2 / DeepSeek-v4-flash via the
NVIDIA shim; traces `m7-r1*.jsonl`, `m7-r2*.jsonl`, `m7-r3*.jsonl`, `m7-r4*.jsonl`,
`m7-r4-m3r1.jsonl` in `.pi/m1-demo/traces/`).

M7 ports pi-relay's packaged subagent roles (`SkillKind::SubagentRole`) onto the extension
stack: role files on disk + roles as continual-harness content, a documented public seam from
prime-harness, role-configured `rlm.run(..., role="...")` spawns in prime-rlm, the prompt
catalog, registry/tree metadata, and pi-relay's real role content seeded into the demo.

It also restores the three PA prompt elements lost to M1 compression that live in
prime-rlm's prompt territory (delegated by the orchestrator mid-milestone; the harness-side
restorations were done by the orchestrator).

## Architecture

```
~/.prime/agent/roles/<name>/SKILL.md   (global-dir origin; demo: .pi/m1-demo/agent/roles/)
<cwd>/.pi/roles/<name>/SKILL.md        (project-dir origin; NEW — pi-relay had none)
rlm.harness entries kind="role"        (harness-global / harness-local origins; NEW)
        │
        ▼  prime-harness/src/roles.ts  (discovery, validation, catalog, preload resolution)
globalThis[Symbol.for("prime-harness.host-api")] v1
  { roleCatalog(ctx), resolveRole(name, ctx) }      ← public seam (src/host-api.ts)
        │
        ▼  prime-rlm/src/roles.ts resolveRoleForSpawn (lazy lookup; missing seam + role ⇒ error)
rlm.run admission: role resolution FAILS FAST (pi-relay role_not_found parity), model
precedence applied (explicit model= → role frontmatter → parent default), registry entry
records role + resolved model override, registerPendingRole before bindExtensions
        │
        ▼  child session_start consumes the pending role into SessionState.role
prime-rlm prompt.ts appends: # Subagent contract + # Subagent role + # Preloaded skill: …
prime-rlm index.ts adds a kernel bootstrap contribution pre-importing the role's
python skills (works even in rlm-only loads; prime-harness pre-imports everything anyway)
```

### Origin mapping (pi-relay → this stack)

| pi-relay | this stack | notes |
|---|---|---|
| `$runtime_config_root/subagent-roles` (only origin) | `<agentDir>/roles` (`global-dir`) | agentDir = `PI_CODING_AGENT_DIR` (default `~/.prime/agent`) |
| — | `<cwd>/.pi/roles` (`project-dir`) | NEW; mirrors pi's `.pi/` project-config convention |
| — | harness `kind="role"` scope global/local | NEW; PA-style manageability (`rlm.harness.create_role`) |

Precedence (first match wins): **harness-local > project-dir > harness-global > global-dir**.
pi-relay has exactly one origin and errors on duplicates; ordered origins make same-name
shadowing intentional (same doctrine as harness merge: local beats global). Documented
difference, not an oversight.

### Frontmatter

Canonical pi-relay names: `name`, `description`, `model`, `reasoning_effort`, `max_tokens`,
`skills` (preload list). Aliases accepted (documented): `effort` → `reasoning_effort`,
`preload` → `skills`. `name` must equal the directory name (file origins) or the harness
entry id (harness origins) — pi-relay's `resolve_role_file` doctrine; invalid roles are
omitted from the catalog and produce a descriptive error at spawn.

`model:` canonical form is this stack's `provider/model-id` (model ids may themselves contain
slashes — provider is the first segment). pi-relay's legacy `provider:model` colon form is
still accepted (pi-relay seeded roles use it, e.g. `openai:gpt-5.6-luna`).

### Model / effort / max_tokens policy (select_subagent_provider port)

Precedence: **caller `model=` → role `model:` → parent default** (pi-relay
`explicit.or(role_provider).unwrap_or(parent)`).

* Explicit caller model that is not in the registry ⇒ hard error (pre-existing M3 behavior).
* Role model that is not in the registry ⇒ console warning + `role_model_fallback` note in
  the lifecycle "admitted" metadata + fall back to the parent default. This adapts pi-relay's
  `stable_default_provider()` fallback: pi-relay falls back to its configured stable provider;
  the extension stack's stable default IS the parent session model.
* `reasoning_effort` → pi `ThinkingLevel`, passed via `createAgentSession({ thinkingLevel })`
  (invalid value ⇒ warn + medium). `max_tokens` ⇒ the resolved `Model` object is cloned with
  `maxTokens` overridden before session creation (pi Model is a plain clonable interface).

### Subagent contract port (and the deliberate upgrade)

pi-relay `subagent_contract_text` (agent-daemon/src/subagents.rs) is ported with three
adaptations, all deliberate:

1. **Nested delegation sentence inverted.** pi-relay: "You cannot spawn nested delegations…
   those parent orchestration tools are unavailable to subagents." This stack: children keep
   the full RLM toolset and MAY spawn their own sub-agents with `rlm()` when depth permits.
   RLM semantics are the point of the new stack; depth-capped recursion replaces the flat
   orchestration ban.
2. Workspace-merge sentence dropped (children share the parent cwd; no copy-on-write
   workspace exists here).
3. Per-subagent-type workspace paragraph dropped (no full/read-only subagent split).

Role/preload prompt sections are pi-relay's `child_system_prompt` format verbatim:
`# Subagent role` (Role/Description/SKILL.md path/body) then one `# Preloaded skill: <name>`
section per preload (SKILL.md path + frontmatter-stripped body).

### Catalog in the parent prompt

prime-harness appends pi-relay's PI.md `### Packaged subagent roles` section (catalog JSON
port of `subagent_role_catalog_json`: `{"subagent_roles":[{name,description}]}`, sorted,
pretty-printed). Gating: pi-relay gates on the parent prompt profile (parents see it,
subagents don't); the new-stack equivalent is **depth < maxDepth** (can this session
delegate?), resolved via prime-rlm's `sessionInfo(sessionId).depth` vs `RLM_MAX_DEPTH`.
Consequence: a depth-0 parent sees the catalog, and any child that can still delegate sees it
too — a deliberate upgrade over pi-relay, where subagents never saw the catalog.

The catalog is rebuilt on `before_agent_start`; note pi only rebuilds the system prompt per
agent run, so a role created mid-run (e.g. via `rlm.harness.create_role`) appears in the
catalog on the NEXT run. Spawn-time resolution re-discovers roles on every `rlm.run` call, so
fresh roles are spawnable immediately regardless of catalog staleness.

### The "role" harness kind (additive schema extension)

`harness_state.json` gains a fifth entry kind, `role`, alongside prompt/memory/skill/subagent
— pi-relay `SkillKind::SubagentRole` as continual-harness content. Entry `content` is the
full SKILL.md role document; entry id must equal the frontmatter `name`.

Interchangeability (M2 goal) is preserved in both directions: old files lack the key and both
loaders default it to `{}`; new files carrying `role` entries are tolerated by PA's loader,
which iterates its own four `_KINDS` and ignores unknown keys. `rlm.harness.create_role /
update_role / delete_role` are thin wrappers over the generic CRUD (same as the other kinds).
The refine planner prompt gained one bullet teaching the role kind.

### Seed content + repairs

All nine pi-relay operator roles are seeded verbatim (plus a one-line YAML-comment provenance
header inside the frontmatter) from
`/home/schwinns/agent-config/pi-relay/runtime/subagent-roles/` into
`.pi/m1-demo/agent/roles/`: explore, implementer, merger, monitor, planner, reviewer, tester,
verifier, worker.

**Repair (documented):** pi-relay's `monitor/SKILL.md` carries `name: tester` — pi-relay's own
directory-name validation makes that role unusable there (latent bug; the real tester role
also exists and loads). The seed repairs it to `name: monitor`.

Two demo-only roles were authored (no pi-relay role exercises these paths):
`calc` (`skills: [demo-python]` preload for R1) and `flash-responder`
(`model: nvidia-inference/nvidia/deepseek-ai/deepseek-v4-flash` for R2).

## PA prompt restorations (delegated scope addition)

In `extensions/prime-rlm/src/prompt.ts`:

1. **Conversation log line** next to Working directory (PA rlm.ts:76), from
   `ctx.sessionManager.getSessionFile()`.
2. **Pre-installed packages + uv hint** lines, using `PREINSTALLED_PACKAGE_LABELS` exported
   from `src/provision.ts` (the orchestrator brought `EXTRA_REQUIREMENTS` to PA parity).
   NOTE: already-running demo kernels pick up newly pre-installed packages only after a
   kernel restart (provisioning happens at kernel boot, gated by the stamp hash).
3. **`# Delegating to sub-agents` block** after the rlm() contract — PA rlm.ts:178-198
   verbatim, with the comms/refine lines guarded by peer detection
   (`detectPeerExtensions()` over the globalThis seams; PA's
   hasAgentMessage/hasAgentObserve/includeRefineExamples flags). Two deliberate deviations:
   PA defaults includeRefineExamples to true because `refine.run()` is core there; here
   refine is a peer extension (prime-harness), so the default is false and an rlm-only load
   (C3 modularity) never mentions comms/refine.

Model-facing-text convention (orchestrator, now in skill-creator): prompt text is second
person and never names extensions/hosts/daemons. Applied retroactively ("your host delivers
…" → "your final assistant text is delivered to your parent when you go idle") and to all new
M7 prompt text.

## Verification (real GLM-5.2 unless noted; shim logs per trace)

**R0 — headless prompt render** (`/tmp/gen_prompt_m7.mjs`, 23/23 checks): Conversation log /
Pre-installed packages / uv lines; `# Delegating to sub-agents` present with comms+refine
lines when peers exist; rlm-only render omits agent_message/agent_observe/refine.run; child
render has `# Subagent contract` (parent session id, nested-delegation allowed), `# Subagent
role`, `# Preloaded skill: demo-python` with inlined body; child at maxDepth has no delegation
block; no "your host delivers" wording.

**R1 — catalog + role spawn + preload** (`m7-r1.json`, one attempt): parent printed
`ROLES_SEEN=calc,explore,flash-responder,implementer,merger,monitor,planner,reviewer,tester,verifier,worker`
from its own prompt; `HANDLE_ROLE=calc`, `HANDLE_MODEL=None` (no model policy → inherit);
child computed `demo_python.add(38,4)=42` with the kernel-pre-imported module and replied
`R1-PONG-42 HAS_ROLE_SECTION=yes HAS_PRELOAD_SECTION=yes`; prompt-builds.jsonl logged the
11-name catalog with zero invalid roles.

**R2 — role model override** (`m7-r2.json`): `flash-responder` child ran on
`nvidia/deepseek-ai/deepseek-v4-flash` (child session `model_change` entry; shim log
`m7-r2-shim.jsonl` shows deepseek requests; child self-reported
`FLASH_MODEL id=nvidia/deepseek-ai/deepseek-v4-flash` via `model.info`; admission handle
`model=nvidia-inference/nvidia/deepseek-ai/deepseek-v4-flash role=flash-responder`), while the
no-model `worker` role inherited the parent default (`WORKER_MODEL id=nvidia/zai-org/glm-5.2`).
Upstream reachability of the deepseek id through the shim was curl-verified before building R2
(`nvidia/` prefix required — plain `deepseek-ai/…` is key-denied). An earlier run with
`reasoning_effort: low` in the role produced degenerate deepseek tool-calling; removing it
(default medium) fixed the child. pi-relay-seeded roles pinning `openai:gpt-5.6-luna`
(explore, monitor) exercise the warn-and-fall-back path by design (not separately asserted).

**R3 — management + all origins** (`m7-r3.json`): `rlm.harness.create_role("greeter", …)`
persisted into the session-local `harness_state.json` (`entries.role.greeter`) and spawned
(`GREETER-OK`); scenario-written project role `.pi/roles/projrole/SKILL.md` (host cwd =
repo root) spawned (`PROJ-OK`) and appeared in prompt-builds catalog; seeded global `worker`
spawned (`WORKER-OK`); `rlm.list_subagents()` printed `r3greeter:greeter, r3proj:projrole,
r3worker:worker` (registry role field through the python client).

**R4 — regression** (`m7-r4.json` + `m7-r4-m3r1.jsonl`): plain `rlm.run` spawn unaffected
(`PLAIN_HANDLE_ROLE=None`, `PLAIN-PONG` round-trip); M3's R1 fanout scenario re-ran green.

## Files

* prime-rlm: `src/roles.ts` (NEW — seam consumer, selector parsing, spawn resolution),
  `src/registry.ts` (`role` field, pending-role map), `src/prompt.ts` (contract/role/preload
  sections + PA restorations), `src/rlm-host.ts` (`role` kwarg, admission resolution, model
  precedence, thinkingLevel/maxTokens in createChildSession, registry/lifecycle/payloads),
  `index.ts` (prompt options, role kernel pre-import contribution),
  `python/.../prime_rlm_runtime/__init__.py` (`role` on RLMSpawnHandle/RLMSubagent, docstring).
* prime-harness: `src/roles.ts` + `src/host-api.ts` (NEW), `src/store.ts` ("role" kind),
  `index.ts` (seam bind, catalog section, prompt-builds roles logging),
  `python/.../prime_harness_runtime/harness.py` ("role" kind + CRUD wrappers).
* prime-comms: `index.ts` (`subagentRole` in describeChild/tree rows).
* demo: `agent/roles/` seeds (9 pi-relay + calc + flash-responder), `agent/models.json`
  (deepseek entry), `shim-proxy.mjs` (optional `SHIM_LOG` JSONL request log), `run-m2.sh`
  (per-trace SHIM_LOG), `prompts/m7-r{1,2,3,4}.txt`, `scenarios/m7-r{1,2,3,4}.json`.
* `.pi/roles/projrole/SKILL.md` at the repo root is the R3 project-origin artifact (host cwd);
  left in place as evidence.

## Known limits / notes

* Role catalog validity does not include registry availability of the role's model (a
  spawn-time concern with a defined fallback), matching pi-relay's catalog-vs-spawn split.
* Invalid roles are invisible in the catalog; they surface in `prompt-builds.jsonl`
  (`invalidRoles`) and as descriptive spawn errors.
* `rlm.run` admission-handle `model` shows the resolved OVERRIDE (caller or role), not the
  effective model — unchanged M3 semantics for plain spawns (null = inherit parent).
* The catalog prompt section requires prime-harness; role SPAWNING additionally requires it
  (the seam). rlm-only mode: plain `rlm.run` unaffected, `role=` errors cleanly.
