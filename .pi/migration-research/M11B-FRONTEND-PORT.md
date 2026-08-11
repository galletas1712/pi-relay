# M11b frontend port — completion record (m11b2)

The legacy SPA is ported wholesale onto the bridge contract: `?backend=bridge`
mounts the untouched legacy `<App/>` fed by `BridgeAgentApi implements AgentApi`
(`packages/web/src/bridge/`). The legacy profile and the rust-backend path are
untouched (additive props only: `renderReplPane`, dynamic model options).

## Adapter architecture
- `BridgeAgentApi` (legacyApi.ts): projects/sessions/transcript/history/
  workspace/git/MCP/tools/delegation mapped to bridge RPC; queue mgmt,
  user-launched delegation, metadata writes fail typed-unsupported.
- `legacyEvents.ts`: bridge spool events → legacy EventFrames (echo FIFO, queue
  snapshot, drift→resync).
- `legacySessions.ts`: SessionStore; synthesized entries for live append;
  `reportedLeafId = store.leafId ?? store.branchTipId` — the legacy daemon's
  active leaf is the REAL branch tip, never null mid-history (m11b2 fix for the
  rewind-to-first-boundary "Session refresh failed" fence rejection).
- `legacyTranscript.ts`: bridge blocks → legacy entries + turn cards via the
  legacy appendTurnCard fold.
- Model picker re-homed to `models.list` (available===true filtered at the
  adapter); `session.getState` grew `branchTipId` (contract v0.3 additive).

## Owner-rule conformance highlights
- ipython I/O renders inline inside the "Used N tools"/"Show details" group
  (input block + EntryId badge, ANSI output block); no per-tool dropdown.
- Auth-only model picker: exactly the authenticated models reach the UI.
- /fork = duplicate + real btrfs snapshot; /switch = user-message boundaries
  with composer prefill and true rewind semantics.

## Gates (all green, 2026-08-10)
- m11b-dom.mjs: 45/45 checks, both rigs, real models (traces
  m11b2-dom-test.jsonl / m11b2-dom-dogfood.jsonl).
- vitest 714 passed / 5 skipped; web tsc + bridge tsc clean; `npm run build`
  (both profiles, CSP check) exit 0.
- run-all.sh b1–b8, m9-r1..r5, m11a, m11b: ALL PASS.
- Acceptance mapping with per-item evidence: `.pi/m1-demo/M11B-PROGRESS.md`
  (## m11b2 continuation); the acceptance checkboxes in M11B-ACCEPTANCE.md are
  the orchestrator's to tick.
