# M7 progress log — subagent roles

## Checks (all green)

| # | Check | Evidence |
|---|-------|----------|
| R0 | Headless prompt render: Conversation log line, Pre-installed packages + uv lines, `# Delegating to sub-agents` (peers), rlm-only omits comms/refine, child contract/role/preload sections, new wording convention | `/tmp/gen_prompt_m7.mjs` — 23/23 |
| R0b | Role discovery headless: 11 valid roles, monitor repaired, calc preload resolves python import, unknown-role error lists known names | `/tmp/roles_sanity.mjs` |
| R1 | Catalog in parent prompt + `role="calc"` spawn + kernel preload use | trace `m7-r1.jsonl`; `R1-PONG-42 HAS_ROLE_SECTION=yes HAS_PRELOAD_SECTION=yes`; `HANDLE_ROLE=calc`; prompt-builds.jsonl roles=11 invalid=0 |
| R2 | Role model override (`flash-responder` → deepseek) vs default (`worker` → glm) | trace `m7-r2.jsonl`; `m7-r2-shim.jsonl` (deepseek×3, glm×9); child `model_change` entries; `FLASH_HANDLE model=…deepseek… role=flash-responder`; `model.info` self-reports |
| R3 | `rlm.harness.create_role` (harness-local) + `.pi/roles` (project) + seeded (global-dir); registry/list_subagents role field | trace `m7-r3.jsonl`; `CREATED_ROLE id=greeter kind=role`; `r3greeter:greeter,r3proj:projrole,r3worker:worker`; GREETER-OK/PROJ-OK/WORKER-OK |
| R4 | Plain `rlm.run` regression + M3 R1 fanout regression | traces `m7-r4.jsonl` (`PLAIN_HANDLE_ROLE=None`), `m7-r4-m3r1.jsonl` |
| T | tsc clean: prime-rlm, prime-harness, prime-comms; py_compile clean: prime_rlm_runtime, prime_harness_runtime | 2026-08-10 |

## Incidents / fixes during M7

- rlm-host.ts edit left a stray backtick (syntax error) that broke extension loading and
  blocked m6-frontend's F2 test — fixed immediately; policy: keep prime-rlm parseable between
  edits (tsc after each batch).
- rlm-host.ts:233 `Model<any>` clone on possibly-undefined `ctx.model` — guarded.
- refine-guard default: PA's `includeRefineExamples ?? true` is core-refine semantics; this
  stack requires explicit peer detection (default false) — fixed + render check added.
- R2 first attempt: deepseek-v4-flash at `reasoning_effort: low` produced degenerate output
  (no tool call). Removed the effort pin from the demo role (default medium) → green.
- m7-r3 scenario used `text` for the driver writeFile op; the op takes `content` — fixed.
- Stale pre-M7 shim process held port 8571 (no SHIM_LOG); killed; run-m2.sh now truncates a
  per-trace shim log.

## Deliberate divergences (documented in M7-ROLES.md)

1. Nested delegation allowed for children (pi-relay forbade) — RLM semantics.
2. Project `.pi/roles` + harness-stored roles are new origins; precedence-ordered shadowing
   replaces pi-relay's duplicate-error (single-origin) doctrine.
3. Catalog visible to any session that can delegate (depth < maxDepth), not only depth-0.
4. pi-relay monitor role repaired (`name: tester` → `name: monitor`).
