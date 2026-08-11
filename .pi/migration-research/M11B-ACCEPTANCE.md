# M11b acceptance inventory — ✅ ALL VERIFIED 2026-08-10 (orchestrator firsthand: vitest 719, m11b-dom 49/49 both rigs, run-all.sh full matrix PASS, tsc+builds clean, zero session_not_found in traces) — EVERY owner requirement (gate before declaring dogfood-ready)
Verified by orchestrator firsthand on m11b's return. Source: owner steering 2026-08-10 + M11B-FRONTEND-DESIGN.md.

## Design fidelity
- [x] A1 Legacy pi-relay chat design wholesale: layout/sidebar/composer/panels/inspector/files/history/export — components ported w/ data adapters, NOT redesigned
- [x] A2 Delegations → Subagents: same board idiom, RECURSIVE (children of children)
- [x] A3 Icon-first minimal chrome: statuses are lucide icons + tooltip/aria, NEVER text labels; legacy patterns copied (sidebarToolbar/panels/sessionRow/statusPanels); legacy styles.css/domain.css/theme tokens only
- [x] A4 Dual-profile build intact: legacy profile vs rust backend still compiles+tests green; ?backend=bridge flag

## Transcript rendering
- [x] B1 Assistant text = plain chat flow; NO boxed cards
- [x] B2 Unknown/custom entries render NOTHING (no "Custom"/"..." placeholders)
- [x] B3 ipython I/O hidden by default; per-turn "Used X tools" expandable; per call: input block (few lines, scrollable, cell/kernel-id badge in front) + output block (few lines, scrollable, ansi, success/error status icon); NO per-tool "ipython" dropdown
- [x] B4 Comms blocks: marked agent-to-agent, expandable, gruvbox orange --primary, sender→target + preview collapsed, SAME width/indent as tool blocks (not full-row)
- [x] B5 "See more"/"See less" per turn ONLY when >3 agent messages between user messages; collapsed = natural inline flow
- [x] B6 Compactions collapsed by default (legacy collapsedCompactions)

## Subagents/comms data
- [x] C1 Drill-down via subagent.transcript ONLY — never session.* with child ids; ZERO session_not_found viewing sessions w/ subagents (trace-asserted)
- [x] C2 Comms list/annotations both directions, terminal statuses, replay on attach

## Models
- [x] D1 Picker icon-first legacy idiom (sessionDefaults/newSessionSetup), provider groups, thinking-level selector
- [x] D2 UNAUTHED MODELS ABSENT (adapter filters available===true); dogfood shows codex + nvidia-inference only until anthropic re-login
- [x] D3 Per-session setModel + thinkingLevel wired; session.create{model} honored

## Slash / sessions / workspaces
- [x] E1 /fork = duplicate at current state + REAL btrfs snapshot (rpc clone + workspace-lib); /switch = user-message boundaries (get_fork_messages); discovery via get_commands
- [x] E2 Projects/workspaces UI ported (projectList, workspaceScope, filePane/filesTab over workspace.*, gitComparison, materialization progress)
- [x] E3 MCP picker/OAuth dialogs over bridge mcp.* (kernel-mediated semantics)
- [x] E4 REPL pane kept (bash-block idiom, panels-rail icon toggle, per-session)

## Gates
- [x] G1 vitest full suite + both-profile builds + tsc green; b1–b8 + m9 + m11a traces re-green
- [x] G2 Real-model flows on BOTH rigs (dogfood gpt-5.6-sol chat+fanout+comms-block+model-switch; test GLM /fork btrfs + /switch)
- [x] G3 Dogfood rig restarted on the new build + smoke turn; THEN ping owner
