# M10b — W2 cache-economics bundle (pi-relay → prime-agent migration)

Scope: pre-soak cost work deciding M11 soak cost. Stack under test:
`packages/bridge` (@pi-relay/bridge) spawning `pi --mode rpc` hosts
(@earendil-works/pi-coding-agent **0.84.1**, pinned) + extensions
prime-{rlm,harness,comms,autonomy}. Demo/test env: `.pi/m1-demo/`.

## Q1 — Which models matter for cutover; does the shim/NVIDIA path support prompt caching?

**Money models at cutover** (owner's live `~/.pi/agent/settings.json`):
default `anthropic/claude-opus-4-7`; enabled gpt-5.4 / gpt-5.4-mini /
claude-opus-4-6 / claude-sonnet-4-6. Auth (`auth.json`) = **OAuth for
anthropic + openai-codex**. So cache economics at cutover are Anthropic
Claude (OAuth Bearer) + OpenAI Codex (OAuth).

**Test/demo models**: `nvidia/zai-org/glm-5.2` + `nvidia/deepseek-ai/deepseek-v3.2`
via local shim `http://127.0.0.1:8571/v1` (`api: openai-completions`, cost 0;
shim forwards to `integrate.api.nvidia.com/v1/chat/completions` with its own
env-piped key — I hold no key and never see one).

**Does the NVIDIA/shim path support prompt caching? YES — automatic
server-side prefix caching, no client contract required.** Empirical probe
through the running shim: two back-to-back calls sharing a 4000-token prefix →
second call reports `prompt_tokens_details.cached_tokens: 4000`
(`cacheRead` in pi-ai usage). Explicit contract fields
(`prompt_cache_key`, `prompt_cache_retention:"24h"`) are **tolerated**
(200 OK) but meaningless on NVIDIA.

Upstream evidence this matters: pi-coding-agent 0.84.1 CHANGELOG
("Fixed inherited Fireworks GLM 5.2 requests sending the unsupported
`prompt_cache_retention` field when long cache retention is enabled, and
enabled session affinity for automatic prompt caching", #7676 / commit
b9497c8c1). GLM-hosting providers may **reject** `prompt_cache_retention`;
the correct pattern for GLM hosts is automatic caching + session affinity,
not explicit cache fields. Our shim provider (`nvidia-inference`, baseUrl
127.0.0.1:8571) detects as *generic* OpenAI-compatible →
`supportsLongCacheRetention` defaults **true** → with long retention enabled
pi-ai would send those fields to NVIDIA. Harmless today (tolerated), fragile
by upstream's own evidence → compat override implemented below.

SSE note: shim synthesizes SSE from non-streaming JSON (NVIDIA streaming
broken per M1 findings); usage chunk appended only when
`stream_options.include_usage` is requested — pi-ai requests it
(`supportsUsageInStreaming` detected true), so cacheRead accounting flows.

## Q2 — Cache primitives in pi-ai / pi-coding-agent 0.84.1; what our stack plumbs

pi-ai 0.84.1 (nested under pi-coding-agent's node_modules):

- `StreamOptions.cacheRetention?: "none"|"short"|"long"`,
  `StreamOptions.sessionId?: string`,
  `Model.compat.supportsLongCacheRetention?: boolean` — also settable
  per-provider/per-model via `models.json` `compat` (schema-verified).
- `resolveCacheRetention()` = explicit option → `PI_CACHE_RETENTION` env
  (only `"long"` recognized) → default `"short"`. Env read **per request**
  via `getProviderEnvValue` (falls back to `process.env`).
- Provider behavior:
  - **anthropic-messages**: when retention ≠ none, stamps
    `cache_control {type:"ephemeral"}` at 3 breakpoints (system tail block,
    last tool, last user-message block); adds `ttl:"1h"` when long +
    `supportsLongCacheRetention` (compat default true for Claude models).
    Parses `cache_read_input_tokens` / `cache_creation_input_tokens`
    (incl. `ephemeral_1h_input_tokens` split) → cacheRead/cacheWrite/
    cacheWrite1h with 5m-vs-1h write pricing.
  - **openai-completions** (our shim api): sends `prompt_cache_key` only for
    api.openai.com OR (long + supportsLongCacheRetention);
    `prompt_cache_retention:"24h"` when long + supported. Parses
    `prompt_tokens_details.cached_tokens` → cacheRead.
  - **openai-codex-responses** (owner's Codex OAuth):
    `prompt_cache_key = sessionId` whenever retention ≠ "none" — i.e.
    **by default**; cache affinity via stable session id.
- `sessionId` plumbing: pi-agent-core `Agent.sessionId`; sdk
  `createAgentSession` passes `sessionManager.getSessionId()` into stream
  options → Codex cache affinity works by default in our RPC hosts.
- Extension seam (not needed here, noted for future fingerprint work):
  `Agent.onPayload` is public/mutable; sdk wires it to the
  `before_provider_request` hook; RPC path (main.js →
  agent-session-services.js → sdk.js) includes it → hooks fire for all
  providers in `pi --mode rpc` hosts.
- **count_tokens / countTokens: DOES NOT EXIST** in pi-ai or pi-coding-agent
  0.84.1 (grep-verified across both dists). The local gate instead:
  `estimateTokens` (chars/4) + usage-anchored `calculateContextTokens` +
  `shouldCompact` (contextTokens > contextWindow − reserveTokens). Demo
  settings.json exercises it (reserveTokens 16384, keepRecentTokens 250).

**What our stack plumbs today: nothing.** No cacheRetention knob anywhere in
packages/bridge (grep-verified). Hosts inherit the bridge process env, so a
global `PI_CACHE_RETENTION` on the bridge would leak to all hosts — too
coarse. Per-session control needs the plumbing added below.

## Q3 — What does PA do that we lack?

- **Compaction cache-isolation (upstream 9b3a2059, "isolate summarization
  requests")**: adds `cacheRetention:"none"` + fresh `sessionId:uuidv7()` to
  summarization requests. NOT an ancestor of PA main (merge-base verified —
  PA's fork predates it). **But our pinned pi-coding-agent 0.84.1 already
  contains it**: upstream 0.82.0 CHANGELOG ("Fixed compaction and
  branch-summary requests to use fresh routing session IDs with prompt
  caching disabled where supported", #6618; present at line 331 of the pinned
  package's CHANGELOG) + dist-verified
  (`dist/core/compaction/compaction.js:444`, pi-agent-core
  `harness/compaction.js:57`). **Cherry-pick = NON-GOAL — inherited via the
  0.84.1 pin.**
- **Deep breakpoint placement + Anthropic attribution fingerprint**: absent
  in PA *and* upstream — these are pi-relay-only inventions
  (CONTEXT-ENGINEERING §10.2: "Mechanism parity… deep breakpoint placement +
  attribution fingerprint remain pi-relay-only"). Upstream's
  "provider-attribution headers" are telemetry/routing headers
  (x-session-affinity et al.), a different thing. Nothing to port here;
  recorded for M11+ planning.
- PA's own cache code is mechanism parity with upstream 0.84.1 (same
  `resolveCacheRetention` / 3-breakpoint anthropic stamp; same
  `PI_CACHE_RETENTION` env). PA's coding-agent **never passes**
  cacheRetention (grep-verified) and PA has **no count_tokens**. → PA has
  nothing cache-wise that our stack lacks.

## Decision — implement only what's real

| # | Item | Verdict |
|---|------|---------|
| 1 | `BRIDGE_PI_CACHE_RETENTION` env knob → validated `none|short|long` → per-session host env `PI_CACHE_RETENTION` (sessionEnv → host.start extraEnv). Default unset = upstream "short" unchanged. | **IMPLEMENT** (field exists upstream; per-provider behavior verified) |
| 2 | GLM shim provider compat hygiene: `supportsLongCacheRetention: false` in `.pi/m1-demo/agent/models.json` (upstream #7676 pattern). | **IMPLEMENT** (prevents meaningless/fragile fields leaking to NVIDIA when long enabled) |
| 3 | Compaction cache-isolation cherry-pick (9b3a2059) | **NON-GOAL** — already in pinned 0.84.1 (dist + CHANGELOG verified) |
| 4 | count_tokens-ish preflight gate | **NON-GOAL** — no primitive exists upstream; local usage-anchored gate already exists; a raw Anthropic count_tokens client over OAuth would be speculative new machinery |

## Evidence

### Code changes (all behind defaults that leave current behavior untouched)

| File | Change |
|------|--------|
| `packages/bridge/src/config.ts` | `parseCacheRetention()` (validated `none\|short\|long`, empty→null, invalid→boot-time throw) + `config.piCacheRetention` from `BRIDGE_PI_CACHE_RETENTION` |
| `packages/bridge/src/supervisor.ts` | `sessionEnv()` (now exported) threads `PI_CACHE_RETENTION` into per-session host env; `host.ts:69` already merges `extraEnv` into the spawn env |
| `.pi/m1-demo/agent/models.json` | provider-level `compat.supportsLongCacheRetention: false` on `nvidia-inference` (upstream #7676 pattern for GLM hosts: automatic prefix caching, no explicit cache fields) |
| `packages/bridge/test/m10-cache-config.test.mjs` | NEW — unit tests (node --test), no bridge/PG needed |
| `packages/bridge/test/m10-recording-stub.mjs` | NEW — loopback OpenAI+Anthropic-shaped stub, records bodies (never headers) |
| `packages/bridge/test/m10-cache-payloads.mjs` | NEW — boots real `pi --mode rpc` 0.84.1 hosts per case, diffs wire payloads |

### Verification (deterministic, offline)

- `npm run typecheck` (tsc --noEmit): **clean**.
- `node --test test/m10-cache-config.test.mjs`: **5/5 pass** — parse
  literals/garbage; default unset → no `PI_CACHE_RETENTION` in sessionEnv;
  configured → threaded (child-process, env set pre-import); invalid → boot
  fails loudly; M8 workspace/MCP env behavior unchanged.
- `node test/m10-cache-payloads.mjs`: **PASS** — 5 host boots × 1 turn each
  against the loopback stub; normalized records in
  `.pi/m1-demo/traces/m10b-prompt-builds.jsonl` (raw bodies in
  `packages/bridge/test/out/m10/raw-requests.jsonl`):

  | case | env | result |
  |------|-----|--------|
  | openai-default | — | no cache fields on the wire |
  | openai-long | `PI_CACHE_RETENTION=long` | `prompt_cache_key`=<session uuid> + `prompt_cache_retention:"24h"` sent (upstream raw behavior on a generic provider — the leak #7676 fixed for Fireworks) |
  | openai-long-compat | `long` + compat override | **byte-clean** — no cache fields (our models.json shape) |
  | anthropic-default | — | 3 `cache_control {type:"ephemeral"}` breakpoints (system tail, last tool, last user block), **no ttl** → 5-minute write pricing |
  | anthropic-long | `PI_CACHE_RETENTION=long` | same 3 breakpoints **+ `ttl:"1h"`** → 1-hour write pricing |

### Real GLM turns (through the live local shim → NVIDIA, key env-piped only)

- Default env: `M10B-REGRESSION-OK`, usage input=5189 output=125 **cacheRead=32** —
  trace `.pi/m1-demo/traces/m10b-regression-default.jsonl`. Regression: unchanged.
- `PI_CACHE_RETENTION=long`: `M10B-REGRESSION-OK`, cacheRead=32 — trace
  `.../m10b-regression-long.jsonl`. New compat field parses on a real boot
  with all 4 prime extensions loaded; NVIDIA automatic prefix-cache
  accounting (`cached_tokens` → `cacheRead`) flows end-to-end through pi.

### M11 soak implications

- **Claude OAuth (cutover default)**: default "short" already gives 3
  ephemeral breakpoints; `BRIDGE_PI_CACHE_RETENTION=long` upgrades all three
  to 1h TTL (2x write price, ~10x longer read window) — the knob is now
  bridge-level and per-session-host, ready for soak A/B.
- **Codex OAuth**: `prompt_cache_key = sessionId` already flows by default;
  nothing to do.
- **GLM/DeepSeek via shim (test fleet)**: free automatic caching; payloads
  stay clean under any retention setting; compaction/branch-summary requests
  are cache-isolated (`none` + fresh uuid) by the pinned 0.84.1 — they will
  not poison or evict the main conversation cache chain.
- **No count_tokens gate**: context pressure uses the existing usage-anchored
  local estimate + `shouldCompact` threshold. If a real preflight primitive
  appears upstream later, revisit; building one over OAuth now would be
  speculative machinery.
