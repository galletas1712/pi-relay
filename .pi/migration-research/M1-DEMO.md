# M1 Demo Report — prime-rlm on unpatched upstream pi (0.84.1)

**Date:** 2026-08-09 · **Milestone:** M1 (spikes S1+S2 from IMPLEMENTATION-PLAN.md) · **Result: ALL PASS (V1–V4 + depth-guard bonus)**

## TL;DR

prime-agent's two core differentiators run as a **pure extension package** on the pinned,
unpatched upstream `@earendil-works/pi-coding-agent@0.84.1` (npm install, `--mode rpc`):

- **(a) persistent IPython kernel as the model's only tool** — `extensions/prime-rlm` registers
  an `ipython` tool; `--tools ipython` (CLI allowlist) / `tools: ["ipython"]` (SDK) restricts the
  tool surface exactly, including extension tools.
- **(b) in-kernel `rlm()` spawning in-process child agent sessions** — a Jupyter comm
  (`host.request`) bridge from the kernel to a TS host handler that runs
  `SessionManager.create` → `createAgentSession({tools:["ipython"], ...})` → `bindExtensions({})`
  → `prompt()` → await `agent_settled`, **blocking** until the child finishes and returning its
  final text (M1-minimal semantics; PA returns at admission).
- **(c) RLM system prompt** — `before_agent_start` handler returns `{systemPrompt}`, replacing
  pi's default coding prompt entirely.

Deliverables:
- Extension: `extensions/prime-rlm/` (index.ts + src/{kernel,provision,ipython-tool,rlm-host,registry,prompt}.ts + `python/prime-rlm-runtime/`)
- Demo: `.pi/m1-demo/` (`run-demo.sh`, `driver.mjs`, `shim-proxy.mjs`, `agent/`, `venv/`, `traces/`)
- Traces: `.pi/m1-demo/traces/{v1,v2,v3,v3b,v4,v5}.jsonl` (+ `.out` final-text logs); sessions in `.pi/m1-demo/sessions/`

## Verification matrix (GLM-5.2, nvidia-inference)

| Run | Verifies | Trace | Result |
|-----|----------|-------|--------|
| V1 | Python exec via ipython tool | `v1.jsonl` | PASS — printed `SQUARES_SUM 285` |
| V2 | `%%bash` cells | `v2.jsonl` | PASS — printed `BASH_TOKEN_42` |
| V4 | kernel state persists across calls | `v4.jsonl` | PASS — 2nd cell read `marker_var` → `PERSIST_57` |
| V3 | `rlm()` child spawn + registry | `v3b.jsonl` | PASS — `{status:'completed', result:'144'}` (F(12)=144 ✓); `rlm.list_subagents()` returned 1 entry; child session at `sessions/rlm-children/019fe574/sub-c3136fd3/` shows `[task from parent]` prompt + its own ipython call |
| V5 | depth guard error path | `v5.jsonl` | PASS — `RLM_MAX_DEPTH=0` → `RuntimeError: RLM recursion depth limit reached (RLM_DEPTH=0, RLM_MAX_DEPTH=0)` surfaced in-kernel |

(`v3.jsonl` is the pre-fix run — kept because it shows the envelope bug AND GLM successfully
self-debugging the kernel runtime, reading the installed package source and hot-patching it.)

## Upstream API surface used (all public, no patches)

- `ExtensionAPI`: `registerTool`, `on("session_start" | "before_agent_start" | "session_shutdown")`
- `ToolDefinition` incl. `executionMode: "sequential"` (single kernel — prevents parallel in-batch calls)
- `BeforeAgentStartEvent` → `{systemPrompt}` replacement (runner.ts `emitBeforeAgentStart`,
  agent-session.ts:1232 applies it per run)
- SDK: `createAgentSession({cwd, agentDir, model, thinkingLevel, sessionManager, tools, sessionStartEvent})`,
  `SessionManager.create(cwd, sessionDir)`, `session.bindExtensions({})`, `session.prompt(text,
  {source: "extension"})`, `session.subscribe(...)` (`agent_settled`), `session.messages`,
  `session.dispose()`, `session.modelRuntime`
- `ExtensionContext`: `sessionManager.getSessionId()/getSessionDir()`, `model`, `modelRegistry.find()/
  hasConfiguredAuth()`, `thinkingLevel`, `cwd`
- settings: `<agentDir>/settings.json` → `extensions: [<abs path>]`, `defaultProvider/defaultModel`;
  `PI_CODING_AGENT_DIR` env isolates the demo agentDir
- models.json `apiKey: "$NVIDIA_INFERENCE_API_KEY"` — env refs confirmed resolved through
  `provider-composer.ts:351 resolveConfigValueOrThrow` (same mechanism as auth-storage `!` commands)

### The one non-obvious wiring fact

`createAgentSession` runs extension factories (so handlers/tools register) but does **not** emit
`session_start` — that's done by `bindExtensions()`, which only the mode entrypoints
(rpc/print/interactive) call. In-process child sessions must call `await child.bindExtensions({})`
themselves (bindings all optional; no UI context → `hasUI=false`, mode defaults to `"print"`).

### Module-state hazard (documented for M2+)

The extension loader caches the extension **module** per path but re-executes the **factory** per
AgentSession. Parent + in-process children therefore share module-level state — all cross-session
state in prime-rlm is keyed by `sessionId` (`registry.ts`), and child depth is pre-registered
(`registerPendingDepth(childSessionId, depth+1)`) *before* `createAgentSession` runs the factory.

## Adaptations from PA (what was stripped for M1)

| PA subsystem | M1 status |
|---|---|
| KernelManager (zeromq wire, conn file, exec queue, iopub truncation, interrupt, host-request dispatch) | **ported nearly whole** (`src/kernel.ts`) |
| fork-server kernel spawn | dropped (direct `python -m ipykernel_launcher`) |
| state snapshots / restore | dropped (known gap) |
| attachments/diff/agent-message MIME rendering | dropped |
| bootstrap lock, skill-sync, `PRIME_AGENT_INSTALL_UV` | dropped; simple stamp-file provisioner keyed on runtime-package content hash (`src/provision.ts`); `PRIME_RLM_KERNEL_PYTHON`/`PRIME_RLM_KERNEL_VENV` overrides |
| `rlm.run` admission-async + daemon registry | **blocking** run + module-level registry; `rlm.list_subagents` returns registry entries |
| harness/mcp/find_models host handlers | dropped |
| full RLM prompt (harness, skills, messaging) | minimal port (`src/prompt.ts`) |
| busy-kernel UI prompt | dropped (rpc/print: busy kernel surfaces an error) |

## GLM / endpoint quirks found

1. **NVIDIA inference-api streaming is broken endpoint-wide right now** (glm-5.2, deepseek-v4-flash):
   `stream:true` returns a single chunk `delta:{}` + `finish_reason:"stop"` with *zero content*
   (usage chunk shows the server generated tokens it never streamed). Non-streaming works fine.
   pi-ai is streaming-native → **workaround: `.pi/m1-demo/shim-proxy.mjs`**, a local adapter
   (127.0.0.1:8571) that strips `stream`, forwards non-streaming, and synthesizes spec-shaped SSE
   (reasoning_content deltas, content deltas, tool_calls, finish chunk, usage chunk, [DONE]).
   models.json `baseUrl` points at the shim. This is demo harness, not a pi patch.
2. With working transport, GLM-5.2 (reasoning=true) drove the tool loop cleanly: single ipython
   call per task, correct `%%bash` usage, and (in the broken-envelope run) remarkably debugged the
   kernel runtime itself — read site-packages source, hot-patched `host_request`, retried
   successfully.
3. `reasoning_content` maps to thinking blocks via pi-ai's openai-completions parser — no
   `reasoning=false` fallback was needed.

## Bug found & fixed during bring-up

Host-bridge envelope collision: `rlm.run`'s result carries `status:"completed"`, which clobbered
the bridge's own `status:"ok"` envelope field (`{status:"ok", ...result}`). Fixed by nesting the
payload: `{status:"ok", value: result}` + Python unwraps `reply["value"]`. **PA should be checked
for the same hazard** — any PA host handler returning a `status` key hits the same spread order in
`packages/coding-agent/src/core/kernel/index.ts` (`sendHostReply(commId, {status:"ok", ...result})`).

## P1 spec (usage attribution) — status: hook point identified, not implemented

- **Where:** `rlm-host.ts` `runRlmChild` — the `child.subscribe(...)` block has a marked
  `P1 HOOK` comment: `message_end` events with `message.role === "assistant"` carry
  `message.usage` per child turn.
- **What's missing upstream:** pi has no `sessionManager.appendChildUsageAttribution` (PA fork
  addition) and no session-entry type to record child-usage-against-parent-turn. Extension-side
  workarounds all fall short: `appendEntry` writes custom entries but the cost/accounting
  rollups won't attribute them to the parent's last assistant message.
- **Fork-forcing?** For M1's purposes: **no** — children are observable, usage is inspectable
  live via the event stream. For PA-parity cost accounting (child usage folded into parent turn):
  **yes, still a fork patch** (P1), or an upstream feature request for an attribution entry API.

## Other fork-forcing issues: none found for M1 scope.

Everything else (kernel tool, child spawning, prompt replacement, tool allowlisting, model/auth
resolution, rpc driving) is extension-clean on 0.84.1.

## Gaps / not-yet-done (M1 acceptance vs PA parity)

- rlm.run is blocking (PA: admission-async with observable running state); no concurrent children
- no state snapshots, no fork-server, no daemon registry, no session-name UI surfacing
- no busy-kernel recovery UX (busy kernel → error string)
- child sessions are one-shot (disposed after settle); no follow-up to an existing child
- `reasoning=false` fallback untested (not needed)
- V3's child depth is structural (registry/prompt plumbing verified by code path + V5 guard);
  a child reporting its own depth in-text was not separately asserted
- npm-install layout nuance: `@earendil-works/pi-ai` etc. are nested under
  `pi-coding-agent/node_modules` — fine at runtime (jiti aliases to host copies) but extension
  typechecking needs symlinks (see `extensions/prime-rlm/node_modules` symlink farm).

## Reproduce

```bash
cd /home/schwinns/pi-relay/.pi/m1-demo
./run-demo.sh prompts/v3.txt v3b   # needs /home/schwinns/inference_hub_key
```
