# Draft Sessions And History Editing Plan

This plan describes how the web UI should handle empty new sessions, unsent
composer drafts, rewind, and fork. The goal is to avoid durable empty Postgres
sessions while preserving the current transcript model and our existing
resume/rewind/fork semantics.

## Goals

- A blank newly opened chat should not create a durable `sessions` row.
- Unsent brand-new drafts should survive browser refresh.
- The first real user action should create durable state atomically.
- Rewind and fork should use visible transcript points, not internal turn
  boundary rows, as the user's mental model.
- Historical user-message targets should restore that message into the
  composer so the user can edit and resend it.
- UI composer drafts should be decoupled from the agent/session data model. The
  web UI may depend on transcript/session state, but the core session schema
  should not depend on web UI draft state.

## Non-Goals

- Do not remove low-level root rewind support from the daemon.
- Do not make drafts part of `transcript_entries`.
- Do not store composer drafts in `sessions.metadata`.
- Do not add a general session lifecycle enum or explicit open/close/resume
  API.
- Do not add task abstractions.

## UI And Backend Boundary

The Rust backend owns durable agent/session semantics. The web UI owns how a
human edits, stages, and navigates those semantics.

### Rust Backend RPC Should Own

- Durable session creation once there is meaningful state.
- Atomic creation of a session plus first materialized user input.
- Queued input persistence, idempotency, ordering, replacement, and cancellation
  while the input is still queued.
- Queued follow-up promotion to steer priority while the input is still queued.
- Interrupting active work.
- Rewind as a core active-leaf mutation to root or a valid turn boundary.
- Fork as a core branch-copy operation from an explicit transcript entry.
- Provider configuration, global system prompt configuration, tool definitions,
  transcript/history reads, and transient reconnect event replay.
- Recovery after daemon death and stale action handling.
- Validation of core invariants: no source-mutating history operations while
  busy, no fork from null, no rewind to a non-boundary, no replacing already
  consumed input.

Backend methods should be shaped around domain operations that are meaningful
without a particular frontend. For example, `history.rewind` should not know
about a text area, but it should know how to move the active transcript path.

### Web UI Should Own

- Brand-new unsent draft sessions.
- Composer text, including restored historical messages.
- Sidebar grouping, draft display, selected-session state, and local draft
  persistence.
- Picker labels and the visible mapping from transcript rows to user actions.
- Turning a visible user-message target into "previous safe boundary plus
  restored composer text."
- Deciding when to combine primitives into one user gesture, such as
  interrupt-then-rewind-then-restore-composer for "edit last message."
- Rendering policy for Markdown/HTML, tool cards, hidden turn bookkeeping, and
  history picker affordances.

UI state may depend on core transcript/session data, but core session data must
not depend on web UI state.

### Separate Web-Owned Persistence, If Needed Later

If local `localStorage` drafts are not enough, add a separate web-owned
persistence surface instead of mixing UI drafts into core tables. Options:

- A `web_client_state` table keyed by client/user/session.
- A small web companion service.
- Browser sync/storage if the target remains personal-use.

That surface can store composer drafts, sidebar preferences, selected session,
expanded tool rows, or picker state. It should not be required for daemon
recovery, provider execution, transcript materialization, or fork/rewind
validity.

### Borderline Cases

`session.start` belongs in Rust because it is not just UI convenience; it
prevents an invalid durable intermediate state by atomically creating a session
and its first input.

Queued inputs are durable backend-owned work, but the websocket UI no longer
edits or cancels queued rows directly. The visible edit path is interrupt plus
rewind/fork picker semantics; queued rows can only be promoted to steer
priority before the daemon claims them.

`history.fork` belongs in Rust because it creates durable session provenance
and, for project sessions, copies workspace state. While workspace state is
copied from the live checkout, fork is limited to the current active completed
turn or compaction root.

`restore this text into the composer` does not belong in Rust. It is a UI
consequence of choosing a historical user message.

## Vocabulary

- **Frontend draft session**: A browser-local unsent chat draft. It has a draft
  id, title, provider config, and composer text, but no Postgres session row.
- **Durable session**: A Postgres `sessions` row with at least one meaningful
  durable state marker: queued input, transcript entries, fork metadata, or
  actions from real use.
- **Composer draft**: Text currently in the message box. Composer drafts live in
  web/client state, not in the core session row. Initially this is browser
  storage keyed by draft id or durable session id.

## Frontend Draft Sessions

Clicking `New session` creates a local draft selection. No durable session row
is created until `session.start`.

Draft shape in browser storage:

```json
{
  "draft_id": "draft_...",
  "title": "New session",
  "provider": {
    "kind": "codex",
    "model": "gpt-5.5",
    "prompt_cache": { "key": "pi-relay-web" }
  },
  "composer": "",
  "created_at": 1770000000000,
  "updated_at": 1770000000000
}
```

Use `localStorage` initially. IndexedDB is unnecessary unless draft payloads
grow beyond text/provider/title. Drafts should appear in the session sidebar
above durable sessions with a local-only visual treatment.

When the user types in a frontend draft, update the local stored draft so it
survives browser refresh.

## Starting A Durable Session

Sending the first message from a frontend draft should create the durable
session and materialize the first input in one backend transition.

Preferred RPC:

```json
{
  "method": "session.start",
  "params": {
    "session_id": "optional",
    "provider": { "kind": "codex", "model": "gpt-5.5" },
    "metadata": { "title": "New session", "created_by": "web" },
    "client_input_id": "web_...",
    "priority": "follow_up",
    "content": [{ "type": "text", "text": "hello" }]
  }
}
```

Behavior:

- Insert `sessions`.
- Feed the first user message into the session.
- Insert durable `session.created`, `input.accepted`, transcript, action, and
  derived session events in one transition before dispatch.
- Return the real `session_id`.

The frontend then removes the local draft, selects the real session id,
subscribes to its events, and clears the composer.

The websocket contract has no empty-session creation RPC; harness/manual flows
should use `session.start` or direct fixtures.

## UI Draft Store For Existing Sessions

For durable sessions, store restored/editable composer drafts in the web UI's
own draft store, not in `sessions.metadata`.

```json
{
  "session_id": "session_...",
  "base_active_leaf_id": "entry_...",
  "draft": {
    "content": [{ "type": "text", "text": "historical message" }],
    "source_entry_id": "entry_...",
    "source": "rewind" ,
    "updated_at": 1770000000000
  }
}
```

Rules:

- Loading/selecting a durable session hydrates the composer from the UI draft
  store if a draft exists for that session.
- Sending that composer content clears the UI draft after the input is accepted.
- The backend does not know about this draft, and does not need to. If another
  UI opens the same session without the local draft, it still sees a valid
  active transcript state.

Use `localStorage` initially. If server-side UI state becomes important later,
add a web-owned `web_client_state` table or service that is separate from the
core session tables. Do not put the draft blob into `sessions.metadata`.

## Rewind Semantics

The web picker should show visible transcript entries as targets. It should not
ask the user to pick hidden `TurnFinished` bookkeeping rows.

User-message target:

- Means "go back to the moment before this message was sent."
- Replaces the composer with that historical user message.
- Moves `active_leaf_id` to the previous safe turn boundary.
- For the first user message, moves `active_leaf_id` to `null`.
- Stores the restored message in the UI draft store after the rewind succeeds.

Completed-turn target:

- Means "go back to after this completed turn."
- Moves `active_leaf_id` to the selected turn boundary.
- Composer handling can preserve any current draft unless the target is a user
  message. The important special case is user-message replacement.

The current UI intentionally hides a bare root rewind option. The first user
message target is the visible way to rewind to root while preserving that
historical message in the composer.

Tool-result or mid-turn target:

- Rewind remains boundary-only, so the picker should either hide these for
  rewind or map them to the previous safe boundary with clear copy.
- Prefer hiding them initially to avoid surprising lossy behavior.

Backend shape:

- Keep raw `history.rewind { leaf_id: null }` valid.
- `history.rewind` remains a core history operation: validate that `leaf_id` is
  root or a turn boundary, then write `sessions.active_leaf_id`.
- The web UI computes the prior safe boundary for user-message targets and
  manages the composer draft locally.

## Fork Semantics

The picker should present one action: **Fork current state**.

Fork only targets the current active completed turn or compaction root. It
creates the child at that same boundary and starts with an empty composer.
Historical user-message, older-boundary, assistant-message, tool-result, and
other mid-turn entries are not valid fork targets until workspace checkpointing
exists.

Core provenance should be represented by core events. A forked child's
`session.created` event should identify the source session and source entry.

RPC shape:

- The frontend sends the current `active_leaf_id` as `leaf_id`.
- `expected_active_leaf_id` protects the picker choice from stale source
  history.
- The backend rejects older boundaries with `not_active_leaf`.

## Empty Session Pruning

After the UI stops creating durable blank sessions, prune existing accidental
empty web sessions.

Candidate definition:

- `metadata.created_by = "web"`
- no `transcript_entries`
- no `queued_inputs`
- no `actions`
- no durable fork metadata that should be kept

First run a read-only count/list query and inspect candidates. Delete only after
the candidate set looks correct.

Future `session.list` can also hide durable empty sessions defensively, but the
primary fix is to stop creating them.

## RPC Changes

Implemented:

- `session.start`
  - Atomically creates a session and first materialized input.
- `history.fork` accepts only the current active turn boundary and rejects older
  boundaries or non-boundary entries.
- Optional `expected_active_leaf_id` on user input and rewind RPCs for stale
  picker/draft protection.
- Durable consumed input ledger rows for idle accepted inputs that include
  `client_input_id`.

Consider:

- No backend draft RPCs for the first pass. Draft storage belongs to the web UI.
- If cross-device draft sync becomes important later, add a separate web-owned
  UI state surface rather than mixing drafts into the core session schema.

## Frontend Changes

- Add local draft session state backed by `localStorage`.
- Merge local drafts and durable sessions in the sidebar.
- Make the composer work when selected id is a draft id.
- On send from draft id, call `session.start`.
- On successful `session.start`, replace draft selection with real session id.
- Hydrate composer from the UI draft store for durable sessions.
- Clear UI draft state after the restored draft is sent.
- Update rewind picker options to visible transcript points.
- Keep fork picker options limited to the current active boundary.
- Historical user messages replace current composer contents.

## Interrupt To Edit Last Message

The clean user flow for "I sent this, stop and let me edit it" should be a
single UI action even though it uses existing primitives underneath.

When the last user message has already been consumed into the transcript and
the model/tool turn is running:

1. User chooses "Edit last message" or picks the last user message in the
   rewind picker.
2. UI sends `input.interrupt`.
3. UI waits until the session is idle and the interrupted turn tail is durable.
4. UI computes the previous safe boundary for that user message.
5. UI calls `history.rewind` to that boundary/root.
6. UI writes the selected user message into the UI draft store and composer.
7. User edits and sends; the new input starts from the rewound context.

The interrupted branch remains in the append-only transcript forest. Rewind
only changes the active path, which preserves our existing semantics.

If the message is still queued and has not become a transcript entry yet, rewind
is the wrong primitive because there is nothing to rewind to. For the current
personal-use UI, queued rows are left alone except for `input.promote_queued`;
editing uses interrupt plus rewind/fork once the message is part of transcript
history.

## Race Conditions And Required Invariants

This design has several races unless the backend exposes conditional,
idempotent operations and the UI keeps draft context explicit.

### Draft Start Retried Or Double-Sent

Risk: the user sends from a frontend draft, the websocket response is lost, or
two tabs send the same draft. A naive `session.start` retry could create two
sessions or enqueue the first message twice.

Invariant:

- Every frontend draft gets a stable `draft_id`.
- `session.start` carries a stable client-chosen `session_id` derived from the
  draft, plus a stable `client_input_id`.
- `session.start` is one transaction and is idempotent for that stable
  `session_id`: retrying returns the already-created session.
- The UI only deletes its local draft after it has mapped the draft id to the
  returned durable session id.

### Queued Input Edit Versus Queue Consumption

Risk: the daemon reads a queued input into memory, the user edits/cancels that
queued row, and the daemon later appends the old content into transcript.

Invariant:

- The daemon must claim a queued input before materializing it.
- Once claimed, status becomes `consuming` with a claim/attempt id. UI steering
  promotion returns `"promoted": false` with the current row status; editing
  should use interrupt+switch if the message appears in transcript.
- Transcript append and final `consuming -> consumed` validate the claim id in
  one transaction.
- Daemon recovery resets abandoned `consuming` rows to `queued` before
  continuing. Because the transcript append and consumed mark are one
  transaction, there is no committed transcript-with-unconsumed-claim split to
  reconcile.

This is stronger than the current read-then-update pattern and prevents stale
queued content from becoming transcript after a user edit.

### Interrupt Versus Model Completion

Risk: the user interrupts at the same time the model completes. Without attempt
guards, the completed model could append after the interrupt path closes the
turn.

Invariant:

- Each action attempt has a durable `attempt_id`.
- Interrupt marks unfinished actions interrupted/stale in the same transaction
  that appends the interrupted turn tail.
- Model/tool completion must update by `(session_id, action_row_id, attempt_id,
  status in ('pending','running'))`.
- If the update affects zero rows, the completion is stale and cannot append
  transcript.

This is already the shape of the current action handling; the edit-last-message
workflow depends on preserving it.

### Rewind/Fork Picker Uses Stale History

Risk: the user opens a picker, the session advances, then the user selects an
old visible target. The UI might compute the wrong "previous boundary" or fork
from a target that is no longer on the active branch.

Invariant:

- Picker actions send the explicit source `entry_id` plus the expected
  `active_leaf_id` or last seen event id.
- Backend validates that the entry exists and, for source-mutating rewind, that
  the session is still idle and at the expected active leaf.
- If the active leaf changed, return `history_changed`; the UI refreshes the
  tree and asks the user to pick again.
- Fork is source-non-mutating, but it still validates the expected active leaf
  and rejects non-boundary targets.

### UI Draft Sent Into The Wrong Context

Risk: a restored composer draft is local UI state. Another tab or operation can
move the durable session active leaf before the draft is sent, causing the draft
to be submitted into a different context than the one it was restored from.

Invariant:

- UI draft records include `base_active_leaf_id`.
- `input.follow_up` accepts optional `expected_active_leaf_id`.
- Visible steering is `input.promote_queued` on a queued follow-up row.
- If the backend sees a mismatch, it rejects with `history_changed`; the UI
  refreshes and asks whether to keep the draft against the new context.

This keeps drafts UI-owned while still letting the backend protect the core
session from stale-context input.

### Rewind To User Message While Work Is Running

Risk: the user chooses a user message to edit while the turn is still running.
If the UI tries to rewind immediately, the backend correctly returns
`session_busy`.

Invariant:

- The UI action should be an orchestrated gesture:
  interrupt, wait for idle, refresh history, verify the source user entry still
  exists, rewind to the computed previous boundary, then restore the composer
  draft locally.
- If interrupt completion races with model completion, action attempt guards
  decide the winner. The UI refreshes after idle and computes from durable
  state.

### Fork From First User Message Produces A Child With No Transcript

Risk: pruning or session list code might mistake this valid child for an
accidental empty session.

Invariant:

- `history.fork` writes durable fork lineage into the child session metadata.
- Empty-session pruning excludes sessions with fork metadata.
- The local UI draft attached to the child is not part of core validity.

### LocalStorage Multi-Tab Draft Conflicts

Risk: two browser tabs edit the same draft/session composer. Last write can
silently overwrite text.

Invariant:

- Each UI draft has `updated_at` and optionally a monotonically increasing
  local `version`.
- Listen for `storage` events. If another tab changes the selected draft while
  the current composer is dirty, show a conflict notice or keep the current tab
  version until explicit reload.
- This is UI-only conflict management; it should not leak into the Rust session
  model.

### Empty Session Pruning During Active Creation

Risk: a cleanup job deletes a just-created session before its first queued input
or fork metadata is visible.

Invariant:

- `session.start` creates session and initial input atomically, so this cannot
  happen for web-created first-message sessions.
- Manual pruning should ignore very recent sessions. The UI never creates empty
  durable sessions eagerly.
- Automated pruning, if added, should require an age threshold and no actions,
  queued inputs, transcript entries, or fork metadata.

### Event Subscription And Draft Cleanup

Risk: the UI starts a session, deletes the local draft, but misses the
`session.created`/`input.queued` events due to reconnect.

Invariant:

- After `session.start`, the UI should subscribe from its last known
  `last_event_id` and refresh `session.get` with `include_entries=true`.
- Event replay is a transient reconnect buffer while a session is active; UI
  correctness after reconnect should come from refreshing durable state with
  `session.get`/`history.tree`.

## Test Plan

Manual browser/RPC tests should cover real behavior, not stub-only checks:

1. Click `New session`, type a draft, refresh browser.
   - Draft remains visible.
   - No Postgres `sessions` row exists.

2. Send from a brand-new draft.
   - Exactly one durable session row is created.
   - Initial transcript/action/event state exists without an intermediate
     queued state; if `client_input_id` is present, the input ledger row is
     already `consumed`.
   - No durable empty session is left behind if the websocket response is lost
     and retried with the same `client_input_id`.

3. Rewind to first user message.
   - Durable session `active_leaf_id` becomes `null`.
   - UI draft store contains the first user message.
   - Composer shows that message after reload.

4. Send after rewind-to-first-message.
   - UI draft store entry clears.
   - New turn starts from root with edited content.

5. Rewind to a later user message.
   - `active_leaf_id` becomes the previous completed turn boundary.
   - Composer is replaced by the selected historical message.

6. Fork from an older boundary.
   - Backend rejects the target as `not_active_leaf`.

7. Fork from a completed turn.
   - Child active leaf is the same current boundary.
   - Composer is empty.

8. Fork from mid-turn tool/result point.
   - Backend rejects the target as `not_turn_boundary`.
   - The child session is not created.

9. Prune existing empties.
   - Candidate list excludes sessions with transcript entries, queued inputs,
     actions, or fork metadata.

10. Interrupt to edit last message.
    - While a model turn is running, the UI interrupts, waits for idle, rewinds
      to before the last user message, and restores that message into the
      composer from UI draft state.

11. Promote still-queued input.
    - If an input is still `queued` and not yet transcript, `input.promote_queued`
      can move it into the steer queue. Once claimed or consumed, promotion is
      reported as a normal no-op and editing remains picker-based after
      transcript materialization.

## Open Implementation Notes

- For UI draft storage, `ContentBlock[]` is better than a bare string so
  image-containing drafts remain representable later even if the first UI only
  supports text.
- Initial local draft persistence can focus on text because the current composer
  only sends text.
- The backend should not read or write UI composer drafts in the core session
  tables. Keep any future server-side UI draft sync in a separate web-owned
  state surface.
- Documentation must be updated alongside implementation:
  `websocket-rpc.md`, `design-decisions.md`, and `WORKLOG.md`.
