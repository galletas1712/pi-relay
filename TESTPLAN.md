# Phase 2 / Phase 3 Test Plan

## Local Verification

1. Build the shared dependencies and local packages:
   - `npm run build`
2. Run the focused Phase 1 regression suite:
   - `cd packages/agent-core && npm test`
3. Run the orchestrator suite:
   - `cd packages/orchestrator && npm test`
4. Run the app runtime test:
   - `npx vitest --run packages/app/test/runtime.test.ts`

## Live OpenAI-Backed E2E

1. Use the persisted `openai-codex` OAuth session and run:
   - `node scripts/phase23-live-e2e.mjs`
2. The harness should verify:
   - root creates a worklog entry before spawning children
   - root emits two `spawn` calls plus one background `bash` call in the same dispatch burst
   - root launches one background `bash` with `__background: true`
   - the root dispatch prompt returns while the root is `idle` and at least one child is still `running`
   - root transcript gets the `[PENDING]` bash result first and the background completion later
   - background completion advertises the combined stdout/stderr output path
   - both child `agent_report` and `agent_worklog` messages reach the root
   - both child `agent_idle` notifications reach the root
   - child session context includes the inherited root worklog prefix
   - child worklog files are populated on disk
   - child `idle` status persists to `tree.json`
   - root can summarize the incoming child/background updates in a later turn
3. Inspect the JSON report:
   - `/tmp/pi-relay-phase23-e2e/report.json`
