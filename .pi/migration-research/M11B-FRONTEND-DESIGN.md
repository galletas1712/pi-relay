# M11b — frontend design directive (owner, 2026-08-10): KEEP pi-relay's design

**Directive: the new UI is pi-relay's existing SPA, re-pointed at the bridge contract. No redesign.**
The M6 BridgeApp shell (SessionList/TranscriptView/Composer/SubagentTreeView/ReplPane) was scaffolding
to prove contract v0 — the production target is the legacy app (src/App.tsx, chatPane, transcript,
composer, delegationBoard, inspector, filePane, panels, slash.ts, mcp dialogs…) adapted to the bridge.

## The sanctioned delta (and ONLY this)
1. **Delegations → Subagents**: same board/panel design; subagents are RECURSIVE (children of children).
   Data: subagent.tree (live lifecycle + durable) + subagent.transcript drill-down (m11a) + comms annotations.
2. **ipython tool I/O = reuse the bash tool block design** (command box + ansi-aware output, collapsed/expanded,
   error styling). Model cells stream into the transcript like tool calls; the separate REPL pane (user console,
   owner-requested M9) keeps the same block design for its cells.
3. **Model selector**: pi-relay ModelPicker UX, data from contract models.list (upstream get_available_models:
   agentDir models.json custom providers + built-in catalog + auth state). Per-session switch = session.setModel.
4. **Slash commands**: keep pi-relay semantics (src/slash.ts: fork = duplicate at current state; switch = switch
   branches / edit historical message). Backed by upstream rpc — VERIFIED 1:1:
   - get_fork_messages → getUserMessagesForForking() = **user-message boundaries only** (owner's rewind rule) ✓
   - fork {entryId} branches the session tree; clone = fork at current leaf; switch_session; get_tree/get_entries
   - pi-relay fork ALSO btrfs-snapshots the workspace → bridge composes: workspace-lib snapshot + rpc fork/clone.
   - get_commands exists for slash discovery.
5. Everything else (chat layout, composer, inspector, files, mcp picker/OAuth dialogs, history, export, tasks)
   ports as-is with contract adapters. MCP stays kernel-mediated (M8 design); roles/skills/prompt-scope surfaces
   map to harness CRUD.

## Contract additions beyond m11a v0.2 (for M11b)
session.fork {sessionId, entryId?} (no entryId = clone-at-leaf) → {newSessionId, workspaceSnapshot}; 
session.getForkPoints {sessionId} → user-message entries; session.switch {sessionId, entryId};
slash command discovery via get_commands. All journaled/idempotent per bridge conventions.

## Sequencing
m11a (contract v0.2 model surface + minimal picker/drill-down on interim shell) lands FIRST (running).
M11b = the legacy-design port (one child, big brief); m11a's picker/drill-down logic gets re-homed into the
legacy components. Then M11 soak/cutover gates (reconcileOnBoot guard, workspaces plan, verify-growth, etc.).

## Visual language addendum (owner, 2026-08-10)
SubagentTreeView / ReplPane / DiffPanel STAY as panels, but must follow the legacy design language:
minimal, icon-first chrome (lucide-react icons + tooltips/aria-labels, NOT text-labeled buttons),
legacy styles.css/domain.css tokens, compact rows. The M6 scaffolding versions are functionally correct
but visually too texty — re-skin during the port. Model picker = compact icon-triggered dropdown in the
legacy ModelPicker/sessionDefaults idiom, not a labeled bar.

## Transcript rendering rules (owner, 2026-08-10) — legacy chat design is law
- NO "Custom" cards / generic "..." placeholders. Unknown/custom entries render as NOTHING by default,
  or as their proper typed surface below.
- Assistant messages = plain chat flow (legacy transcript.tsx), NOT boxed "cards".
- ipython input/output: HIDDEN by default — surfaced only via the legacy Expandable pattern
  (turn-card-expand idiom, transcript.tsx:923). What matters in the chat flow = assistant TEXT
  (intermediate assistant messages included). Tool detail is opt-in.
- Agent-communication entries (prime-comms agent_message deliveries): rendered as PROPERLY MARKED
  comms blocks — expandable, distinct accent color: gruvbox ORANGE (existing --primary token #fe8019 dark / #af3a03 light — NOT purple), sender→target + preview when
  collapsed; full content expanded.
- Compactions stay collapsed-by-default like legacy (collapsedCompactions pattern).

## Minimalism re-affirmation (owner, 2026-08-10)
Original frontend is THE reference: minimal text in chrome; statuses are ICONS (lucide + tooltip/aria),
not words. If a status/state is shown as a text label, it's wrong. Copy legacy component patterns verbatim
(sidebarToolbar, panels, sessionRow, statusPanels) rather than inventing new widgets.

## Picker filtering rule (owner, 2026-08-10)
Models without auth are NOT grayed-out entries — they are ABSENT from the picker entirely.
Filter models.list by available===true at the adapter layer. (m11a's 'unauthed disabled' is superseded.)

## 'See more' turn affordance (owner, 2026-08-10)
Legacy 'Show details'/'Hide details' per-turn toggle is renamed 'See more' / (expanded: 'See less').
Render it ONLY for turns (span between consecutive user messages) containing >3 agent messages;
shorter turns show their full flow inline: <agent msg 1> <Used-N-tools expandable> <agent msg 2> … 
in order. Expanded state = full turn detail (legacy buildTurnViews), collapsed = the inline flow.
