# M10a — one-shot migrator: pi-relay PG dump → new-stack artifacts

Date: 2026-08-10. Status: **COMPLETE — V1, V2, V3, V4 all PASS.**
Package: `packages/migrator` (TS CLI, `node src/cli.ts <migrate|verify>`; Node 24 strip-types, no build step).

Source: ONLY the captured dump `.pi/backups/20260809-112741/pi_relay.dump` (2.9 GB, SHA256-verified)
restored into scratch DB `pi_relay_migtest` on the TEST container @127.0.0.1:56432. The migrator is
read-only against PG and refuses any non-56432 port unless `MIGRATE_ALLOW_NONTEST=1`. Live pi-relay
(@55432), `~/.config/pi-relay`, `~/.local/state/pi-relay`, and `pi-relay/rust/` were never written.

## Output layout ($MIGRATE_OUT, default packages/migrator/out)

| artifact | contents |
|---|---|
| `sessions/<ISO-ts>_<uuidv5>.jsonl` | 5159 pi v3 session files (tree format: 8-hex entry ids, parentId chains) |
| `idmap/<newid>.json` + `idmap/manifest.json` | per-session old↔new id map (incl. every entry id), global manifest w/ per-file sha256 + per-session action counts |
| `control-plane.sql` | bridge PG rows: 3 projects + 5159 sessions, `INSERT … ON CONFLICT DO NOTHING` (idempotent), `state='host_down'`, `mcp_selection=NULL`, `workspaces='[]'` |
| `workspace-plan.json` | per-session workspace materialization plan (PLAN ONLY — no btrfs, no live-state reads) |
| `mcp.toml` | 4 oauth streamable_http server defs, normalized; parses with bridge `parseMcpConfig` |
| `mcp-manifests/<fp>.json` + `by-session.json` | 12 captured MCP session manifests + per-session fingerprint map (record-only; OAuth creds NOT migrated) |
| `agent/` | M7 harness layout: `roles/` (9 role dirs, byte-copies), `skills/` (4 workflow-* + swe), `projects/dynamo/` (project skills staging), `AGENTS.md`, `harness/harness_state.json` (schema:1 + provenance prompt entry) |
| `audit/` | full exports: delegations.jsonl (3114), events.jsonl (21539), queued_inputs.jsonl (8959) |
| `cwd/<newid>/` | empty per-session cwd dirs so migrated sessions are bootable |
| `migration-report.json` | run log (only non-deterministic artifact; excluded from V3 tree hash) |

## Mapping table (source → new stack)

| source (pi_relay_migtest) | mapping | verification |
|---|---|---|
| sessions 5159 (356 root, 4654 delegation children, 149 forks) | v3 header (uuidv5 id, original created_at, cwd) + converted entries + `pi_relay_session_info` + `pi_relay_migration` marker (pins leaf); forks get `header.parentSession` | V1 per-session chain hash 5159/5159 |
| transcript_entries 933130 | user/assistant/tool_result/compaction_summary → v3 `message`/`compaction`; daemon_tool_observation (3787) → `custom_message` (user-role context, matches pi-relay openai.rs rendering); **skipped (not model-visible): turn_started 12995, turn_finished 13059, tool_call_started 343980**; provider_replay 204060 dropped (counted); args_json string→object (0 parse failures); usage/cost zeroed | counts: 563096 emitted + 370034 skipped = 933130 ✓; chain hashes ✓ |
| compaction_summary 1949 (28 cross-session) | v3 `compaction` entries, parentId = mapped source_leaf (same-session) / null (cross); `firstKeptEntryId=null` ⇒ nothing pre-compaction retained — matches pi-relay provider feed (summary+suffix; provider-native compaction) | V1 hashes over compaction-hop walks; V2 boot session had 1 compaction (GLM turn used ~6k-token compacted context) |
| delegations 3114 | parent file gains `rlm_child_lifecycle` custom entries (admitted + terminal phase from status: done→completed, done_with_failures→completed, failed/cancelled→failed, running→admitted-only); rlm_child_id = child new uuid prefix; child marker carries delegation_id + parent new id; 14 launch-failure delegations recorded with session_id=null | V4 via real `supervisor.subagentTree` |
| actions 544265 | per-session counts by kind in manifest (bodies stay in the 2.9 GB dump archive) | sum == 544265 ✓ |
| queued_inputs 8959 (0 pending), events 21539 | full audit JSONL exports (all consumed/cancelled → record-only) | line counts ✓ |
| mcp_session_manifests 12 | manifest copies + per-session fingerprint map; session rows get mcp_selection=NULL (stale selections are unsafe: `buildSessionCatalog` throws on unknown servers; OAuth tokens live in the off-limits state dir) | V2 parse; M11 fresh logins |
| projects 3 / runtimes 1 | projects → bridge rows (uuidv5, camelCase WorkspaceDecl); runtimes → recorded in manifest only (no bridge equivalent) | scratch-DB load ✓ |
| daemon_config 0 | empty in dump; nothing to emit | — |
| roles 9 / skills 5 / prompt scopes / mcp.toml | byte-copies into M7 layout; note: `monitor` role dir has `name: tester` in SOURCE — both pi-relay and prime-harness reject it ("directory must match name"); parity = 8 usable + 1 invalid, preserved as-is (zero-data-loss, no silent fixes) | V2 `discoverRoles`/`loadHarnessSkills` |

## Id strategy (deterministic ⇒ idempotent)
- session/project ids: **uuidv5** (fixed migrator namespace, name = `pi-relay-session:<old>`), so reruns are byte-stable and M11 can re-derive any id offline.
- entry ids: first 8 hex of sha256(`<session>:<old entry id>`), deterministic widening on collision (none observed).
- All timestamps preserved from source (created_at/updated_at/timestamp_ms). File bytes depend only on source rows.

## Verification results
- **V1 (counts + checksums): PASS.** Table counts reconcile exactly (above). Per-session active-branch chain hash (source CTE-equivalent walk incl. same-session compaction source_leaf hops vs emitted file walk, canonical model-visible content incl. ms timestamps): **5159/5159 match**. Per-file sha256 in manifest; 25-file spot re-hash OK.
- **V2 (consumer load): PASS.** bridge `rebuildTranscript` on a 1017-block migrated file (644 messages, 373 tools) ✓; bridge `parseMcpConfig` → 4 servers ✓; prime-harness `loadHarnessState` (schema 1) ✓; `discoverRoles` 8+1 parity ✓; `loadHarnessSkills` 5 ✓; full `control-plane.sql` loads into a scratch bridge-schema DB (3 projects, 5159 sessions, FKs OK) ✓; **boot test: migrated session `73aa1b79-…` (old `session_7ba72230-…`, 114 entries, 1 compaction) inserted as host_down, test bridge restart respawned it, `prompt.send` produced a real GLM turn replying exactly `M10A-BOOT-OK` (nvidia-inference/glm-5.2, usage 6101/8), appended as proper v3 entries chained off the migration marker.** Historical model `openai/gpt-5.5` is unconfigured in the test env ⇒ pi's documented resume fallback to the settings default kicked in (sdk.js `modelFallbackMessage` path).
- **V3 (idempotency): PASS.** Second full run: **0 files changed**; tree snapshot compare over 10356 files: added=0 removed=0 changed=0 (report log excluded by design).
- **V4 (linkage): PASS.** Parent `session_247c936b-…` (3 delegations, 6 children) via real bridge RPC `subagent.tree`: 6 children with correct names/statuses/phases (4 completed, 2 cancelled), childSessionIds == uuidv5(child old ids) — set equality verified; all 6 child files exist and their markers back-reference parent new id + delegation_id. Cross-check vs PG ground truth exact.

## M11 runbook (live cutover)
1. Freeze writes on live pi-relay; capture final dump; verify SHA256; restore into scratch; point `MIGRATE_*` at the new restore (still port 56432) and rerun `node src/cli.ts migrate` (deterministic — diffs only where the dump changed).
2. Sanity: `node src/cli.ts verify` (chain hashes) + `--tree-check` against the M10a baseline if desired.
3. Apply `control-plane.sql` to the new-stack bridge PG (`ON CONFLICT DO NOTHING` makes re-application safe).
4. Materialize workspaces per `workspace-plan.json` (bridge `workspaces.ts` materializeForSession; transfer content from `~/.local/state/pi-relay/sessions/<old>/cwd` — the migrator never touched it).
5. Copy `agent/` into place (PI_CODING_AGENT_DIR), merge `mcp.toml` server defs, run fresh OAuth logins per owner checklist (credentials intentionally not migrated).
6. Start the bridge; sessions are `host_down` so nothing mass-respawns; resume sessions on demand.

## Risks / open items
- **reconcileOnBoot respawns ALL non-closed rows.** Migrated rows are emitted `host_down` — any bridge restart respawns every such session (5159 × pi host = non-viable if bulk-imported then restarted). M11 mitigation: either mark imported rows `closed` until first use, or add a "resume on demand" path / a bridge-side guard before bulk import. Single-session resume proven by the boot test.
- **Historical models unconfigured in the new env** → resume falls back to settings default (proven graceful). If exact-model fidelity matters, configure `openai`/`claude` providers in the new agent dir.
- **The 1 'running' delegation** at snapshot time is stale state; migrated as admitted-only and flagged in the report; owner should reconcile at M11.
- **Workspaces are plan-only** (no btrfs ops, no live-state reads by design); content transfer is an M11 step.
- **MCP OAuth credentials** live in the off-limits state dir; fresh logins required at M11 (per owner checklist).
- 955 sessions have >1 root (branchy histories); emission preserves ALL branches (every entry emitted in sequence order), the marker pins the pi-relay active leaf; non-active branches remain browsable via pi's tree navigation.
- `queued_inputs`/`events`/`actions` are audit exports, not live state (all inputs consumed; actions derivable from entries).

## Reproduce
```
cd packages/migrator
MIGRATE_PG_PASSWORD=$(cat ../../.pi/m1-demo/.bridge-pg-password) node src/cli.ts migrate   # ~112s
MIGRATE_PG_PASSWORD=… node src/cli.ts verify                                              # V1 chain hashes
node test/v2-consumers.mjs out                                                            # V2 file-level consumers
```
