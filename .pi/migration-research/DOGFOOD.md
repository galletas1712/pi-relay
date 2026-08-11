# Dogfood rig — new stack, real models, zero live overlap

**Open: http://127.0.0.1:8789/?backend=bridge**

| Piece | Dogfood (this rig) | Live deployment (untouched) |
|---|---|---|
| web | vite dev :8789 (`?backend=bridge`) | prebuilt assets |
| bridge | :8731, `.pi/dogfood/bridge-data` | rust backend |
| PG | `pi_relay_dogfood` in TEST container 127.0.0.1:56432 | live 55432 |
| models | Codex OAuth `gpt-5.6-sol` (default), `claude-opus-5` after re-login | — |
| agentDir | `.pi/dogfood/agent` (auth.json = SYMLINK to ~/.pi/agent/auth.json) | ~/.config/pi-relay |
| workspaces | `.pi/dogfood/workspace-state` (btrfs, home fs) | ~/.local/state/pi-relay (2.0T, never touched) |

## Proven (2026-08-10)
- Real Codex OAuth turn: `gpt-5.6-sol` ran an ipython kernel call (6*7) and replied `DOGFOOD-OK 42`; repl.cell events + prompt-build trace recorded.
- Everything M1–M9 (roles, skills, harness CRUD, /refine, comms w/ rpc-park fix, repl console, workspace/project routes, MCP mock) applies here too — same extension set loaded from `.pi/dogfood/agent/settings.json`.

## Operate
- Restart bridge: `kill $(cat .pi/dogfood/bridge-data/bridge.pid); nohup .pi/dogfood/run-dogfood.sh >> .pi/dogfood/bridge-data/bridge.log 2>&1 &`
- Restart web: `BRIDGE_PORT=8731 BRIDGE_TOKEN_FILE=.pi/dogfood/bridge-token npx vite --port 8789` (from packages/web)
- Switch model: edit `.pi/dogfood/agent/settings.json` defaultModel → restart bridge (session.create has NO per-session model param yet — M11 gap list).
- Claude cache A/B: `BRIDGE_PI_CACHE_RETENTION=long` in run-dogfood.sh env (1h anthropic breakpoints).

## Owner action items
1. **Anthropic re-login** (`pi /login anthropic`): current OAuth grant is dead (`invalid_grant` on refresh). Then set defaultModel `claude-opus-5` — it's in the pinned catalog.
2. Codex geo note: `gpt-5.5` is geo-blocked for this project; the gpt-5.6 series works (sol verified; terra untested; luna returned empty).
3. Optional: real MCP OAuth logins (Slack/Linear/Outlook/NVCarPs) against this rig — checklist in M8-WORKSPACE-MCP.md §owner.
4. Old pi-relay sessions: arrive when m10a migrator lands (running now); imported into THIS rig's PG for rehearsal before any live cutover.

## Notes
- NVIDIA SSE endpoint: owner says temporarily broken — not a blocker; GLM shim (:8571) remains the test path, direct streaming resumes when upstream heals.
- The two rigs coexist: test rig (bridge :8730 / vite :8788, GLM) + dogfood (:8731 / :8789, real OAuth).
