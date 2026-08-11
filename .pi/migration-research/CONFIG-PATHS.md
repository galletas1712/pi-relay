# CONFIG-PATHS.md — new-stack config/state path audit (2026-08-09)

Purpose: every path the new stack (extensions + bridge) reads or writes, its
source of truth, and the hard off-limits list. Rules: (1) discover from the
**upstream pi agentDir** (`$PI_CODING_AGENT_DIR`, default `~/.pi/agent/`);
(2) additive subdirs only — never overwrite pre-existing pi-CLI files;
(3) never touch live pi-relay paths; (4) repo `.pi/` is campaign workspace.

## Upstream pi path facts (verified in dist/config.js)
- `CONFIG_DIR_NAME = ".pi"` (pkg piConfig override; branded builds differ).
- `getAgentDir()` = `$PI_CODING_AGENT_DIR` ?? `~/.pi/agent/` — the ONLY user-level root.
- `PI_CODING_AGENT_SESSION_DIR` overrides session storage root.
- **No project-level `.pi/` discovery in upstream** — project config is OUR
  convention (mirrors pi-relay's project scope), not upstream behavior.
- `~/.pi/agent/` PRE-EXISTS on this machine = the owner's pi CLI home
  (auth.json, settings.json [claude defaults], sessions/, extensions/).
  → New-stack global state lives there additively (harness/, roles/, AGENTS.md).
  → One campaign-era leak: `models-store.json` ({}, 08-09 00:48) written by
    upstream FileModelsStore when some early-M1 invocation ran without
    PI_CODING_AGENT_DIR. Harmless (pi's own file in pi's own dir); always set
    the env when invoking pi code.

## New-stack paths
| Surface | Path | Owner |
|---|---|---|
| pi user config (shared w/ pi CLI) | `~/.pi/agent/` (settings.json, models.json, auth.json — READ, never overwrite) | upstream |
| Global harness store | `<agentDir>/harness/harness_state.json` | prime-harness |
| Global roles | `<agentDir>/roles/<name>/SKILL.md` | prime-harness (M7) |
| Global AGENTS.md | `<agentDir>/AGENTS.md` | prime-harness context-files |
| Project roles (our convention) | `<cwd>/.pi/roles/<name>/SKILL.md` | prime-harness (M7) |
| Project AGENTS.md | cwd-upward walk, bounded at git root / $HOME | prime-harness context-files |
| Workspace-scope AGENTS.md | `$PI_RELAY_WORKSPACE_DIRS` (multi-dir, M8) | prime-harness context-files |
| Per-session state | `<sessionDir>/prime/<sessionId>/` (harness local, refine-results, snapshots, outbox) | all extensions |
| Kernel venv | `$PRIME_RLM_KERNEL_VENV` (demo: repo .pi/m1-demo/venv) | prime-rlm provision |
| Bridge runtime | ALL env-driven (`packages/bridge/src/config.ts`): BRIDGE_PORT/AUTH_TOKEN/ALLOWED_ORIGINS/PG_URL/PI_CODING_AGENT_DIR/DATA_DIR/... | bridge |
| Workspace state root | `$BRIDGE_WORKSPACE_STATE_ROOT` (demo default packages/bridge/data/workspace-state; btrfs probed) | bridge M8 |
| MCP config | `$BRIDGE_MCP_CONFIG` (mcp.toml; pi-relay runtime/mcp.toml analog) | bridge M8 |
| MCP OAuth creds | `<workspaceStateRoot>/mcp-oauth-credentials.json` (0600) | bridge M8 |
| Session titles | sidecar via GLM shim (BRIDGE_TITLE_*) | bridge M8 |

## OFF-LIMITS (live pi-relay — never read for writes, never delete)
- `~/.config/pi-relay/` (agentd + runtime config)
- `~/.local/state/pi-relay/` (live btrfs workspace base — bridge comment enshrines this)
- Containers `pi-relay-postgres` (55432), `infra-control-1`; live DB `pi_relay`
- `pi-relay/rust/` (reference-only until Phase 4)

## Prod deployment (cutover) env set — to be finalized in M8 report
PI_CODING_AGENT_DIR (prod agentDir w/ extensions+models), BRIDGE_* (port, token,
origins, PG, data dir, workspace root [btrfs], mcp.toml), PRIME_RLM_KERNEL_VENV.
