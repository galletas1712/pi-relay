# Web Client UI Ergonomics and Look-and-Feel Implementation Plan

**Status:** Proposed

**Scope:** `packages/web` user experience, supporting daemon contracts, accessibility, responsive behavior, and rollout

**Audience:** Web, daemon/store, design, accessibility, QA, and product engineers

**Intent:** Implementation-ready roadmap; this document does not implement UI changes

## 1. Executive direction

Improve the existing pi-relay web client without replacing its identity or its strongest workflow. The transcript and live-work experience are the product's best foundation: keep its turn-based progressive disclosure, per-session draft and scroll safety, stale-response defenses, live tool visibility, warm Gruvbox palette, Geist/Space Grotesk typography, light/dark support, and reduced-motion behavior.

The primary structural change is to stop treating one global "selected session" as all of the following at once:

1. the stable root run whose execution is being inspected;
2. the conversation whose transcript is visible;
3. the execution entity in focus;
4. the recipient of the next message; and
5. the target of an in-flight mutation.

The current right panel combines status, navigation, inspection, and destructive mutations. Replace its user-facing portion with an attention-oriented **Run Navigator**, move technical inspection to a separate **Debug Inspector**, and make **Conversation** and **Execution** route-backed workspace destinations. Execution contains **Overview**, **Activity**, and **Handoffs**; Overview contains an authoritative **Outline** and, later, an optional **Map**.

This is an evolution, not a visual rewrite. It should reduce ambiguity, improve responsive use and accessibility, and preserve existing correctness properties.

## 2. Evidence and proposal notation

This plan distinguishes two kinds of statement:

- **Confirmed current source fact** means the behavior is present in the repository at the cited path and line range.
- **Proposed target** means work described by this plan; it must not be read as a current capability.

Line citations describe the repository state used to write this plan and may move as implementation proceeds.

## 3. Goals, scope, and non-goals

### 3.1 Goals

1. Make it clear which root run is pinned, which conversation is open, which execution entity is focused, and where a message or mutation will go.
2. Establish route-backed navigation with reliable deep links, refresh, and browser Back/Forward behavior.
3. Prioritize the conversation and navigation surfaces over technical inspection at constrained widths.
4. Turn execution status into a scannable, attention-oriented workflow rather than a card-heavy debug board.
5. Make common actions discoverable without removing slash-command efficiency.
6. Fix known accessibility, focus, semantic-control, contrast, mobile history, and async-state defects.
7. Improve transcript reading rhythm and composer ergonomics while preserving progressive disclosure and state safety.
8. Define honest frontend/backend boundaries for execution history, handoffs, and future graph work.
9. Deliver incrementally behind migration controls with explicit exit gates, validation, and observability.

### 3.2 In scope

- Information architecture, routes, selection state, shell geometry, and panel behavior.
- Session/project navigation, new-session setup, header actions, queue, settings, and scoped run controls.
- Run Navigator, Execution Overview/Outline, Live Activity, Handoffs, Debug Inspector, and a deferred optional Map.
- Shared accessible interaction primitives and owned async states.
- Transcript/composer presentation and interaction polish.
- Visual-token and contrast corrections that preserve Gruvbox.
- Daemon/store contract additions required for complete execution history and durable activity.
- Automated and manual validation, instrumentation, staged rollout, and migration.

### 3.3 Explicit non-goals and deferred work

- **No wholesale rebrand or design-language replacement.** Keep the warm Gruvbox personality and current type families.
- **No claim that current execution data is a full DAG.** Today the visible relation is root parent session -> delegation -> direct subagents.
- **No graph library before the topology and user need warrant it.** Outline comes first; a Map is optional.
- **No frontend-fabricated durable timeline.** The reconnect event buffer is not durable history.
- **No immediate transcript virtualization without measured evidence.** Turns and details are already paged/lazy.
- **No lowering of the full three-pane breakpoint to 1152px with current pane widths.** That would compress the center below a usable reading/work area.
- **No two authoritative Run Boards.** A temporary migration comparison may exist behind a flag, but only one view may be interactive/authoritative and the old board must be removed at the migration exit gate.
- **No generalized “Artifacts” product surface from handoff-only data.** Use **Handoffs** until the backend models broader artifact metadata.
- **No nested/dependency graph until the backend supports actual edges and nested delegation.** Subagents currently do not receive delegation tools.
- Diff-rendering sophistication is lower priority than correctness, navigation, accessibility, and transcript/composer fundamentals.

## 4. Current baseline

### 4.1 Strengths to preserve as invariants

| Strength | Confirmed current source fact | Preservation requirement |
| --- | --- | --- |
| Turn-oriented progressive disclosure | Completed turns summarize user/final assistant content, details load on demand, and the current turn stays expanded (`packages/web/src/transcript.tsx:831-914`). Tool groups have controlled collapsed/recent/all modes that preserve user overrides during live churn (`packages/web/src/transcript.tsx:1371-1452`). | Do not flatten the transcript into an undifferentiated event log. Preserve explicit user control over detail density. |
| Per-session draft safety | Drafts are keyed per selected session, versioned, persisted best-effort, and restored after rejected submissions (`packages/web/src/composer.tsx:103-200`, `packages/web/src/composer.tsx:247-275`). | Route/state changes must keep draft keys tied to the immutable recipient, not to whatever is focused after a click. |
| Immutable send routing | Composer routing uses the captured session ID, requires a matching snapshot, and routes root follow-up versus parent-scoped subagent steering without rereading current selection (`packages/web/src/composerRouting.ts:30-86`). | Preserve this capture-at-intent pattern for every message and mutation. |
| Stale-response protection | Selected-session refreshes verify the current selected ID before committing, coalesce requests per session, and ignore late selection changes (`packages/web/src/App.tsx:878-938`). Cache reducers reject session-mismatched entry, branch, tree, queue, and turn payloads and reject stale revision/page combinations where applicable (`packages/web/src/selectedSessionCache.ts:112-218`, `packages/web/src/selectedSessionCache/turns.ts:6-21`). | Generalize the guard from a single selection to root/conversation/focus stores; never weaken it. |
| Warm per-session cache | The selected-session store retains a map of session caches and repoints without evicting other sessions (`packages/web/src/selectedSessionStore.ts:20-69`). | Conversation switching within a root run should remain fast and must not force a full reload. |
| Scroll persistence | Sticky-bottom and explicit scroll positions are saved per session, restored after content is ready, and maintained through resize (`packages/web/src/transcript.tsx:233-369`). | Keep per-conversation scroll keys, including across Conversation/Execution route changes. |
| Bounded/lazy transcript loading | The app loads a 50-turn page and lazily fetches older turns/detail (`packages/web/src/App.tsx:113-114`, `packages/web/src/transcript.tsx:483-507`). | Measure before virtualizing; do not discard paging and lazy detail. |
| Live-work feedback | Current-turn detail and a server-anchored elapsed “Working…” indicator expose progress (`packages/web/src/transcript.tsx:420-444`, `packages/web/src/transcript.tsx:535-537`). | Preserve live visibility while reducing ambient motion elsewhere. |
| Visual identity and theme foundations | Light/dark Gruvbox tokens, Geist Sans/Mono, and Space Grotesk are declared centrally (`packages/web/src/styles.css:1-32`, `packages/web/src/styles.css:112-154`). | Correct contrast and hierarchy in place rather than replacing the palette or typography. |
| Reduced motion | A global reduced-motion query collapses animation and transition durations (`packages/web/src/styles.css:3967-3975`). | New motion must use the same opt-out and avoid conveying status only through animation. |
| Existing safe areas and mobile input sizing | The mobile top bar, left drawer, right Inspector, notices, and composer apply the relevant safe-area inset (`packages/web/src/styles.css:243-255`, `packages/web/src/styles.css:328-332`, `packages/web/src/styles.css:2980-2984`, `packages/web/src/styles.css:3599-3604`, `packages/web/src/styles.css:3755-3757`). Mobile inputs use the 17px input token to avoid iOS zoom (`packages/web/src/styles.css:34-68`, `packages/web/src/styles.css:3607-3621`). | Extend, do not regress, these protections. |

### 4.2 Primary structural problem

**Confirmed current source fact:** `App` has one `selectedId`/`selectedRef`, and the loaded snapshot, transcript, tool list, and delegation query all derive from it (`packages/web/src/App.tsx:313-349`, `packages/web/src/App.tsx:512-595`). The delegation query uses `loadedSnapshot.session_id` as the parent and returns only that session's direct list (`packages/web/src/App.tsx:588-612`). Clicking a subagent in the right panel calls the same `selectSession`, which resets/repoints selected state and therefore re-roots the delegation query (`packages/web/src/App.tsx:638-655`, `packages/web/src/App.tsx:2417-2436`).

Consequences:

- A user cannot keep the parent execution pinned while reading a subagent conversation.
- “Selection” has different meanings depending on which surface initiated it.
- The right panel mixes run navigation, status, controls, and technical session diagnostics.
- A selection change can make the visible run context appear to disappear.
- Back/Forward and deep-link semantics are absent because workspace state is local/persisted selection rather than route state; the entry point renders `App` directly with no router (`packages/web/src/main.tsx:1-24`).

### 4.3 Data/topology limits that the UI must state honestly

**Confirmed current source facts:**

- `delegation.list` is explicitly a bounded, newest-first page for one parent session and is documented as a lightweight run-board feed (`rust/docs/websocket-rpc.md:1544-1548`).
- The daemon defaults to 3 rows, caps the request at 100, fetches one extra row only to compute `has_more`, and exposes no cursor in the current request (`rust/crates/agent-daemon/src/delegation_tools.rs:1472-1491`, `rust/crates/agent-daemon/src/delegation_tools.rs:1517-1544`).
- The frontend model is `Delegation[]`, each with direct `subagents[]`; the list response has `has_more` but no cursor (`packages/web/src/types.ts:124-174`).
- Subagent prompt profiles exclude all delegation tools, and the child contract explicitly says nested delegation is unavailable (`rust/crates/agent-daemon/src/provider_runtime/prompt.rs:551-590`, `rust/crates/agent-daemon/src/subagents.rs:459-469`).
- Current event frames have an ID, name, session ID, and data but no event timestamp in the web type (`packages/web/src/types.ts:117-122`).
- `events.subscribe` is a reconnect stream, not historical notification storage; initial subscribe does not replay history (`rust/docs/websocket-rpc.md:395-401`). The event buffer is empty once a session is idle (`rust/docs/websocket-rpc.md:1865-1868`).
- The run board has no dedicated delegation event and relies on lifecycle invalidation plus a two-second poll while work is running (`packages/web/src/App.tsx:1178-1188`, `packages/web/src/App.tsx:596-602`).
- Existing handoff reads are limited to task prompt, final message, transcript, and cancelled transcript names (`packages/web/src/types.ts:176-184`, `rust/docs/websocket-rpc.md:1586-1606`).

Therefore:

- Call the first topology view an **Outline**, not a DAG.
- Label an event-buffer-based first activity surface **Live Activity**, not “Timeline” or “History.”
- Call the file surface **Handoffs**, not “Artifacts.”
- Treat complete history, durable Activity, nested/dependency edges, and generalized artifacts as backend-dependent.

### 4.4 Additional confirmed issues

| Issue | Confirmed current source fact | Target correction |
| --- | --- | --- |
| Right panel overload | The Inspector tabs are “Run board” and “Inspector”; Run Board contains navigation, status, cancel, and rerun while Inspector contains raw session/pending/tool/slash diagnostics (`packages/web/src/panels.tsx:232-269`, `packages/web/src/panels.tsx:804-950`). | Run Navigator becomes a user-facing navigation surface; Debug Inspector becomes a separate drawer/command. |
| Nested interactive rows | Project and session rows are `<button>` elements containing focusable `role="button"` spans (`packages/web/src/panels.tsx:394-439`, `packages/web/src/panels.tsx:585-680`). | Use one semantic row link/button plus one sibling overflow Menu trigger. |
| Dialog semantics incomplete | Custom scrims add `role="dialog"`/`aria-modal`, but shared focus trap, background inerting, Escape policy, and focus restoration are not centralized (`packages/web/src/App.tsx:2624-2657`, `packages/web/src/App.tsx:2688-2725`). | Adopt a shared Dialog/AlertDialog primitive with a documented modality contract. |
| Mobile history dead end | Alternate branches are collapsed by default (`packages/web/src/historyPickerCompact.tsx:69-86`), but mobile CSS hides `.branch-toggle` while descendants remain filtered (`packages/web/src/styles.css:3854-3865`). | Keep a visible expand control or do not collapse alternate descendants at that breakpoint. |
| Ephemeral errors | Notices are automatically removed after four seconds (`packages/web/src/App.tsx:104-105`, `packages/web/src/App.tsx:386-392`). | Keep transient confirmations, but give actionable failures persistent owned error/retry UI. |
| Ambiguous async empty states | Session list renders one row for loading, refreshing, or no sessions, and delegation errors are inline text (`packages/web/src/panels.tsx:346-363`, `packages/web/src/panels.tsx:245-260`). | Use Loading/Empty/Error/Retry state panels owned by each surface. |
| Light muted contrast | Light tokens use `#7c6f64` as `--muted-foreground` on several warm backgrounds (`packages/web/src/styles.css:10-29`). Audit measurements are approximately 3.55:1 on `--muted`, 4.42:1 on `--card`, and 4.29:1 on `--background`. | Raise normal-text contrast to at least 4.5:1 in every intended token pairing; prototype the exact color. |
| Microtext is overused | The type scale explicitly assigns 11px to statuses, timestamps, pills, badges, counts, and IDs (`packages/web/src/styles.css:34-68`). | Do not use 11px for actionable state or required comprehension; reserve it for nonessential annotations. |
| Responsive priority is reversed at medium widths | At 900-1279 the sidebar remains an overlay while a 320-380px Inspector column persists; the full three-column layout starts at 1280 with 320px + center + 320-380px (`packages/web/src/styles.css:2992-3039`). | Keep both side surfaces in drawers below 1000px; from 1000px persist session navigation and overlay the Run Navigator; allow full three-column geometry at 1440px only when the measured center remains at least approximately 720px. |
| Breakpoint changes reset preference | On media-query changes the app reapplies default open states (`packages/web/src/App.tsx:2189-2205`). | Preserve user panel intent across breakpoints unless geometry makes the state impossible. |
| Queue dominates composer when nonempty | The full queue pane opens whenever queued inputs exist and slash completion is not open; each row exposes many inline controls (`packages/web/src/composer.tsx:313-329`, `packages/web/src/composer.tsx:371-520`). | Replace with a count/tray; use popover on desktop, sheet on mobile, and row menus. |
| Slash semantics are incomplete | A standalone `role=listbox` contains button/options but is not connected to the textarea as a combobox; keyboard completion supports arrows and Tab/Cmd-or-Ctrl+Enter but not standard Enter/Escape completion behavior (`packages/web/src/composer.tsx:277-310`, `packages/web/src/composer.tsx:581-619`). | Implement an APG combobox with active descendant, Enter, Escape, and discoverable help. |
| Shortcut copy is not platform-correct | Logic accepts Meta or Ctrl, while visible title text always says Cmd+Enter (`packages/web/src/composer.tsx:43-45`, `packages/web/src/composer.tsx:330-363`). | Render `⌘ Enter` on macOS and `Ctrl Enter` elsewhere, visibly near Send. |
| New-session setup can clear the draft | Starting the explicit new-session state clears the composer value before focusing it (`packages/web/src/App.tsx:1556-1564`). Workspace scope appears only above the composer when no session is selected (`packages/web/src/App.tsx:2394-2397`). | Preserve the new-session draft and colocate workspace, model, and reasoning choices in a setup state. |

## 5. Design principles

1. **Protect work before polishing it.** Draft ownership, immutable mutation targets, stale-response fences, and recoverable errors outrank animation or visual novelty.
2. **One concept, one authoritative surface.** The Run Navigator and Execution workspace may summarize the same root projection for different purposes, but must not become competing mutable boards.
3. **Conversation is not execution focus.** Reading or messaging an agent must not silently re-root the execution context.
4. **Progressive disclosure over permanent chrome.** Keep summaries scannable; reveal tools, diagnostics, queue details, and raw IDs on demand.
5. **Text plus icon, never color alone.** Status, outcome, selection, and attention need readable labels and semantic icons.
6. **Navigation before inspection.** Preserve the session navigator and center workspace before showing Run Navigator or Debug Inspector columns.
7. **URLs are state.** Major destinations and meaningful selections must survive refresh and browser history.
8. **Backend truth over frontend inference.** Unknown, bounded, or live-only data must be labeled as such.
9. **Accessible primitives before more one-off overlays.** Centralize keyboard, focus, modality, pending, and error behavior.
10. **Incremental visual convergence.** Adopt spacing, control-height, radius, and elevation tokens as components are touched; avoid a risky all-at-once CSS rewrite.

## 6. Target information architecture

### 6.1 Workspace hierarchy

```text
Project or Host scope
└── Root run
    ├── Conversation
    │   └── Conversation subject (root session or one direct subagent session)
    └── Execution
        ├── Overview
        │   ├── Outline (authoritative)
        │   └── Map (optional, never the only representation)
        ├── Activity
        │   ├── Live Activity (frontend/current backend)
        │   └── retained Activity timeline (only after durable event API)
        └── Handoffs
            └── task prompt / final message / transcript / cancellation transcript
```

### 6.2 Route contract

The exact path spelling needs a small prototype because Host has no project ID, but the state carried by the URL is not optional. Recommended canonical shapes:

```text
/w/project/:projectId/run/:rootSessionId/conversation/:conversationSessionId
/w/project/:projectId/run/:rootSessionId/execution/overview
/w/project/:projectId/run/:rootSessionId/execution/activity?conversation=agent:<sessionId>&focus=agent:<sessionId>
/w/project/:projectId/run/:rootSessionId/execution/handoffs?conversation=agent:<sessionId>&focus=agent:<sessionId>&handoff=:handoffRef

/w/host/run/:rootSessionId/...        # same destination suffixes for project_id=null
```

The examples show decoded query values for readability; the serializer must URL-encode them.

Route rules:

- `Conversation` and `Execution` are route links, not ephemeral tabs.
- `Overview`, `Activity`, and `Handoffs` are nested route links. APG tabs are acceptable only if they remain links with correct route semantics; do not use a local-only tab state.
- Every Execution route accepts an optional typed `conversation` query value: `root:<rootSessionId>` or `agent:<sessionId>`. It retains the effective conversation while Execution is visible.
- An absent `conversation` means `root:<rootSessionId>`. The canonical serializer omits `conversation` for that root default; it must include a valid non-root conversation when navigation enters Execution from an agent Conversation.
- Execution destination, subview, focus, and detail link builders preserve a valid non-root `conversation` unchanged unless the user selects a different root or explicitly opens/messages another conversation.
- The Conversation path ID and an Execution `conversation` agent ID must resolve to a session belonging to the route's root. Validate membership from session/root data, not merely from the currently bounded delegation page.
- A malformed, unknown, or wrong-root Execution `conversation` falls back to the root conversation, displays a persistent owned warning that the requested conversation was unavailable, and replaces the URL with the canonical root-default form. This fallback must never be silent and must not add a history entry. An invalid Conversation path instead renders an owned Unavailable state with a link to the root Conversation; it does not silently replace the visible transcript.
- The message recipient is derived exclusively from the effective conversation. There is no independent recipient query parameter, preference, or `localStorage` value.
- `Outline` is the default Overview mode, so canonical URLs omit `overview=outline`. `overview=map` is added only when Map ships.
- `focus` uses a typed entity reference such as `root:<sessionId>`, `delegation:<delegationId>`, or `agent:<sessionId>`, not an untyped ID.
- An absent `focus` means `root:<rootSessionId>` and is omitted by the canonical serializer.
- `handoff` is optional and opens a selected handoff in a drawer/detail pane while retaining the route.
- Invalid or no-longer-visible `focus`/`handoff` refs produce an owned “Unavailable” state with a route back to the root outline; they do not silently select a different entity.
- Query parameters should represent optional detail state; path segments represent major navigation.
- Default elision and invalid-query correction use `history.replaceState`; explicit destination, conversation, and durable-detail actions use `pushState`. These corrections do not consume an extra Back step.
- The old `localStorage` selection remains a one-time fallback only when the URL has no route state.

### 6.3 Target state dimensions

| State | Meaning | Persistence/source | Must not imply |
| --- | --- | --- | --- |
| `rootSessionId` | Stable top-level/root run whose execution projection is loaded | Required route parameter after a run is chosen | It is not necessarily the visible transcript or next message recipient. |
| `conversationSessionId` | Effective conversation session: visible transcript in Conversation and retained messaging context in Execution | Conversation path parameter; typed optional `conversation` query in Execution; defaults to `rootSessionId` when that query is absent | It does not re-root delegation queries. |
| `focusedExecutionEntity` | Root, delegation, or direct agent selected in Execution/Run Navigator | Typed route query or default `root:<rootSessionId>` | It does not change the transcript or composer recipient by itself. |
| `workspaceView` | `conversation` or `execution` | Route segment | It does not destroy the hidden view's draft/scroll state. |
| `executionView` | `overview`, `activity`, or `handoffs` | Nested route segment | “Activity” is not necessarily durable until the backend phase. |
| `overviewMode` | `outline`, later `map` | Optional route query; Outline default | Map never replaces Outline. |
| `selectedHandoff` | Optional typed handoff reference | Route query | Selection does not imply the file has loaded successfully. |
| `messageRecipient` (derived) | Explicit recipient shown at the composer; always the effective conversation subject | Derived only from validated `conversationSessionId`; never persisted independently | Execution focus alone never changes it. |
| Panel preferences | Nav/run/debug open state and optional sizes | User preference store, constrained by current geometry | Breakpoint changes do not overwrite the preference. |

Implementation note: create a small route adapter/state reducer rather than adding more independent `useState` calls to `App.tsx`. It should parse and validate route state, derive the effective conversation and recipient, canonicalize defaults, and issue atomic push/replace transitions. Keep server caches keyed by entity IDs outside the route reducer.

### 6.4 Click, history, and recipient semantics

| User action | Route/state transition | Conversation/recipient result | History behavior |
| --- | --- | --- | --- |
| Select a root run in session navigation | Set `rootSessionId`; open its Conversation by default | `conversationSessionId=rootSessionId`; recipient=root | Push a new history entry. |
| Follow the Execution destination from Conversation | Open the selected/default Execution view; serialize `conversation` when the current conversation is non-root | Preserve the effective conversation; derive the same recipient | Push. |
| Follow the Conversation destination from Execution | Open the Conversation path for the effective conversation | Preserve the effective conversation; derive the same recipient | Push. |
| Click a Run Navigator row | Open Execution at the relevant view and set `focusedExecutionEntity`; carry a non-root `conversation` query unchanged | Preserve current conversation and derived recipient | Push when entering/changing a major Execution destination; replace when already in that destination and only focus changes. |
| Click or rove to an Outline row | Set `focusedExecutionEntity`; show focus details in place | Preserve the `conversation` query and derived recipient | Replace for focus-only changes, including keyboard roving, so history is not flooded. |
| Open a handoff or other durable detail | Add its typed detail query while preserving `conversation` and `focus` | Preserve conversation and derived recipient | Push so Back closes/restores the prior detail state. |
| Choose “Open conversation” for an agent | Open Conversation route for that agent | Set conversation and recipient to the agent | Push. Root remains pinned. |
| Choose “Message agent” from Execution | Open Conversation for that agent and focus the composer | Conversation changes to the agent, so the derived recipient changes explicitly | Push. There is no in-place recipient-only state. |
| Switch transcript branch/history | Mutate only `conversationSessionId`'s transcript branch | Recipient unchanged | Route is stable; mutation result may replace branch-specific optional state, not root/focus. |
| Browser Back/Forward | Restore all route-backed dimensions atomically | Re-derive recipient from restored conversation; recover draft and scroll by conversation/session keys | Never emit a mutation or write an independent recipient. |
| Parse an explicit root default or invalid optional Execution `conversation` | Canonicalize to omitted `conversation`; warn for invalid values | Root conversation and recipient after validation/fallback | Replace; never push a correction entry. |
| Open Debug Inspector | Toggle separate diagnostic drawer | No identity changes | Preference, not browser history, unless deep-linked diagnostics become a requirement. |

### 6.5 Mutation target capture

Every mutation starts from an immutable target object captured at click/submit time:

```text
target kind + rootSessionId + conversation/session/delegation ID
+ relevant revision/leaf ID + client control ID
```

The handler must never reread “current selection” after awaiting. This extends the safe pattern already used by composer routing and stop-session control (`packages/web/src/composerRouting.ts:30-86`, `packages/web/src/stopSession.ts:7-20`).

Required rules:

- Rename/archive/delete capture the session ID and project scope.
- Stop captures the exact conversation session and active generation/leaf information available to the API.
- Steer captures root parent ID, child session ID, and client control ID.
- Cancel captures root parent ID and delegation ID; when multiple agents are affected, an AlertDialog names/counts them.
- Rerun captures the delegation revision/attempt identity once the backend exposes it.
- Queue edit/delete/reorder captures session ID, input ID, and queue revision.
- Settings capture session/project ID and applicable revision.
- Success or failure updates only the targeted entity's pending/error state even if the user navigates elsewhere.

## 7. Target layouts and wireframes

Wireframes show hierarchy, not final visual styling.

### 7.1 Wide three-pane (1440px and above when the measured fit passes)

```text
+----------------------+--------------------------------------------------+----------------------+
| Session navigation   | Root title        Conversation | Execution       | Run Navigator        |
| 260-288              +--------------------------------------------------+ 260-300              |
|                      |                                                  | Needs attention      |
| Projects             |              active workspace                    |  ! failed review     |
| Sessions             |              center hard min ~720                |  ! handoff ready      |
|  status + title      |                                                  |                      |
|  one overflow menu   |   Conversation transcript OR Execution view     | Active               |
|                      |                                                  |  • implementer 2/3   |
|                      |                                                  |                      |
|                      |                                                  | Recent               |
|                      |                                                  |  ✓ audit completed   |
+----------------------+--------------------------------------------------+----------------------+
```

- Debug Inspector is not the third persistent pane. It opens as a separate drawer/command over the right side.
- The Run Navigator may be persistent or resized only while the measured center remains at least approximately 720px. If it does not fit, retain the two-pane shell and open Run Navigator as an overlay/drawer even at a nominally wide viewport.

### 7.2 Medium two-pane (1000-1439px)

```text
+---------------------------+-------------------------------------------------------------+
| Session navigation 260-280| Root title      Conversation | Execution     [Run nav] [⋯] |
| persistent                +-------------------------------------------------------------+
| Projects / Sessions       | center preferred >=720; hard floor 640                     |
|                           |                    center workspace                          |
|                           |                                                             |
|                           |                                                             |
+---------------------------+-------------------------------------------------------------+
                                                Run Navigator opens as overlay/drawer --->
```

- This deliberately reverses the current priority: navigation persists, Run Navigator overlays.
- At the 1000px threshold, use a 260px navigation track; after a divider and a shell-gutter budget of no more than 20px, approximately 720px remains for the center. Grow navigation toward 280px only when the center still meets its preferred 720px target.
- Approximately 720px is the preferred medium center target. A computed 640px is the hard two-pane floor during zoom, reflow, or constrained embedding; if less than 640px remains, switch session navigation back to a drawer rather than squeeze the workspace.
- Run Navigator never occupies a medium layout track and never auto-opens; it appears only as a user-invoked overlay/drawer.

### 7.3 Compact (below 1000px, or whenever the measured two-pane fit fails)

```text
+----------------------------------------------------------------------------+
| [Sessions] Root / subject                  Conversation | Execution [Runs] |
+----------------------------------------------------------------------------+
|                                                                            |
|                         full-width workspace                               |
|                                                                            |
|  session navigation <--- modal drawer         modal drawer ---> Run nav    |
|                                                                            |
+----------------------------------------------------------------------------+
| recipient: root or agent · queue 2 · Ctrl/⌘ Enter                          |
| [ composer, safe-area aware                                      ] [Send] |
+----------------------------------------------------------------------------+
```

- Navigation and Run Navigator are drawers with focus transfer, focus trap, background inerting, Escape behavior, and restoration.
- Debug Inspector is available from the actions menu/command palette, not promoted over navigation.

### 7.4 Conversation

```text
+----------------------------------------------------------------------------+
| Root run / Conversation: reviewer                         [settings] [⋯]   |
| status text + icon · exact last activity time                               |
+----------------------------------------------------------------------------+
|                         shared content rail                                |
|                                                                            |
| User message                                                               |
|                                                                            |
| Assistant prose (72-80ch measure)                                          |
| [code block / table may use wider technical rail              ] [Copy]    |
|                                                                            |
| tool activity [3 uses] [completed]                         [Show details]  |
|                                                                            |
|                                              10:42 AM · Worked for 1m 08s  |
|                                              [Show details] [Copy response] |
|                                                                            |
|                                   [5 new events · Jump to latest]           |
+----------------------------------------------------------------------------+
| To: reviewer agent   [Queue 2] [model · reasoning]             Ctrl/⌘Enter |
| [ message                                                               ] |
+----------------------------------------------------------------------------+
```

### 7.5 Execution Overview — authoritative Outline

```text
+----------------------------------------------------------------------------+
| Execution / Overview        [Outline] [Map later]       [Activity] [Handoffs]|
+----------------------------------------------------------------------------+
| Root run  ✓ idle      2 delegations · 3 agents · 1 needs attention          |
|                                                                            |
| v Implement change                         done with failures · 2/2         |
|   ├─ ! implementer                         failed · final message available |
|   └─ ✓ reviewer                            done   · handoff available       |
|                                                                            |
| v Investigate UI                           running · 2/3                    |
|   ├─ • explorer A                          running                          |
|   ├─ • explorer B                          running                          |
|   └─ … explorer C                          queued                           |
|                                                                            |
| Focus details: explorer A                                                    |
| Task, known status/outcome, progress, actions, [Open conversation]          |
+----------------------------------------------------------------------------+
```

This is a direct hierarchy, not a DAG. If the optional Map later visualizes this same data, a visible Outline/Table remains available with equivalent content and actions.

## 8. Responsive geometry and panel behavior

### 8.1 Target ranges

| Range | Session navigation | Center | Run Navigator | Debug Inspector |
| --- | --- | --- | --- | --- |
| Compact `<1000px`, or failed measured fit | Drawer | Full workspace | Drawer | Drawer opened from menu/command |
| Medium `1000-1439px` | Persistent 260px at the lower bound; may grow to 280px only when space permits | Preferred >=720px; hard floor 640px before falling back to compact | User-invoked overlay/drawer; never a layout track | Overlay/drawer |
| Wide `>=1440px` and measured three-pane fit passes | Persistent 260-288px | Hard minimum ~720px, fluid beyond | Persistent 260-300px if user preference allows and the center minimum still passes; otherwise overlay/drawer | Overlay/drawer |

The 1000px two-pane threshold is deliberately conservative:

```text
260px nav + 720px preferred center + <=20px dividers/gutters <= 1000px
```

The 1440px nominal wide threshold leaves useful slack beyond the narrowest three-pane arithmetic:

```text
260px nav + 720px hard center + 260px run nav = 1240px before dividers/gutters
```

Do not lower the full three-pane threshold to 1152px: even those minimum tracks need 1240px before dividers, gutters, resize affordances, scrollbars, and zoom/reflow effects. Width alone never forces three panes; the measured center-fit check is authoritative.

### 8.2 Behavior requirements

- Store user intent independently for session navigation, Run Navigator, and Debug Inspector.
- Choose presentation from both the nominal thresholds and measured available geometry. Below the 640px two-pane hard floor, use compact drawers; below the approximately 720px three-pane hard floor, demote Run Navigator to an overlay.
- Crossing a breakpoint constrains presentation but does not overwrite preference. For example, a preferred-open Run Navigator becomes a closed drawer trigger below a passing wide fit, then reopens when the wide fit passes again.
- On initial load, persist session navigation only when the two-pane hard floor is satisfied and persist Run Navigator only when the three-pane center minimum is satisfied.
- Re-evaluate on zoom/reflow and resize using actual available geometry, not viewport width alone where practical.
- Offer keyboard-accessible pointer resizing only at sufficiently wide widths. Use separators with `role=separator`, orientation/value semantics, arrow-key increments, Home/End bounds, and a reset action.
- Do not add resizing on compact/medium where it competes with content.
- Preserve safe-area padding on all drawer edges and bottom actions.
- At coarse pointers, make interactive target rectangles at least 40px and preferably 44px without requiring the visible icon to be that large.

## 9. Implementation workstreams

### 9.1 Routing and workspace-state foundation

**Outcome:** stable root identity and route-backed destinations.

Implementation slices:

1. Introduce a routing solution. Prefer a small established router already approved for the project; if dependency policy rejects one, implement only the route patterns above with `history.pushState`, `popstate`, and a tested parser—do not build a general router.
2. Add a typed `WorkspaceRouteState` parser/serializer and atomic transition helpers, including the optional typed Execution `conversation` query, canonical root/default elision, and explicit push-versus-replace operations.
3. Resolve any selected subagent to its `parent_session_id` when constructing a root route. If a direct parent cannot be loaded, show an error rather than guessing.
4. Split cache selectors: root execution projection keyed by `rootSessionId`; conversation snapshot/turns keyed by `conversationSessionId`.
5. Retain current per-session normalized caches. Rename “selected” concepts incrementally only where needed to avoid an unsafe big-bang refactor.
6. Derive the composer's explicit recipient object from the validated effective conversation, not generic selected state or a separate persisted recipient.
7. Persist only panel preferences and a no-route migration fallback; the URL wins for identity/navigation, and Back/Forward re-derives the recipient.
8. Add Ctrl/Cmd+K command palette navigation for projects/runs, Conversation, Execution views, agents, header actions, and Debug Inspector. Keep it command-oriented and keyboard searchable.

Acceptance details:

- A root execution query stays keyed to the root while a child conversation loads.
- Refreshing any canonical route restores the same workspace, root, effective conversation and derived recipient, focus, and optional handoff. An Execution URL without `conversation` restores the root conversation by definition.
- Back/Forward does not lose drafts or scroll position and does not re-send actions.
- Invalid required path/focus/handoff IDs render owned Retry/Back/Unavailable state rather than blanking the app; invalid optional Execution `conversation` values visibly fall back to root and are canonicalized with replace.

### 9.2 Run Navigator and Execution workspace

**Outcome:** replace the overloaded right Run Board with an attention-first view.

Run Navigator sections:

1. **Needs attention:** known failed/done-with-failures/cancelled work, actionable load failures, or explicitly available completion/handoff requiring review. Never infer an outcome from missing data.
2. **Active:** running/queued delegations and agents, with text + icon status and progress such as `2 of 3`.
3. **Recent:** bounded known completions from the available page. Until cursor paging exists, label limits (“Recent from loaded delegations”) rather than implying completeness.

Row requirements:

- The whole row is one navigation target; no nested buttons.
- Show status icon **and text**, primary label/role, progress, and known outcome.
- Use a sibling overflow Menu for actions such as Open conversation, View handoffs, Cancel, or Rerun.
- Keep raw IDs, “full,” `readonly_fanout`, and protocol statuses in details/debug, not as primary labels.
- Do not pulse every active row. A static icon/text treatment is sufficient; reserve motion for the currently observed live operation and honor reduced motion.
- Use owned Loading/Empty/Error/Retry panels for each section.

Execution Overview:

- Build a normalized direct hierarchy from the root `delegation.list` response.
- Outline is authoritative and supports keyboard navigation with ordinary links/buttons; do not prematurely use `role=tree` unless full APG tree keyboard semantics are implemented.
- Show root -> delegation -> direct agents only.
- Show a focused details region with task label/role, progress, known outcome/status, handoff availability, and scoped actions.
- If `has_more=true`, say that older delegations are not loaded and offer the backend-supported action available at that phase; do not present the outline as complete.

Activity:

- Initial frontend-only view is titled **Live Activity**.
- It may show events observed during the current connection/root subscription, with a clear “Live updates are not retained here” explanation.
- Batch announcements and visual updates; never reorder the currently focused row or steal focus.
- Rename to a retained **Activity** timeline only after the durable timestamped API ships. “Activity” may remain the route label, but the page heading/content must state the current fidelity.

Handoffs:

- Use existing handoff vocabulary and allowed file types.
- Load content only after explicit selection.
- Show file type, agent/delegation, availability, load state, copy/download actions, and a prose/code-friendly viewer.
- Do not call the page Artifacts or imply generalized metadata.
- Validate whether Handoffs has enough frequency to retain top-level Execution placement; the proposed default is top-level because it is a primary completion workflow.

Debug Inspector:

- Move session IDs, leaf IDs, metadata counts, pending action IDs, provider tool names, and raw protocol labels into a separate drawer.
- Open from header actions and Ctrl/Cmd+K.
- Make copy actions explicit with `CopyButton`.
- Ensure closing restores focus to the invoking control.

### 9.3 Session/project navigation

**Outcome:** semantic, compact navigation with fewer accidental actions.

Implementation slices:

- Render each project/session as one link or button with clear selected/current semantics and one sibling overflow Menu trigger.
- Replace three inline session actions with menu items: Rename, Archive/Unarchive, Delete.
- Keep status visible with icon/text available to assistive technology; do not rely on the 3px color rail alone.
- Distinguish list states: no sessions, no filter matches, archived hidden, loading, refresh failure.
- Rename user-facing **Host** after task-centered copy validation. Candidate: **Local sessions** with helper “Runs from your home directory.” Keep `host` only in route/debug terminology if needed.
- Preserve compact row density, but meet coarse-pointer target rectangles.
- Keep full-row navigation independent from the overflow menu and restore focus to the trigger after menu/dialog completion.

### 9.4 New-session setup and project settings

**New-session setup**

- Introduce an explicit `New session` setup state in the Conversation workspace.
- Preserve any existing `__new_session__` draft when entering/leaving setup; never clear it merely because New session was clicked.
- Colocate:
  - project/Host scope;
  - workspace inclusion and branch;
  - model;
  - reasoning effort;
  - optional title, if retained;
  - a clear “applies to this new session” description.
- Keep setup compact by default and use controlled disclosure for workspace detail.
- Validation errors stay next to the owning field and do not discard the draft.

**Model/reasoning settings**

- Replace separate always-visible model and reasoning selects with a combined settings Popover.
- Summary trigger should show the current model and reasoning in concise text.
- Explain locking after the first transcript entry and running-state restrictions in visible copy, not only a disabled-title tooltip.
- For an existing session, say “Applies to this session.” For new-session setup, say “Applies to the new session.”

**Project settings**

- Widen and clarify the existing settings layout as needed for workspace rows.
- State that project workspace edits affect **future sessions**; existing sessions retain instantiated workspace configuration.
- Add Delete project for empty projects using the existing backend capability documented at `rust/docs/websocket-rpc.md:862-865`.
- Use AlertDialog and expose backend `project_not_empty` as persistent actionable feedback.
- Keep workspace validation at the trust boundary and preserve safe busy dismissal.

### 9.5 Header actions, queue, and run controls

**Header/session actions**

- Add one actions Menu exposing **History**, **Export**, **Instructions** (current PI.md), and **Context Summary** where current data supports it.
- Retain `/switch`, `/export`, `/system`, `/compact`, and `/help` for expert users (`packages/web/src/slash.ts:12-18`).
- Do not claim Context Summary exists if only raw compaction markers are available; either derive it from an explicit current summary field or show an unavailable state until the contract exists.
- Use task-centered copy in primary UI:
  - Execution rather than Run Board;
  - Agents rather than subagents where precision is not needed;
  - Handoffs rather than artifacts;
  - Live Activity rather than timeline;
  - “Parallel agents” or “multiple agents” rather than fan-out;
  - “Send guidance” rather than steer;
  - “Conversation branch/current point” rather than leaf/root;
  - “Summarize context” rather than compaction.
- Preserve raw terms in Debug Inspector and detailed error payloads.

**Queue**

- Show a compact `Queue N` trigger near the recipient/composer.
- Desktop opens a Popover; mobile opens a Sheet.
- Each item displays explicit state: queued follow-up, guidance waiting, processing, or no longer editable.
- Put move/edit/delete/promote actions in a row Menu; keep the primary row readable.
- Label “Send as guidance” rather than icon-only steer.
- Offer Undo for queue deletion only if the backend can atomically restore or delay the destructive commit; otherwise do not fake Undo.
- Preserve queue revision fencing and add per-input pending/duplicate suppression.

**Scoped actions**

- **Stop** always names or exposes the exact conversation being stopped.
- **Send guidance** identifies the target agent.
- **Cancel delegation** states how many agents may be affected and uses AlertDialog when more than one can be affected.
- **Rerun** identifies the source delegation and handles missing task prompts as an owned error.
- Show pending state on the initiating control and suppress duplicates per target, not globally.
- Keep draft input enabled while disconnected, but disable connection-requiring actions with a visible explanation.

### 9.6 Transcript and shared content rail

**Rail and rhythm**

- Introduce shared layout tokens, for example:
  - `--content-rail-max` around the current 900-920px shell;
  - `--prose-measure` at approximately 72-80ch;
  - a wider technical measure for code, tables, diffs, and diagrams.
- Align transcript and composer edges. Current transcript rows use 900px and the composer 920px (`packages/web/src/styles.css:1129-1163`, `packages/web/src/styles.css:2758-2764`); converge through one shared rail.
- Increase separation between turns while keeping user/assistant/tool content inside one turn visually related.
- Use subtle current-turn background/edge emphasis and grouping; do not automatically wrap every turn in a heavy bordered card.
- Move **Show details** after the final assistant response into the turn footer. It currently renders before `TurnSummaryAssistant` (`packages/web/src/transcript.tsx:893-914`).

**Timestamps and navigation**

- Render a readable wall-clock `<time>` for each turn or message grouping using durable `timestamp_ms`.
- Show local wall-clock time in the UI and exact date/time/timezone in tooltip or accessible detail.
- Keep elapsed duration as a separate concept.
- Add a **Jump to latest** / `N new events` affordance when live content arrives while the reader is not sticky-bottom.
- Preserve current previous/next turn controls as secondary navigation; the new-event affordance solves a different problem.
- Do not auto-scroll readers who moved away from the bottom.

**Code and tools**

- Wrap every fenced code block with a header/action shell and per-block `CopyButton`.
- Cap tall code blocks and offer Expand/Collapse; preserve horizontal scrolling.
- Use a stable tool-group shell from one tool onward. The current one-tool path renders a separate stand-alone card (`packages/web/src/transcript.tsx:1396-1407`); unify the interaction without losing compactness.
- Keep controlled three-state group behavior. Do not replace it with `<details>`, which only models binary open/closed.
- Improve edit/diff rendering after navigation, correctness, and accessibility work. Candidate later improvements: file headers, clearer hunk context, wrapped-line markers, copy patch, and large-diff caps.

**Virtualization decision**

- Instrument render/commit time, interaction latency, node count, memory, and scroll correction first.
- Test long sessions with collapsed and expanded details.
- Add virtualization only if agreed thresholds are exceeded and a prototype preserves sticky-bottom, persisted positions, expanded detail, browser find expectations, and screen-reader reading order.

### 9.7 Composer and command discovery

- Show the explicit recipient above/in the composer: `To: Root conversation` or `To: reviewer agent`.
- Keep the recipient stable while execution focus changes.
- Make the platform-correct send shortcut visible near Send.
- Convert slash completion into a real combobox:
  - textarea/input has `role=combobox`, `aria-autocomplete=list`, `aria-controls`, and `aria-activedescendant`;
  - listbox options have stable IDs;
  - Arrow keys move active option without moving DOM focus;
  - Enter accepts the highlighted command;
  - Escape closes completion and returns to ordinary editing;
  - Tab behavior is deliberate and documented;
  - empty/no-match state is announced once.
- Add a compact command-help entry and command palette integration. `/help` should open persistent, navigable help instead of a four-second list notice.
- Keep Enter for newline and Ctrl/Cmd+Enter to send unless usability testing strongly supports an alternate explicit preference.
- Continue drafting while disconnected; show persistent connection state and defer/disable only actions that require the daemon.

### 9.8 Async, errors, and correctness

**Connection**

- Add a persistent connection/retry banner at workspace level for connecting, reconnecting, offline, or terminal connection error.
- Banner actions: Retry now, copy error details where useful, and dismiss only informational states—not unresolved blocking failures.
- Keep local drafting and reading cached transcript data available.
- Gate network mutations by connection/capability while preserving captured targets.

**Pending and duplicate suppression**

Track pending operations with typed entity keys, for example:

```text
rename:session-123
archive:session-123
queue-delete:session-123:input-456
cancel:root-1:delegation-2
rerun:root-1:delegation-2
settings:project-7
```

Required coverage: rename, archive/unarchive, project/session settings, queue promote/edit/delete/reorder, stop, guidance, delegation cancel, rerun, handoff load, and destructive delete.

**Owned state panels**

Use a shared `StatePanel` with variants:

- Loading;
- Empty/no data yet;
- No matches;
- Hidden archived data;
- Error with Retry;
- Unavailable/permission/capability;
- Stale/live-only explanation.

Do not collapse these into one “No sessions/delegations” message.

**Failure persistence**

- Toasts/notices remain for short confirmations.
- Failed loads stay in the owning surface until retried or resolved.
- Failed mutations stay attached to the relevant row/control and remain available after the four-second notice window.
- Do not expose raw stack text as primary copy; provide task-centered message plus expandable/copyable technical detail.

**Undo**

- Archive can support Undo by invoking unarchive against the captured session if backend state remains compatible.
- Queue delete needs backend semantics before promising Undo.
- Session/project permanent delete remains AlertDialog-confirmed and not undoable unless a real soft-delete contract is introduced.

### 9.9 Shared accessible primitives

Adopt primitives incrementally in this order:

1. `Dialog` and `AlertDialog`
2. `Menu`
3. route navigation / `Tabs`
4. `Sheet` / `Drawer`
5. `Popover`
6. `Disclosure`
7. `AsyncButton` / action-state wrapper
8. `IconButton`
9. `StatusIcon`
10. `StatePanel`
11. `CopyButton`
12. `Combobox`

Primitive requirements:

| Primitive | Required behavior |
| --- | --- |
| Dialog/AlertDialog | Accessible name/description, initial focus, modal focus trap, background inert, Escape policy, scrim policy, busy-state dismissal policy, and focus restoration. AlertDialog initially focuses the least destructive action. |
| Menu | Trigger linkage, roving focus, Arrow/Home/End, Escape, typeahead, disabled items, and restoration. Menu items are not nested inside row buttons. |
| Route nav/Tabs | Prefer links for routes. If tabs are used, implement roving focus, selected state, panel linkage, and activation policy. |
| Popover | Nonmodal focus policy, outside interaction, Escape, collision/viewport handling, restoration, and mobile fallback. |
| Sheet/Drawer | Modal behavior on compact, inert background, safe areas, initial/restore focus, and no offscreen focusable children when closed. |
| Disclosure | Button with `aria-expanded` and controlled content. Native `<details>` is reserved for truly binary disclosure. |
| AsyncButton | Idle/pending/success/error state, duplicate suppression, stable label/width where useful, and `aria-busy`/live copy without focus loss. |
| IconButton | Required accessible name, target-size contract, tooltip only as enhancement, visible focus. |
| StatusIcon | Icon plus text/accessible label; forced-colors treatment; no color-only meaning. |
| StatePanel | Owned title, explanation, Retry/action, semantic busy/error state, and no-layout-jump option. |
| CopyButton | Clipboard fallback/error, copied feedback announced without replacing focus, and explicit copied object label. |
| Combobox | APG keyboard/focus semantics, stable active option, no nested interactive option controls. |

Native `<dialog>` may be evaluated, but it is not a complete primitive by itself. The implementation still must define initial focus, modality/focus trap behavior, Escape and `cancel`, scrim dismissal, busy dismissal, nested overlay policy, portal/layer behavior, and focus restoration.

### 9.10 Visual system

- Keep Gruvbox light/dark palette, orange accent, Geist Sans/Mono, and Space Grotesk.
- Fix light `--muted-foreground` normal-text contrast. The exact replacement/mapping needs aesthetic validation, but every normal-size text/background pairing must meet 4.5:1.
- Review warning and success tokens on every surface where they carry text, not just icons.
- Disabled controls are WCAG contrast-exempt, but still need ergonomic legibility. Do not make disabled state indistinguishable from missing content.
- Raise required/actionable 11px text to at least the 12-13px rungs.
- Reduce pill proliferation: reserve pills for compact categorical state, not every count/metadata fragment.
- Reduce ambient pulsing. Keep motion for one salient live status, and use static state elsewhere.
- Reduce excessive bordered-card nesting. Prefer spacing, background tint, typography, and a single boundary for grouped interactive regions.
- Extend existing spacing/radius/elevation tokens with control-height tokens (compact/default/touch) and content-rail tokens. Migrate only components touched by each phase.
- If theme override remains supported, expose explicit **System / Light / Dark** selection and persist it. Current CSS supports system dark plus `.light`/`.dark` classes but no explicit user control is evident (`packages/web/src/styles.css:112-154`).
- Validate the updated palette in forced colors and do not suppress user color overrides unnecessarily.

### 9.11 Accessibility and live-update behavior

- Shared dialogs/drawers own focus transfer, trap, inert background, Escape, and restoration.
- Route navigation uses real links and current-page semantics; local tabs follow APG.
- A Map always has a visible Outline/Table equivalent with the same statuses, labels, and actions.
- Status is never color-only; labels must remain available at zoom and in forced colors.
- Batch live announcements into meaningful summaries such as “2 agents updated; 1 needs attention.”
- Do not move DOM focus, reorder the focused row, close a menu, or replace the active option due to live updates.
- Preserve stable React keys and row positions within attention sections; if an entity changes sections while focused, defer movement until focus leaves or provide a controlled announcement.
- Meet 40-44px coarse-pointer target rectangles.
- Verify safe areas, 200-400% zoom/reflow, forced colors, reduced motion, keyboard-only use, and no horizontal page overflow.
- Fix mobile history first: retain a visible expand/collapse control with a touch-sized hit area, or render alternate descendants expanded when that control cannot fit.
- Avoid incomplete semantics. A `role=tree` needs tree keyboard behavior; a `role=listbox` needs composite focus/selection behavior; a combobox must connect input and popup. Prefer simpler native list/link semantics when rich behavior is unnecessary.

## 10. Backend and API contract plan

### 10.1 Frontend-only capability boundary

The following can ship without backend schema changes:

- Route/state separation.
- Conversation/Execution IA and responsive shell.
- Direct root -> delegation -> direct-agent Outline from currently loaded `delegation.list`.
- Run Navigator sections based only on known loaded statuses/progress.
- Live Activity scoped to the current connection and clearly labeled non-retained.
- On-demand reads for currently known Handoffs.
- Debug Inspector separation, primitives, semantic rows, contrast, transcript/composer changes, connection gating, and owned async states.

Limitations must be visible: bounded delegation list, direct topology, unknown outcomes where absent, and non-retained live events.

### 10.2 Backend-dependent contracts

| Capability | Proposed contract work | Why it is required | Dependent UI |
| --- | --- | --- | --- |
| Complete delegation history | Add cursor-paged `delegation.list` or a new root execution endpoint with stable ordering, `next_cursor`, and filters; remove the 100-row completeness ceiling for pagination. | Current list is bounded/newest-first/per-parent with only `has_more`. | Complete Outline/Recent/Handoffs coverage. |
| Aggregate root execution projection | If multiple calls are otherwise expensive, add one projection returning root, delegations, direct agents, known attention status, progress, and handoff availability with a projection revision. | Prevent client N+1 and cross-query inconsistency while keeping root pinned. | Run Navigator and Overview convergence. |
| Delegation metadata | Add created/started/completed/updated timestamps, duration or enough timestamps to compute it, attempt/revision, last-change time, known outcome, and handoff availability metadata. | Current web `Delegation` has no timestamps/revision and list examples leave outcomes/files null. | Exact status recency, duration, safe rerun, attention sorting. |
| Change notification | Emit a root/delegation projection-change event containing root/parent ID and new revision, or a lightweight invalidation topic. | Current UI infers invalidation from subagent events and polls. | Efficient stable Run Navigator/Outline updates. |
| Durable execution events | Store/query timestamped normalized execution events with cursor pagination and stable IDs; define retention. Include event timestamp, root, entity ref, event kind, user-safe summary fields, and technical detail reference. | Reconnect events are not history and clear after idle. | Retained Activity timeline and deep links. |
| Handoff index | Return typed handoff metadata (kind, agent, delegation, availability, created/updated time, size, content type, safe display name) separately from content. | Current API reads one known file but does not provide a complete durable index. | Complete Handoffs list, filters, safe loading. |
| Generalized artifacts, only if needed | Add an artifact kind/schema only when product scope genuinely includes outputs beyond current handoffs. | Avoid relabeling four handoff file types as a general artifact system. | A future Artifacts product, not this plan's initial Handoffs. |
| True graph | Add explicit dependency/nesting edge types, stable node IDs, parent/root projection, and cycle/consistency rules. Enable nested delegation only as a deliberate runtime/product capability. | Current direct hierarchy and subagent tool filtering cannot represent a recursive/dependency DAG. | True DAG layout, critical path, dependency navigation. |
| Undo semantics | Add reversible archive/delete tokens or a delayed tombstone/restore API where product decides Undo is required. | Frontend-only Undo after an irreversible commit is misleading. | Queue-delete Undo; any future soft delete. |

### 10.3 Contract design requirements

- Cursor ordering must be deterministic and documented for concurrent inserts.
- Projections and mutations expose revisions/ETags suitable for optimistic concurrency.
- Event summaries contain no transcript/task content by default; content loads through authorized detail APIs.
- Errors have stable codes and task-centered fields so UI copy does not parse English strings.
- Capability/version flags allow old daemons to receive the frontend safely.
- Root execution queries validate that requested children belong to the root; the frontend must not infer membership from IDs.
- Performance budgets and maximum page sizes are part of the contract.
- Backend tests cover pagination without gaps/duplicates, revision races, event retention, handoff authorization/path safety, and edge consistency.

## 11. Phased delivery roadmap

Effort bands are planning aids, not calendar promises:

- **S:** roughly 1-2 focused engineer-days
- **M:** roughly 3-5 engineer-days
- **L:** roughly 6-10 engineer-days
- **XL:** cross-stack or more than 10 engineer-days; split before implementation where possible

Estimates exclude external review queues and user-research scheduling.

### Phase 0 — Baseline and guardrails

**Estimated effort:** M

**Dependencies:** none

**Backend changes:** none

Deliverables:

- Capture screenshots and interaction recordings for representative current states in light/dark and target widths.
- Add performance instrumentation for transcript render/commit, long-task/input delay, route transition, and delegation projection load.
- Define privacy-safe UX event names and a baseline dashboard.
- Add automated contrast checks for intended token/background pairs.
- Add a viewport/accessibility browser-test harness if none exists; retain current unit/Vitest suite.
- Codify invariants: per-recipient drafts, stale response rejection, per-session scroll, immutable mutation targets, and no color-only status.
- Create feature flags and daemon capability detection.

Exit gate:

- Baseline metrics are captured.
- Test fixtures cover root plus several delegations/agents, failed states, long transcript, branches, queued input, disconnected state, and handoffs.
- No implementation phase may remove the preserved invariants without an explicit design review.

### Phase 1 — Correctness and accessibility quick wins

**Estimated effort:** L

**Dependencies:** Phase 0 harness

**Backend changes:** none

Deliverables, in priority order:

1. Fix light muted normal-text contrast and review warning/success pairings.
2. Fix compact history expansion so alternate descendants are reachable.
3. Replace nested project/session row controls with semantic full rows plus one Menu.
4. Introduce the initial shared Dialog/AlertDialog foundation and migrate destructive/session/project dialogs.
5. Add persistent connection/retry banner and owned Error/Retry panels for key queries.
6. Distinguish no data/no matches/archived hidden/load failure.
7. Raise actionable microtext and coarse-pointer targets.
8. Add status text/icon pairing and forced-colors basics.

Exit gate:

- No nested interactive row markup remains in project/session navigation.
- Dialog keyboard/focus tests pass for initial focus, trap, Escape/busy policy, and restoration.
- Mobile alternate branches can be expanded and selected.
- Intended normal text meets WCAG AA contrast in both themes.
- Blocking load/connection failures persist and expose Retry.

### Phase 2 — Primitives and state foundation

**Estimated effort:** XL, split into route/state and primitive PRs

**Dependencies:** Phase 1 Dialog/Menu baseline

**Backend changes:** none

Deliverables:

- Routing and typed route-state adapter.
- `rootSessionId`, Conversation path state, typed optional Execution `conversation`, `focusedExecutionEntity`, workspace/execution view, and optional handoff selection, including validation and canonical defaults.
- Composer recipient derived only from the effective conversation, plus immutable mutation target helpers.
- Panel preference model that survives breakpoints.
- Remaining Menu, route nav/Tabs, Drawer/Sheet, Popover, Disclosure, AsyncButton, IconButton, StatusIcon, StatePanel, CopyButton, and Combobox foundations as needed by Phase 3.
- Ctrl/Cmd+K command palette shell.
- Dual-read migration from old UI resume storage to URL state.

Exit gate:

- Root and conversation can differ without changing the root execution query.
- Conversation and Execution deep links—including absent, valid non-root, and invalid Execution `conversation` queries—pass refresh, canonicalization, and Back/Forward browser tests.
- Focus-only changes never alter the effective conversation or recipient; explicit Message/Open conversation actions navigate to Conversation.
- Drafts and scroll survive route transitions.
- Mutation-target safety tests prove selection/focus changes cannot retarget an in-flight action.
- Overlay background inerting and focus restoration pass.

### Phase 3 — Shell, information architecture, and Execution Outline

**Estimated effort:** XL

**Dependencies:** Phase 2 state/routes/primitives

**Backend changes:** none for initial bounded/direct version

Deliverables:

- Route-backed Conversation and Execution destinations.
- Execution Overview/Activity/Handoffs route structure.
- Authoritative direct hierarchy Outline with honest completeness labels.
- Attention-oriented Run Navigator with Needs attention / Active / Recent.
- New responsive geometry: compact below 1000px, two-pane medium from 1000-1439px, and conditional three-pane wide from 1440px with measured center-fit fallbacks.
- Debug Inspector moved to separate drawer/command.
- Initial Live Activity explanation/state and currently known Handoffs access.
- Remove the old interactive Run Board at phase exit.

Exit gate:

- Opening an agent conversation leaves root execution pinned.
- Run Navigator full-row navigation never changes recipient implicitly.
- At 899px, 900px, and 999px both side surfaces remain drawers; at a passing 1000px fit, medium persists session navigation and overlays Run Navigator.
- Medium targets at least approximately 720px of center width and never crosses its 640px hard floor; wide never persists Run Navigator below the approximately 720px center hard minimum.
- Outline is keyboard usable and never labeled DAG.
- Live Activity states that it is non-retained.
- There is only one authoritative interactive run navigation view.

### Phase 4 — Workflow and component improvements

**Estimated effort:** XL, split by component

**Dependencies:** Phase 3 shell and target state

**Backend changes:** optional only for Undo

Deliverables:

- Explicit draft-preserving new-session setup with workspace/model/reasoning configuration.
- Combined model/reasoning Popover.
- Header actions Menu and command-palette actions.
- Queue count/tray with desktop Popover/mobile Sheet and row Menus.
- Scoped Stop/Send guidance/Cancel/Rerun with confirmations and per-entity pending states.
- Wider project settings, future-session semantics, and delete-empty-project action.
- Task-centered terminology pass with raw terms retained in debug/details.
- Persistent per-entity mutation failures and Archive Undo.

Exit gate:

- New-session draft survives entering/leaving setup and failed start.
- Every destructive/multi-agent action identifies its captured target and has correct pending/dismissal behavior.
- Queue remains operable by keyboard and coarse pointer without exposing a dense permanent button strip.
- Project edits explain future-session scope; empty project deletion handles success and `project_not_empty`.

### Phase 5 — Transcript and composer polish

**Estimated effort:** L-XL, split into rail, transcript actions, and combobox

**Dependencies:** Phase 2 primitives; preferably Phase 4 recipient/settings shell

**Backend changes:** none

Deliverables:

- Shared transcript/composer rail and 72-80ch prose measure with wider technical content.
- Improved intra-turn/inter-turn rhythm and subtle current-turn emphasis.
- Show details moved into turn footer after assistant response.
- Wall-clock/exact timestamps.
- Code-block Copy and height cap/expand.
- Jump to latest/new-event affordance.
- Stable one-or-more tool-group shell.
- Platform-correct shortcut display.
- APG slash combobox and persistent command help.
- Queue state labels/Undo only where supported.
- Performance review and explicit virtualization decision record.

Exit gate:

- Reading measure and technical overflow work at all matrix widths and 200-400% zoom.
- Live updates do not steal focus or force scroll.
- Every fenced code block can be copied and expanded keyboard-only.
- Slash completion passes combobox keyboard/screen-reader tests.
- Virtualization is either justified by measurements and validated or explicitly deferred.

### Phase 6 — Backend-supported Activity and complete Handoffs

**Estimated effort:** XL across daemon/store/web

**Dependencies:** backend pagination/revision/event/handoff contracts; Phase 3 routes

**Backend changes:** required

Deliverables:

- Cursor-paged complete delegation/root execution history.
- Projection revisions and change notifications.
- Durable timestamped execution events.
- Delegation timestamps, duration inputs, outcome, revision/attempt, and handoff availability.
- Handoff metadata index and complete Handoffs browsing.
- Upgrade Live Activity to retained Activity only after durable data is deployed and capability-negotiated.

Exit gate:

- Pagination tests prove no gaps/duplicates under concurrent updates.
- Activity survives disconnect, daemon restart, and idle event-buffer clearing.
- Every retained event displays a backend timestamp.
- Run Navigator/Outline/Handoffs communicate complete versus filtered results correctly.
- Old-daemon fallback remains honest and usable.

### Phase 7 — Optional Map and advanced graph

**Estimated effort:** M-L for a direct-hierarchy map prototype; XL/backend-dependent for a true graph

**Dependencies:** Outline complete; Phase 6 projection recommended; true graph contract for DAG claims

**Backend changes:** required for nested/dependency graph, not necessarily for a simple direct-hierarchy Map

Deliverables:

- User prototype comparing Outline alone versus Outline + Map.
- If valuable, optional Map rendering the same direct hierarchy with Outline visible as equivalent alternative.
- Map library decision based on accessibility, bundle/performance, layout stability, forced colors, pan/zoom keyboard support, and actual topology—not popularity.
- Only after explicit edge contracts: nested/dependency graph, cycle-safe layout, and accurate DAG terminology.

Exit gate:

- User evidence shows the Map improves a real task.
- Outline/Table parity is complete and visible.
- No color-only status and no inaccessible canvas-only information.
- “DAG” appears only when actual dependency/nesting edges exist and backend consistency tests pass.

## 12. Dependencies and critical path

```text
Phase 0 baseline
   |
   +--> Phase 1 early correctness (contrast, mobile history, rows, dialogs, errors)
   |       |
   |       +--> Phase 2 primitives + route/state split
   |                 |
   |                 +--> Phase 3 pinned root + IA + Outline + Run Navigator
   |                           |
   |                           +--> Phase 4 workflows/components
   |                           |
   |                           +--> Phase 5 transcript/composer polish
   |                           |
   |                           +--> Phase 6 complete history + durable Activity/Handoffs
   |                                     |
   |                                     +--> Phase 7 optional Map / true graph
   |
   +--> Backend contract design can begin during Phases 1-3
```

Hard ordering constraints:

1. Root/conversation/focus separation precedes a pinned Execution view.
2. Route parsing/history semantics precede broad deep-link UI.
3. Dialog/Menu/Drawer foundations precede multiplying overlays and row menus.
4. Mobile history, contrast, persistent errors, semantic rows, and dialog focus behavior land early.
5. Outline precedes Map.
6. Durable event storage/API precedes calling Activity a retained timeline.
7. Complete cursor pagination precedes claims of complete run history.
8. Actual nested/dependency edges precede a true DAG.
9. Backend Undo semantics precede queue-delete Undo.
10. Performance instrumentation precedes transcript virtualization.

Parallel opportunities:

- Backend contract design and store prototypes can proceed while frontend route/state work is underway.
- Visual token corrections and semantic-row work can proceed before the route migration.
- Transcript rail/code-block work can proceed after primitives stabilize, independently of durable Activity.

## 13. Acceptance criteria

### 13.1 Program-level functional criteria

- A user can pin a root run, inspect Execution, open a direct agent's conversation, send to that agent, and return to the exact Execution focus using Back.
- Root execution remains stable while conversation subject and focus change independently; recipient always follows the validated conversation and never execution focus.
- Every action visibly communicates and immutably captures its target.
- Conversation and Execution deep links restore the effective conversation and derived recipient on refresh; absent Execution `conversation` means root.
- Needs attention / Active / Recent rows show text + icon status, known progress/outcomes, and use full-row navigation.
- Technical inspection is separate from the Run Navigator.
- Outline accurately describes only loaded direct topology and its completeness.
- Activity fidelity and Handoff types are labeled honestly.
- No duplicate authoritative run views remain after Phase 3.

### 13.2 Accessibility criteria

- All functionality is reachable by keyboard with visible focus.
- Dialogs/drawers trap/restore focus appropriately and inert the background.
- Menus, route nav/tabs, popovers, disclosures, comboboxes, list/tree controls, and async actions meet their documented semantics.
- Live updates neither steal focus nor unexpectedly reorder the focused item.
- Status/outcome/selection is understandable without color.
- Outline/Table provides parity for any Map.
- Touch targets meet the agreed coarse-pointer minimum.
- The app reflows without lost functionality at 200% and 400% zoom and has no page-level horizontal overflow.
- Forced colors and reduced motion preserve meaning and operability.
- Mobile history exposes all alternate descendants.

### 13.3 Visual/content criteria

- Gruvbox, Geist, Space Grotesk, light/dark behavior, and warm identity remain recognizable.
- Normal text meets WCAG AA contrast in intended pairings.
- Actionable state is not 11px.
- Prose stays around 72-80ch while technical blocks can use the wider rail.
- Turn grouping is readable without a heavy card around every turn.
- Ambient pulses and pills are materially reduced.
- Explicit System/Light/Dark is available if theme override is retained.

### 13.4 Correctness/async criteria

- Draft text is never lost or moved to another recipient due to navigation, disconnection, or a failed request.
- Late responses and live events cannot overwrite another root/conversation/entity.
- Duplicate mutation submission is suppressed per entity while unrelated entities remain operable.
- Blocking/actionable failures persist with Retry.
- Drafting and cached reading work while disconnected; network mutations are gated.
- Cancel involving multiple agents uses AlertDialog and shows scope.
- Undo is offered only when the backend can honor it.

## 14. Test and validation matrix

This is the required future validation plan; this document does not claim these tests were run.

### 14.1 Automated unit/component coverage

| Area | Required cases |
| --- | --- |
| Route parser/serializer | Project and Host routes; every workspace/execution view; typed `conversation`/focus/handoff refs; absent-root defaults; canonical default elision; valid non-root round trip; malformed, unknown, and wrong-root conversation fallback with replace; invalid required IDs; legacy fallback. |
| State reducer | Root/conversation/focus independence; recipient derivation with no independent persistence; explicit Message/Open conversation transition; focus-only replace versus destination/detail push; atomic Back/Forward restore; panel preference constraints. |
| Mutation capture | Navigate after rename/archive/queue/cancel/rerun/Stop/Send guidance click; response updates only captured target; duplicate suppression per entity. |
| Cache/query keys | Root projection stays keyed to root while child transcript changes; stale revision/session responses no-op. |
| Run grouping | Needs attention/Active/Recent classification from only known data; unknown outcome; bounded/incomplete labels; stable focused row. |
| Outline | Direct hierarchy, `has_more`, no children, partial progress, failed/cancelled statuses, keyboard navigation, no DAG copy. |
| Async panels | Loading, empty, no matches, archived hidden, error, retry, unavailable, live-only. |
| Primitives | Focus lifecycle, Escape, busy dismissal, outside click, keyboard semantics, ARIA linkage, restoration, nested overlay policy. |
| Contrast/status | Token pairs in both themes; text/icon status independent of color; forced-colors snapshots where tooling permits. |
| History | Alternate branch collapsed/expanded at compact width; every descendant reachable. |
| Transcript | Show-details order, timestamps, code Copy/expand, new-event count, non-sticky scroll, tool group modes, reduced motion. |
| Combobox | Enter/Escape/Tab/arrows, active descendant, no matches, IME/composition safety, screen-reader names. |
| Queue | State labels, menu actions, revision conflicts, per-row pending/error, desktop Popover/mobile Sheet. |

Add component accessibility checks (for example an axe-compatible runner approved by the project) but do not treat automated checks as a substitute for manual assistive-technology testing.

### 14.2 Browser interaction viewport matrix

| Viewport/input | Required scenarios |
| --- | --- |
| Phone portrait (representative 320-430 CSS px) | Safe areas, drawers, history branches, queue Sheet, composer/keyboard, dialogs, no horizontal overflow. |
| Phone landscape | Short viewport, composer and modal reachability, code expansion, drawer close controls. |
| 768px tablet, coarse pointer | 40-44px targets, drawer behavior, no hover dependency, reflow. |
| 899px and 900px | Both remain compact under the new contract; explicit regression coverage for the old 900px cliff; both nav/run surfaces are drawers and state does not reset. |
| 999px and 1000px | Proposed compact-to-medium transition: 999px uses drawers; at a passing 1000px fit, session navigation is 260px and the center is approximately 720px or wider. |
| 1279px and 1280px | Both use two-pane medium behavior; explicit regression coverage for the current legacy cliff while new geometry replaces it. |
| 1366px laptop | Two-pane medium; center remains usable; no forced full three-pane layout. |
| 1439px and 1440px | Medium-to-wide transition; three panes at 1440 only when the measured center remains at least approximately 720px; verify preference restoration and overlay fallback. |
| Wide desktop (for example 1920px) | Persistent nav and preference-open Run Navigator when the fit passes, optional resizing, shared rail, Debug drawer. |

At every relevant width test both light and dark, keyboard and pointer, empty/loading/error/live states, long labels, and large text.

### 14.3 End-to-end scenarios

1. Open root Conversation -> Execution Outline -> focus agent -> open/message agent Conversation -> Back to the exact Execution focus and its root-derived recipient.
2. From an agent Conversation, enter Execution; verify its URL includes the typed non-root `conversation`, refresh restores that derived recipient, and execution focus changes do not alter it.
3. Open an Execution deep link without `conversation`; verify root conversation/recipient defaults. Open malformed, unknown, and wrong-root values; verify visible root fallback, canonical replace, and no extra Back entry.
4. Initiate mutation, navigate, receive success/failure; verify no retargeting.
5. Refresh every canonical deep link, including non-root conversation context and selected handoff.
6. Back/Forward across roots, conversations, execution views, focus replacements, and pushed details; verify recipient derivation, drafts, and scroll.
7. Disconnect during drafting and during each mutation class; reconnect/retry without duplicate commit.
8. Live updates while a row/menu/dialog/combobox option is focused; verify no focus theft or surprise reorder.
9. Multiple-agent cancellation confirmation and pending state.
10. Long transcript with expanded details, code blocks, non-sticky reader position, and new events.
11. Old daemon without new capabilities; verify bounded Outline, Live Activity copy, and disabled/unavailable backend-dependent features.
12. Durable Activity after Phase 6 across idle, reconnect, and daemon restart.
13. Handoff list/content permission, unavailable file, retry, Copy, and long content.

### 14.4 Zoom, color, motion, and platform checks

- Browser zoom at 200%, 300%, and 400%.
- OS/browser text scaling where available.
- Windows forced-colors/high-contrast mode.
- `prefers-reduced-motion: reduce`.
- Coarse pointer/touch target rectangle measurement, not visual estimation.
- Safe-area emulation/device testing for notches/home indicators.
- No page-level overflow; local technical scrollers remain keyboard reachable.
- Contrast measurements for every normal text token pairing and status text.

### 14.5 Manual assistive-technology checks

- **NVDA + Firefox:** routes/landmarks, session navigation, Run Navigator, Outline, Dialog/AlertDialog, Menu, Combobox, live announcements, status without color.
- **VoiceOver + Safari (macOS and iOS where practical):** rotor landmarks/links, drawers/sheets, composer recipient and shortcut, queue, history branches, code Copy/expand, focus restoration.
- Keyboard-only pass in Chromium and Firefox.
- Screen magnifier/reflow spot check at high zoom.

### 14.6 Performance validation

Instrument before/after:

- route-to-usable Conversation and Execution;
- root projection request/cache hit;
- transcript React render/commit duration by visible turn/detail count;
- input latency during streaming;
- scroll corrections/jumps;
- DOM node count and memory for long sessions;
- Map layout duration/bundle cost if prototyped.

Virtualization investigation begins only when measured long-session thresholds agreed in Phase 0 are exceeded.

## 15. Metrics and observability

### 15.1 Success metrics

Set numeric product thresholds after Phase 0 baseline; correctness/accessibility invariants need no baseline.

| Metric | Desired direction/guardrail |
| --- | --- |
| Misdirected message or mutation | **Zero** known incidents; automated target-capture tests for every mutation. |
| Draft loss after failed send/navigation | **Zero** in automated scenarios and telemetry-reported recovery failures. |
| Time from opening a root to locating a failed/blocked agent | Material reduction from baseline; validate Needs attention. |
| Steps/time to open an agent conversation and return to execution focus | Material reduction from baseline. |
| Route restoration success | 100% in canonical-route test suite. |
| Blocking load errors with visible Retry | 100% of owned query surfaces. |
| Accessible control/overlay violations | Zero critical/serious automated findings in covered surfaces; manual AT exit gates pass. |
| Contrast | 100% intended normal text pairings at WCAG AA. |
| Unexpected focus movement during live update | Zero in interaction suite/manual scenarios. |
| Center under minimum due to auto-open pane | Zero at matrix widths/zoom. |
| Transcript interaction latency | No regression beyond Phase 0 budget; virtualization remains measurement-driven. |
| Queue/cancel/rerun duplicate requests | Zero duplicates from repeated activation while target is pending. |

### 15.2 Proposed privacy-safe instrumentation

Do not record transcript, prompt, handoff content, workspace paths, raw IDs, or user-entered labels.

Suggested events/fields:

- `workspace_view_opened`: view, execution subview, viewport class, route restore/new navigation.
- `execution_entity_focused`: entity kind, source (outline/run navigator/palette), loaded completeness.
- `conversation_opened`: root versus agent, source.
- `message_submit`: recipient kind, connected state, accepted/rejected, latency bucket, restored-draft boolean.
- `mutation_attempt/result`: mutation kind, entity kind, duplicate-suppressed, stable error code, latency bucket.
- `state_panel_retry`: surface and result.
- `panel_toggled`: panel kind, mode, center-width bucket.
- `jump_to_latest_used`: unseen-count bucket.
- `command_invoked`: command name from a fixed allowlist, source.
- `a11y_preference`: reduced-motion/forced-colors/coarse-pointer booleans only for aggregate quality analysis where policy allows.

Correlate frontend request IDs with daemon request/error codes without storing content. Add dashboards for error rate by surface, stale-response drops, reconnect duration, projection freshness, and duplicate suppression.

## 16. Migration and rollout

### 16.1 Proposed flags/capabilities

- `web_workspace_routes`
- `web_execution_outline`
- `web_run_navigator`
- `web_accessible_overlays`
- `web_new_session_setup`
- `web_transcript_rail`
- daemon capability `execution_cursor_pagination`
- daemon capability `durable_execution_activity`
- daemon capability `handoff_index`
- experimental `web_execution_map`

Use existing configuration/flag infrastructure if present; do not create a permanent parallel product.

### 16.2 State migration

1. If a canonical route is present, it is authoritative.
2. If no route is present, read current `piRelayUiResume:v1` selection, resolve it as described below, and replace with the canonical Conversation route under the resolved root.
3. Continue reading existing draft and scroll keys so users do not lose work.
4. Do not add recipient or root identity preferences. If the old selection is a child, resolve its parent as `rootSessionId` and retain the selected child as the Conversation path subject; if parent resolution fails, show an error rather than guessing. Derive the recipient from the resulting route.
5. After a stable release window, stop writing obsolete selection fields; remove fallback only after usage confirms it is safe.

### 16.3 Run Board migration rule

- During development, a flag may allow comparison against the old board.
- In a user cohort, only one run view is visible and interactive at a time.
- If both must render for diagnostic comparison, the old one is visually hidden/noninteractive and excluded from the accessibility tree.
- Phase 3 cannot exit until the old Run Board is removed from normal product paths.

### 16.4 Backend rollout

- Ship capability advertisement and additive response fields first.
- Frontend detects capability and retains bounded Outline/Live Activity fallback.
- Deploy cursor pagination and durable events before enabling retained Activity copy.
- Backfill timestamps/events only if backend semantics can do so truthfully; do not synthesize event history from current state.
- Observe pagination/event load and error rates before broad enablement.

### 16.5 Rollback

- Route migration keeps a redirect back to the old entry path during the rollout window.
- Feature flags can disable new visual surfaces without deleting drafts/scroll/cache data.
- Backend additions are additive until all supported frontends migrate.
- A rollback must not restore two authoritative run views or reintroduce selection retargeting.

## 17. Risks and mitigations

| Risk | Impact | Mitigation |
| --- | --- | --- |
| State split creates inconsistent combinations | Wrong transcript, root, focus, or recipient | Typed atomic route reducer, server-backed root membership validation, canonical Execution `conversation`, recipient derivation with no independent persistence, invariant tests, and explicit recipient display. |
| Route migration loses resume/drafts | User work loss | URL-first/legacy fallback, preserve existing draft/scroll keys, migration tests, staged telemetry. |
| Bounded backend data looks complete | Misleading execution decisions | Visible incomplete labels, `has_more` state, no global totals until backend supports them. |
| Live Activity is mistaken for history | Lost trust after refresh | Heading “Live Activity,” retained-data explanation, never cache/synthesize as durable. |
| Attention section churn moves focus | Accessibility and control errors | Stable keys/order, defer focused-row movement, batch updates and announcements. |
| Overlay proliferation regresses focus | Keyboard/screen-reader traps | Shared primitives, overlay stack policy, initial/restore focus tests before adoption. |
| New two-/three-pane thresholds fail on real laptops/zoom | Cramped center | Preferred/hard geometry rules, 899/900/999/1000/1366/1439/1440 and zoom tests, compact/overlay fallback, and no auto-open below a minimum. |
| Contrast correction loses Gruvbox feel | Aesthetic rejection | Prototype a small set of token candidates in both themes; validate before global mapping. |
| Row menus hide important actions | Discoverability decline | Full-row primary action, visible overflow affordance, command palette/header actions, usability validation. |
| Backend aggregate projection becomes too broad | Coupling/performance | Start with explicit minimal fields, pagination and revisions; keep content/detail APIs separate. |
| Map adds bundle/performance/a11y cost | Slower, less usable UI | Outline first, user evidence, no library before topology warrants it, parity requirement. |
| Undo is promised without reversibility | Data loss or false recovery | Feature-gate on backend semantics; keep permanent deletes explicit. |
| Visual cleanup becomes a rewrite | Long-lived branch/regressions | Token migration only in touched components, small PRs, screenshots and exit gates. |
| Virtualization breaks scroll/AT | Transcript regressions | Instrument first; prototype against sticky scroll, browser find, expanded details, and reading order. |
| Terminology hides technical precision | Debugging difficulty | Task-centered primary copy with raw protocol terms and IDs in Debug Inspector/details. |

## 18. Decisions requiring prototype or user validation

These are bounded decisions, not reasons to defer the architecture:

1. **Contrast aesthetic:** exact lighter/darker Gruvbox token substitution while meeting 4.5:1.
2. **Breakpoint fit validation:** 1000px and 1440px are the proposed nominal thresholds; validate their measured 640px two-pane and approximately 720px three-pane fallback rules at 999/1000, 1366, 1439/1440, and high zoom. If measured budgets require a change, update the route-independent geometry contract and complete viewport matrix together.
3. **Route labels/path spelling:** Conversation/Execution labels are the target; exact URL spelling for project versus Host scope needs implementation validation.
4. **Handoffs placement:** proposed as a top-level Execution destination; validate frequency and whether sparse states would be clearer nested under Overview.
5. **Outline detail presentation:** focus-only changes replace history; validate which details are durable enough to receive a pushed drawer/detail route.
6. **Map value/library:** whether a Map helps at all for direct topology, and later which library meets accessibility/performance needs.
7. **Wide resizing:** whether users benefit enough to justify accessible resize controls; do not ship pointer-only resizing.

## 19. Traceability checklist

| ID | Recommendation / required outcome | Primary phase(s) | Verification |
| --- | --- | --- | --- |
| R01 | Preserve progressive transcript disclosure and controlled tool detail | 0, 5 | Component/interaction regression tests |
| R02 | Preserve per-session drafts, scroll, stale-response guards, warm cache | 0, 2, 5 | State, routing, long-transcript tests |
| R03 | Keep Gruvbox, Geist/Space Grotesk, light/dark, reduced motion | 1, 5 | Visual/contrast/motion matrix |
| R04 | Split root, conversation, execution focus, and mutation target; derive recipient only from conversation | 2 | Reducer, recipient-derivation, and in-flight retarget tests |
| R05 | Route-backed Conversation and Execution with typed Execution `conversation`, canonical defaults, validation, and push/replace semantics | 2, 3 | Deep-link/refresh/canonicalization/Back/Forward tests |
| R06 | Execution has Overview, Activity, Handoffs; Overview has Outline first | 3 | Route and IA tests |
| R07 | Do not call direct topology a DAG | 3, 7 | Copy review and fixtures |
| R08 | Replace Run Board with Needs attention / Active / Recent Run Navigator | 3 | Navigation/status tests and old-board removal gate |
| R09 | Separate Debug Inspector | 3 | Command/menu and focus-restore tests |
| R10 | Honest bounded/newest-first/per-parent handling | 3, 6 | `has_more` states and pagination tests |
| R11 | Live Activity before durable retained Activity | 3, 6 | Copy/capability tests and restart scenario |
| R12 | Handoffs, not generalized Artifacts | 3, 6 | Type/copy review and handoff tests |
| R13 | Compact `<1000`, two-pane medium `1000-1439`, conditional three-pane wide `>=1440`, and explicit preferred/hard center minimums | 3 | 899/900/999/1000/1279/1280/1366/1439/1440/zoom matrix |
| R14 | Do not lower full three-pane breakpoint to 1152 | 3 | Geometry assertion/design review |
| R15 | Preserve panel preference across breakpoints | 2, 3 | Resize/zoom state tests |
| R16 | Shared transcript/composer rail, prose/technical measures, turn rhythm | 5 | Visual/overflow/zoom tests |
| R17 | Show details after assistant response | 5 | Render-order component test |
| R18 | Semantic project/session rows with one Menu | 1 | DOM/a11y/keyboard tests |
| R19 | Draft-preserving new-session setup; colocated workspace/model/reasoning | 4 | Setup navigation/failure tests |
| R20 | Combined model/reasoning Popover | 4 | Keyboard/state/locking tests |
| R21 | Header actions plus retained slash commands | 4 | Menu, command palette, slash tests |
| R22 | Compact queue tray, row menus, labels, supported Undo only | 4, 5, 6 | Desktop/mobile/revision/Undo tests |
| R23 | Scoped Stop/Guidance/Cancel/Rerun, confirmation, pending | 4 | Target capture and multi-agent AlertDialog tests |
| R24 | Project future-session semantics and delete-empty-project | 4 | Copy, success, `project_not_empty` tests |
| R25 | Persistent connection banner and owned state panels | 1, 4 | Offline/retry/state distinction tests |
| R26 | Shared accessible primitives and complete modality contracts | 1, 2 | Unit a11y + manual AT |
| R27 | Use `<details>` only for binary disclosure; controlled 3-state tools | 2, 5 | Component API/review |
| R28 | Fix light muted contrast; review warning/success/disabled legibility | 1 | Automated/manual contrast |
| R29 | Reduce actionable 11px text, pills, pulse, bordered cards | 1, 4, 5 | Visual review and computed-style checks |
| R30 | Incremental spacing/control/radius/elevation tokens | 1-5 | Per-PR style review |
| R31 | Explicit System/Light/Dark if override remains | 4 | Preference/system-change tests |
| R32 | Focus/inert/restore, route/APG semantics, Map parity | 1-3, 7 | Primitive, route, manual AT tests |
| R33 | Batch live announcements; no focus steal/reorder | 3, 6 | Live-update focus tests |
| R34 | Coarse-pointer targets, safe areas, zoom, forced colors, motion | 1-7 | Full validation matrix |
| R35 | Fix compact history hidden-toggle bug | 1 | Phone branch reachability test |
| R36 | Wall-clock/exact timestamps | 5 | Locale/timezone/accessibility tests |
| R37 | Per-code-block Copy and cap/expand | 5 | Code component tests |
| R38 | Jump to latest/new-event affordance | 5 | Non-sticky streaming tests |
| R39 | Stable tool shell from one tool onward | 5 | One/many/live tool fixtures |
| R40 | Platform-correct shortcut and APG slash combobox/help | 5 | Platform copy, keyboard, AT tests |
| R41 | Virtualization only after measurement | 0, 5 | Performance decision record |
| R42 | Connection-aware gating while drafting remains available | 1, 4 | Offline composer/mutation tests |
| R43 | Per-entity pending/duplicate suppression | 4 | Concurrent entity interaction tests |
| R44 | Persistent actionable failures and distinct empty states | 1, 4 | StatePanel tests |
| R45 | Task-centered terminology, raw terms only in debug/details | 3, 4 | Content review |
| R46 | Complete cursor-paged delegation history | 6 | Store/API pagination tests |
| R47 | Aggregate root projection if needed, with revisions/change event | 6 | Contract/load/convergence tests |
| R48 | Durable timestamped execution events | 6 | Idle/reconnect/restart tests |
| R49 | Delegation timestamps/duration/revision/outcome | 6 | Contract and UI rendering tests |
| R50 | General artifact metadata only if scope expands | Deferred | Product/API review gate |
| R51 | Actual nesting/dependency edges before true graph/DAG | 7/deferred | Backend consistency tests |
| R52 | Ctrl/Cmd+K command palette | 2, 4 | Keyboard/navigation tests |
| R53 | Required viewport, AT, contrast, performance validation | 0-7 | Phase exit reports |
| R54 | Metrics, observability, rollout flags, migration, rollback | 0-7 | Dashboard and rollout checklist |
| R55 | No duplicate authoritative run surfaces | 3 | Feature-flag/product-path audit |
| R56 | No wholesale rebrand or premature graph/durable timeline claims | All | Design/content review |

## 20. Overall Definition of Done

The UI improvement program is done when:

1. Conversation and Execution are route-backed and restore their canonical effective conversation reliably.
2. Root run, conversation subject, execution focus, and mutation target are independently correct and visibly understandable; recipient is visibly and exclusively derived from conversation.
3. The old Run Board is gone from normal product paths; the Run Navigator, authoritative Outline, and separate Debug Inspector cover their intended jobs.
4. Responsive navigation prioritizes the center and session navigator, with accessible overlays, preserved preferences, and no center-width violations.
5. Early correctness/accessibility issues—including contrast, mobile history, semantic rows, persistent failures, and dialog focus—are fixed.
6. Workflow, transcript, composer, queue, settings, and terminology changes meet the acceptance and validation matrices.
7. Activity and graph language match backend truth: retained Activity has durable timestamped data, and DAG claims have real edges.
8. Automated checks, the full browser/zoom/color/motion matrix, and manual NVDA/Firefox plus VoiceOver/Safari passes meet phase exit gates.
9. Observability shows no draft loss, target retargeting, duplicate destructive mutations, or critical accessibility regressions, and product task metrics improve from the Phase 0 baseline.
10. Migration flags and legacy state paths are retired or have a documented support end state, with no duplicate authoritative run views.
