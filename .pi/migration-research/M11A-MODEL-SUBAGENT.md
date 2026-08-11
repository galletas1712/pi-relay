# M11a: model surface + subagent drill-down + comms visibility (contract v0.2)

**Status: complete. V1–V6 all PASS** (2026-08-10; traces:
`.pi/m1-demo/traces/m11a-v1-models-{test,dogfood}.jsonl`,
`m11a-v2v3-dogfood.jsonl`, `m11a-v4v5-{comms,replay}.jsonl`; scenario:
`packages/bridge/test/m11a.mjs`; regression: b1–b8 + m9-r1..r5 green, web
vitest 713 passed / 0 failed, `tsc -b` + vite build + CSP check green).

Additive contract v0.2 on the M5 bridge: the model surface (list/set/create
with pi-runtime snapshot semantics), a per-child transcript reader, and comms
visibility — plus the web surfaces for all three. Nothing in v0/v0.1 changed.

## Bridge (packages/bridge)

- **`models.list {refresh?}`** (`src/models.ts`): in-bridge `ModelRuntime`
  probe constructed exactly like the hosts' (`auth.json` + `models.json` +
  settings, `allowModelNetwork:false`), 15 s TTL cache with stale-on-error.
  Returns `{models[], providers[], defaults, thinkingLevels[]}`; `available` /
  `authConfigured` mark selectability (credential presence only, like pi).
- **`session.setModel`** — rpc `set_model` passthrough, no busy guard (pi
  allows mid-turn). Emits new `session.model {provider, modelId, name,
  thinkingLevel}` + re-emits `session.state` (pi itself emits nothing over
  rpc on set_model). Persists via pi (`model_change` entry + settings.json).
- **`session.setThinkingLevel`** — reconciled via `get_state` because upstream
  clamps silently; returns `{thinkingLevel, availableLevels[]}` from
  `get_available_thinking_levels`.
- **`session.create {model?: "provider/id"}`** — applied before the session
  row is inserted; failure → typed error, no orphan row.
- **`subagent.transcript {sessionId, childId}`** (`src/routes/subagent.ts` +
  `src/agentfiles.ts`): M8 JSONL v3 tree-walk over the child session file
  (`data/sessions/sub-<rlmId8>/…`), returns `{childSessionId, sessionFile,
  blocks[], replCells[]}`. Load-on-open + client refresh; no fs.watch (child
  AgentSessions are in-process). `subagent_not_found` while the file is not
  yet flushed right after admission — clients retry.
- **`comms.list {sessionId}`** (`src/routes/comms.ts`): inbound (session-file
  `custom_message`/`agent_message` entries) + outbound (outbox fold by id,
  last record wins, torn tails skipped), merged and ts-sorted. A session that
  never sent has no outbox → `[]`.
- **Events**: `session.model` (null-field merge semantics — a
  `thinking_level_changed` host event may carry only `thinkingLevel`) and
  `comms.message` in both directions: outbound from the ONE
  `prime_comms_message` appendEntry per terminal status in prime-comms
  `notifyMessageListeners` (sender's binding, `queued` excluded, redrive
  silent); inbound mapped in the supervisor from the `message_start`
  `role:"custom"` `agent_message` the host receives (`direction:"in"`). All
  other custom-role messages are suppressed — they never render.
- **Errors**: `model_not_found`, `model_unavailable`, `subagent_not_found`.

## Web (packages/web/src/bridge)

- **ModelPicker.tsx** (header, per owner steer): compact icon-first
  `NativeSelect` pair (Cpu anchor = model, Brain anchor = thinking level),
  provider-grouped optgroups, unauthed/unavailable entries disabled with "(no
  auth)", optimistic set + `session.getState` reconcile, revert on error.
- **SubagentPanel.tsx** — drill-down from clickable SubagentTreeView rows:
  back/refresh icon buttons, assistant = plain chat flow, tool rows + repl
  cells collapsed-by-default chevron rows, unknown entries render nothing.
- **TranscriptView CommsView** — slim expandable row, `MessagesSquare` icon,
  orange `--primary` gruvbox accent, "agent from → here / → to" + one-line
  preview collapsed, full text + delivery status expanded.
- Data layer: eventStore `session.model` merge + `CommsBlock`, sessionStore
  `reconcileModel`/`applyOptimisticModel`, hooks `useModelsList` /
  `useSubagentTranscript` / `useCommsList`, client methods for all five v0.2
  methods.

## Verification (all traces spooled)

- **V1** models.list on both rigs: 1222 models/41 providers (test, GLM custom
  `nvidia-inference` available via env) and 1220/40 (dogfood, anthropic +
  openai-codex authed; codex catalog gpt-5.3-codex-spark … gpt-5.6-terra).
- **V2** (dogfood): setModel gpt-5.6-luna→gpt-5.6-sol with `session.model`
  event + getState reconcile; setThinkingLevel→xhigh with event +
  availableLevels; real turn answered on the switched model; session file
  carries `model_change` + `thinking_level_change`; typed errors for unknown
  id / unauthed provider / bogus level.
- **V3**: `session.create{model}` result carries the applied model; invalid
  model → `model_not_found` and NO orphan session row.
- **V4** (GLM, real rlm child): transcript while RUNNING (retried through
  `subagent_not_found` right after admission) and after completion — assistant
  text, 2–3 done repl cells, kernel computed 17*23=391; unknown childId →
  `subagent_not_found`.
- **V5** (GLM, real agent_message both ways): outbound `comms.message`
  (role=child, receiverName, terminal `delivered`) + inbound
  `comms.message{direction:"in"}` identifying the sender; parent processed the
  child reply in a follow-up turn; `comms.list` = 2–3 messages ts-sorted,
  outbound terminal statuses only; events replay on fresh attach from seq 0.
- **V6**: web `tsc -b` + vitest (713) + vite build + built-CSP check.

## Upstream findings worth remembering

- rpc `set_model` gates on the **available** snapshot: models from providers
  without auth are "Model not found" BEFORE auth is consulted. The bridge maps
  that to `model_not_found`; `model_unavailable` is reserved for the post-gate
  "No API key for …" failure. `models.list` `available` flags are the UI
  signal. (One test assertion was initially written against the wrong
  assumption; fixed to upstream truth.)
- `set_thinking_level` returns plain success and clamps silently — always
  reconcile via `get_state`. `thinking_level_changed` on the wire carries only
  `{level}`.
- `pi.appendEntry` DOES emit `entry_appended` on the rpc stream
  (`session.subscribe` → `toJsonEvent` passes everything through except
  `message_update.partial`) — that is the outbound comms event pipe.
- pi emits `model_select` only to extension runners, never over rpc — the
  bridge's explicit `session.model` event is required for clients.
- `checkProviderAuth` is credential PRESENCE only (expired OAuth reports
  configured) — the probe mirrors it deliberately.

## Operational notes

- Bridge boot is O(accumulated non-closed sessions): reconcile respawns every
  host sequentially before listening. 63 idle test-rig sessions ≈ 46 s, which
  crossed the old 45 s `waitHealthy` in the test harness (bumped to 180 s,
  test-only). A future `session.close`/prune knob would keep this in check.
- Dogfood pid file was stale once this milestone (real listener held by an
  older process); verify listeners via `ss -tlnp`, not the pid file.
