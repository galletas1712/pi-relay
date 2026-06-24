# Session runtime hot-path architecture plan

## Problem statement

Sending a follow-up message to a long, idle session can take several seconds before the frontend receives the current `input.follow_up` response. This document sometimes uses `input.queue` as the generic durable command boundary, but the existing RPC name is `input.follow_up`. The suspected cause is not a frontend pre-flight transcript fetch. The hot path currently can synchronously wake the daemon runtime for the session, which may require loading and reconstructing a full `AgentSession` from all persisted transcript entries.

The goal is **not** merely to make the UI return sooner while hiding the same work in the background. The goal is to remove total-history-linear work from hot paths and make unavoidable work proportional to the current continuation/model context, not the entire historical transcript/archive.

## Current send path

For a normal existing session, the frontend does roughly:

```text
User presses Send
      │
      ▼
submitComposer()
      │
      ▼
queueUserInput(text)
      │
      ▼
api.queueFollowUp(...)
      │
      ▼
wait for backend response  ◀── send button spins here
      │
      ▼
update UI/cache
      │
      ▼
refresh transcript in background
```

The frontend only does extra pre-flight work for edge cases such as archived sessions or a selected session snapshot that is not yet loaded. For a normal idle selected session, the frontend is primarily waiting for `input.follow_up`.

The daemon `input.queue` path currently does roughly:

```text
input.queue(session_id, message)
      │
      ▼
Acquire per-session driver lock
      │
      ▼
recover_if_needed()
      │
      ▼
Check unfinished actions / queued inputs
      │
      ├───────────────────────────────────────┐
      │                                       │
      ▼                                       ▼
Already running/queued                 Idle, nothing queued
      │                                       │
      ▼                                       ▼
Insert queued input                     ensure_expected_active_leaf()
      │                                       │
      ▼                                       ▼
Maybe drive if needed                   ensure_active_loaded()
      │                                       │
      ▼                                       ▼
Return queue projection                 load_stored_session()
                                              │
                                              ▼
                                      AgentSession::from_stored_session(...)
                                              │
                                              ▼
                                      enqueue input into runtime
                                              │
                                              ▼
                                      persist accepted input/outputs
                                              │
                                              ▼
                                      sync active branch response
                                              │
                                              ▼
                                      dispatch model/tool work
                                              │
                                              ▼
                                      return to frontend
```

The suspicious long-session cost is on the idle/cold branch:

```text
ensure_active_loaded()
      │
      ▼
load session config
      │
      ▼
ensure workspace exists
      │
      ▼
load_stored_session(session_id)
      │
      ▼
StoredSession { entries: Vec<all transcript entries + provider replay> }
      │
      ▼
AgentSession::from_stored_session(stored)
      │
      ▼
put reconstructed runtime in memory
```

This makes cold session wakeup cost scale with total persisted history and provider replay data.

## Current implementation details that the design must replace

The target architecture needs to account for several existing-code realities:

- The current RPC is `input.follow_up`; this document's `input.queue` language means "durably accept an input command".
- `queued_inputs` has statuses such as `queued`, `consuming`, and `consumed`, and reset helpers exist, but the current `take_next_queued_input` path is not a full durable claim/lease state machine. It can rely on the in-memory per-session driver lock and optimistic row checks. A worker-based architecture must add true durable claim semantics.
- Persisted model actions currently store a `context_leaf_id` pointer. On restart, pending action dispatch can reconstruct model context by walking transcript entries to that leaf. A context manifest must replace that pending-action path, not only warm runtime dispatch.
- Model/tool completion handlers currently require a warm active `AgentSession`. In the target design, completions need to become durable commands fenced by `action_row_id`/`attempt_id`, then applied by a session worker after checkpoint resume.
- Some non-send paths still load all transcript history, including history/debug endpoints, turn resume, and manual/boundary compaction. They must be classified as explicit heavy operations or migrated to scoped queries/manifests.
- `workspaces.ensure_session(...)` can also be slow. The normal send transaction should not synchronously ensure or hydrate workspaces; the worker should ensure workspaces before execution.

## Reconciliation with PRs #205 and #206

This plan should be reconciled with two near-term delegation PRs:

- **#205: Allow steering read-only delegation subagents.**
  - Read-only means disposable workspace isolation, not an immutable conversation.
  - Running read-only subagents can accept steer-priority input.
  - Steer eligibility and delegation terminality become work-aware: queued input and unfinished actions can keep a subagent non-terminal even when its active leaf is at a turn boundary.
- **#206: Support partial delegation fanout wakeups.**
  - Parent sessions receive deterministic daemon-authored wakeup observations one terminal child at a time before the final all-subagent barrier.
  - After a partial wakeup, the parent can steer still-running siblings or cancel the delegation.
  - Final completion cancels stale queued partial wakeups and publishes the terminal delegation wakeup.
  - Boot/lifecycle repair must handle expected spawn counts, stale partials, final completion, cancellation, and subsequent terminal siblings.

These PRs are not alternatives to this architecture plan. They are nearer-term correctness/product changes that make the durable-command architecture more important:

```text
#205/#206:
  broaden what can be steered
  increase use of queued inputs as durable commands
  make delegation progress depend on queue/action state
  add internal parent wakeup observations as queued work

this plan:
  makes those command paths cheap, checkpointed, durable, and worker-driven
```

### Recommended PR sequencing

The safest merge order is:

1. **#205 first** as the foundational steerability/work-aware-terminality change.
2. **#206 rebased on #205** so partial wakeups reuse the same steer eligibility and terminality helpers instead of duplicating or diverging from them.
3. **This architecture plan/docs PR after #205/#206**, or at least rebased to name their final semantics accurately.

When rebasing #206 onto #205, any partial-wakeup candidate scan that checks only `active_leaf_is_turn_boundary` must be replaced with the shared work-aware child execution-terminal predicate. Otherwise a child with a terminal-looking leaf plus queued/consuming steer work or unfinished actions could incorrectly publish a partial "finished child" wakeup.

If #206 is merged first, #205 should be rebased and trimmed to avoid reintroducing duplicate read-only-steering changes. One of the two PRs should become the canonical home for:

- read-only subagent steer eligibility,
- shared model-facing and websocket steer guards,
- work-aware subagent terminality/status,
- `steerable` projection semantics.

### Architectural implications from #205/#206

The target architecture should treat every input-like operation uniformly:

```text
top-level follow-up
subagent steer
daemon wakeup observation
delegation partial/final completion observation
```

All of these should be durable commands with idempotency, branch/attempt intent, queue/action fencing, and session-work scheduling. The plan should not optimize only normal user follow-ups.

Work-aware terminality becomes a first-class invariant, but the design should distinguish child execution terminality from delegation publication quiescence:

```text
child execution-terminal != active_leaf_is_turn_boundary

child execution-terminal requires:
  active branch is at a terminal boundary when applicable
  no active queued/consuming inputs for that session
  no unfinished actions

delegation publication quiescent requires:
  pending partial/final wakeup commands are resolved or fenced
  stale partial wakeups for the delegation attempt are cancelled/obsolete
  boot/live repair work cannot publish another observation for the same terminal fact
```

This matters for:

- delegation progress counts,
- `steerable` projections,
- read-only snapshot cleanup,
- final-message/transcript artifact exposure,
- cancellation/archive/delete guards,
- parent wakeup publication.

Partial wakeup candidate selection must use the same work-aware child execution-terminal predicate as delegation progress/final completion. A boundary leaf by itself is not sufficient; a child with a terminal-looking active leaf plus a queued/consuming steer or unfinished action must not publish a "finished child" partial wakeup.

Partial delegation wakeups should be modeled as internal durable commands, not as special synchronous parent turns:

```text
subagent reaches terminal
      │
      ▼
delegation worker/barrier evaluates state
      │
      ▼
enqueue deterministic parent partial wakeup observation
      │
      ▼
schedule parent session_work
      │
      ▼
parent consumes wakeup
      │
      ▼
after the parent durably consumes the previous partial and reaches the chosen decision point,
the delegation worker may publish the next partial
```

The "one partial at a time" property from #206 should remain explicit in the worker design. It prevents the parent from consuming stale prepublished partial wakeups after deciding to steer/cancel remaining siblings. If every subagent is already terminal when the delegation worker evaluates the delegation, the worker may go straight to final completion rather than publishing partials first; partial wakeups are for the still-running fan-out case.

The exact parent decision point must be specified before implementation. Two viable policies are:

- enqueue the next partial after the previous partial is durably consumed into the parent transcript, while keeping it cancellable before parent consumption; or
- wait until the parent reaches a turn boundary/idle state after processing the previous partial.

Either policy must preserve the user's opportunity to steer/cancel between partials.

Near-term #206 effectively uses the first policy: after the parent durably consumes/handles a partial wakeup command, the runner may publish the next already-terminal sibling. The worker architecture may keep that policy or choose the stricter turn-boundary/idle policy, but it must make the choice explicit and must not prepublish multiple sibling partials ahead of the parent's opportunity to steer or cancel.

Internal daemon observations need slightly different branch/intent policy than user messages:

- deterministic `client_input_id`/attempt id is mandatory;
- queued rows should record a strong daemon/internal type or `origin='daemon'` metadata, not merely priority;
- daemon wakeups should be distinguishable from user `Steer`/follow-up inputs in ordering, editability, branch-conflict policy, stale cancellation, and UI display;
- cancellation/final-completion logic must obsolete stale partial wakeups;
- branch conflicts should not silently drop observations, but daemon observations may use a different conflict policy than user follow-ups;
- parent wakeups should remain exactly-once across live races and boot repair.

Current #206-style partial/final delegation wakeups are represented as deterministic steer-priority queued inputs carrying daemon tool-observation content. That is acceptable as a near-term bridge, but the target durable-command schema should not rely on `priority='steer'` alone to identify daemon work. Rows should carry an explicit daemon/internal command type or `origin='daemon'` metadata, plus delegation id, attempt id, wakeup kind, and subagent id when applicable.

Final completion or cancellation must obsolete any not-yet-applied partial wakeup commands for the delegation attempt, including `queued` rows and `consuming` rows whose parent-transcript application has not committed. Already-consumed transcript observations may remain as historical facts; expired consuming leases must not resurrect stale partials after terminal completion/cancellation.

Read-only workspace lifecycle must also become work-aware, but publication should not depend on the disposable workspace. A read-only subagent snapshot can be disposable, and partial/final wakeup publication plus transcript/final-message handoff rendering should use durable transcript/read-model state and tolerate the workspace already being gone. Cleanup should be blocked only by execution work or artifact capture that actually needs the filesystem, such as queued steers, unfinished actions/tools, or workspace-backed artifact reads. Conversely, a missing workspace after confirmed terminal cleanup should be treated as expected terminal cleanup, not necessarily as runtime corruption.

Artifact lifecycle should be defined by durable state, not by disposable workspace presence:

| Artifact/ref kind | When it may be exposed | After cancellation | After read-only workspace cleanup |
| --- | --- | --- | --- |
| Partial terminal-child final-message/transcript refs | Once captured from durable transcript/read-model state for that terminal child. | May remain readable as historical facts if already published. | Should remain readable if backed by durable handoff/read-model state. |
| Final delegation handoff | Only after final completion barrier (`done` / `done_with_failures`). | Not newly created by cancellation. | Should remain readable without the child workspace. |
| Cancellation transcript-only files | On cancellation path for affected subagents. | Expected cancellation artifact. | Should be rendered from durable transcript/read-model state. |
| Workspace-backed artifact reads | Only while filesystem content is still intentionally retained. | May be unavailable after cleanup unless captured first. | Missing workspace after confirmed cleanup is expected, not corruption. |

## Design goals

1. **No total-history replay in user-facing hot paths.**
   - `input.queue`, `session.get`, `events.subscribe`, and lightweight branch sync should not reconstruct `AgentSession` from all transcript entries.

2. **Durable commands first.**
   - Sending a message should durably record intent and schedule work. It should not require reconstructing runtime state before the input is accepted.

3. **Persist runtime continuation.**
   - The daemon should resume from a compact continuation/checkpoint that contains only the state needed to continue execution.

4. **Separate archive, read models, and runtime state.**
   - Full transcript history is an audit/history/archive data source.
   - UI list/detail views should use materialized read models or scoped queries.
   - Runtime continuation should be small and incrementally maintained.

5. **Provider dispatch work should scale with current model context, not total history.**
   - If the current context is huge, sending that context to a provider is inherently expensive. But old side branches, detailed UI bodies, and provider replay archive should not be loaded simply to continue.

6. **Crash-safe and idempotent.**
   - Every accepted input should survive daemon crashes.
   - Background drive should be retryable and single-flight per session.

## Target architecture

Split the system into three layers:

```text
┌─────────────────────────────────────────────┐
│ Full transcript archive                      │
│ - audit/history/details                      │
│ - can be large                               │
│ - not needed for every send                  │
└─────────────────────────────────────────────┘

┌─────────────────────────────────────────────┐
│ Materialized read models / scoped queries    │
│ - session list                               │
│ - session snapshot                           │
│ - active branch / turn cards                 │
│ - queue state                                │
│ - delegation/subagent status                 │
└─────────────────────────────────────────────┘

┌─────────────────────────────────────────────┐
│ Runtime continuation                         │
│ - compact resumable state                    │
│ - active leaf / state-machine continuation   │
│ - current action/turn state                  │
│ - context manifest pointer                   │
└─────────────────────────────────────────────┘
```

The key change:

```text
Current:
  Runtime state is reconstructed from transcript history.

Target:
  Runtime state is persisted incrementally as a continuation.
  Transcript history is an archive/read model, not the mechanism used to resume execution.
```

Or shorter:

```text
Do not replay history to know how to continue.
Persist how to continue.
```

## Target send path

`input.follow_up` / `input.queue` should become a cheap durable command transaction:

```text
begin transaction
  lock sessions row by primary key
  reject deleted/archived sessions unless explicitly unarchiving
  validate expected_active_leaf_id against sessions.active_leaf_id
  resolve idempotency by (session_id, client_input_id)
  insert queued_inputs row with branch intent and status='queued'
  insert input_queued event
  upsert session_work(needs_drive=true, wake_reason='input_queued')
  bump queue/session revision
commit
notify scheduler
return accepted command response + queue projection
```

Properties:

- No `load_stored_session`.
- No `AgentSession::from_stored_session`.
- No full active-branch sync required before returning.
- No model/tool dispatch before returning.
- Work is bounded by indexed session/queue/event operations.

This is not merely a UI trick. It changes the core command boundary: the accepted durable input is the source of truth, and a session worker consumes it.

### Observable API behavior change

Today, an idle `input.follow_up` can return:

```json
{ "accepted": true, "queued": false, "active_branch_sync": "..." }
```

because it immediately mutates the active in-memory runtime, persists new transcript/actions, syncs the active branch, and dispatches work before returning.

With the durable-command boundary, idle sends should instead return an accepted/queued command response. Consequences:

- The user-message transcript row may arrive later through events / branch sync / transcript refresh.
- The queue projection should honestly show the message as queued/accepted until the worker consumes it.
- Frontend state should distinguish "durably accepted" from "already consumed into transcript".
- Existing tests/docs expecting `queued=false` for idle sends must change.
- This should still be presented honestly to users; it is not a fake success state.

## Target drive path

A session worker/actor, not the RPC handler, owns runtime recovery and execution:

```text
Session worker wakes
      │
      ▼
claim session lease / single-flight session driver
      │
      ▼
load runtime checkpoint
      │
      ├─ valid checkpoint: reconstruct compact runtime continuation
      │
      └─ missing/old checkpoint: one-time migration fallback from transcript, then write checkpoint
      │
      ▼
consume queued input
      │
      ▼
apply state machine
      │
      ▼
persist transcript entries/actions/events
      │
      ▼
persist new runtime checkpoint/context manifest
      │
      ▼
dispatch model/tool work
```

The worker should be crash-safe and idempotent. If it crashes after input acceptance, boot recovery or a scheduler sweep should find queued inputs / unfinished actions / `session_work.needs_drive=true` and retry.

## Queued input claim/consume state machine

Workers must claim queued inputs durably rather than relying only on the in-memory session driver lock.

Target state machine:

```text
queued
  ├─ claim by worker ─────────────► consuming(claim_id, lease_owner, lease_until)
  │                                      │
  │                                      ├─ output persistence succeeds ─► consumed
  │                                      ├─ branch conflict ─────────────► conflicted / requires_user_action
  │                                      ├─ system obsoletes command ────► cancelled / obsolete
  │                                      └─ lease expires/crash ─────────► queued
  └─ user/system cancellation ─────► cancelled
```

Claiming should be atomic and revision-fenced:

```text
begin transaction
  claim session_work lease for session
  select next queued input for update skip locked
  update queued_inputs
    set status='consuming',
        claim_id=$claim_id,
        lease_owner=$worker_id,
        lease_until=$deadline
  return claimed row
commit
```

Consumption should only succeed if the worker owns the current claim id or row version. Cancellation/obsoletion from `consuming` must also be fenced by `claim_id` or row version. A worker that tries to commit a consumed result for a cancelled or obsolete command must fail the commit and reload rather than resurrecting the command. This is especially important for stale partial delegation wakeups: expired consuming leases must not reset obsolete partials back to `queued` after final completion or cancellation. Crash recovery should reset only non-obsolete expired `consuming` rows. If a `conflicted` status is not added, the alternative conflict behavior must be stated explicitly before implementation.

## Runtime checkpoints

Introduce a compact persisted runtime continuation, for example:

```text
session_runtime_checkpoints
  session_id primary key
  active_leaf_id
  runtime_version
  transcript_revision
  queue_revision
  action_revision
  checkpoint_json / checkpoint_blob
  created_at
  updated_at
```

The checkpoint should contain only data required to continue the state machine, such as:

```text
Runtime continuation
  ├─ active_leaf_id
  ├─ checkpoint_revision / runtime_version
  ├─ core loop state: current turn id, next action id, readiness/blocking state
  ├─ turn/action state
  ├─ readiness / blocked state
  ├─ outstanding action keys needed to accept/reject completions
  ├─ provider continuation IDs or response IDs if required
  ├─ context manifest pointer
  ├─ pending outbox state only if not already durably represented
  └─ compact state-machine data
```

It should not contain:

```text
all transcript entries
all side branches
all UI detail bodies
all provider replay archive
TranscriptStore.entries_by_id / insertion_order / full branch tree
```

This is a hard design constraint. Serializing the existing `AgentSession` wholesale would likely serialize its full `TranscriptStore` and fail the goal. `AgentSession` needs a resumable state representation separate from transcript archive/read-model data.

Checkpoint and context-manifest updates must be atomic with output persistence, or explicitly revision-fenced if stored as a repairable cache.

Preferred transactional shape:

```text
persist_outputs_tx includes:
  transcript entries
  sessions.active_leaf_id
  action updates/new actions
  queued input consumed/accepted status
  runtime checkpoint upsert
  context manifest update
  revision bumps
```

The transition should fence on:

- expected active leaf id,
- expected checkpoint revision,
- transcript revision / queue revision as needed,
- action attempt ids for completions.

If a checkpoint is written outside the output transaction, it must be treated as a cache. On revision mismatch, the worker must repair/rebuild it rather than trusting it.

Warm in-memory active runtimes must carry the same revision identity. A warm runtime should only persist outputs/checkpoints if its base checkpoint/revision still matches durable state; otherwise it must reload or fail safely.

`ensure_active_loaded()` should eventually prefer:

```text
checkpoint -> AgentSession::from_checkpoint(...)
```

and only fall back to:

```text
load_stored_session -> AgentSession::from_stored_session(...)
```

for old sessions, missing checkpoints, corruption, or explicit repair/migration.

## Model context manifests

Provider dispatch needs the current model context, but it should not derive that by loading all history on every turn.

Maintain a context manifest/read model, such as:

```text
session_context_manifests
  session_id
  active_leaf_id
  context_revision
  token_estimate
  created_at
  updated_at

session_context_entries
  session_id
  context_revision
  ordinal
  transcript_entry_id
  compacted_summary_id
  provider_replay_ref
  token_count
```

Then provider dispatch does:

```text
load current context manifest
      │
      ▼
load only entries referenced by the current context
      │
      ▼
serialize provider request
```

The acceptable complexity is:

```text
O(current model context)
```

not:

```text
O(total session history + all side branches + all provider replay archive)
```

If a session has an un-compacted enormous active context, the provider request itself may still be expensive. That is unavoidable unless compaction/provider-native continuation reduces the current context. But cold-start should not pay for unrelated history.

The context-manifest phase must update every model-context consumer, not only fresh dispatch from a warm runtime. In particular:

- live `SessionAction::RequestModel`,
- pending model actions after daemon restart,
- action dispatch paths that currently store only `context_leaf_id`,
- compaction token gates,
- manual/auto compaction context construction,
- history/debug/provider-context endpoints if kept.

Target pending-action replacement:

```text
current:
  action.payload.context_leaf_id -> model_context_for_leaf() -> walk transcript branch

target:
  action.payload.context_manifest_revision/id -> load manifest entries only
```

Provider replay archive should stay separate from provider request content. The manifest should include provider replay references only when required for provider-native continuation/debug/retry, not by default for every model-context load.

## Read APIs should not synchronously recover/drive

Read-ish endpoints should return durable read state and schedule recovery if needed. They should not acquire the session driver and reconstruct runtime just because the UI polls.

Current problematic shape:

```text
session.get / events.subscribe / sync_active_branch
      │
      ▼
acquire SessionDriver
      │
      ▼
recover_if_needed()
      │
      ▼
maybe load/rebuild long session
      │
      ▼
return data
```

Target shape:

```text
session.get / events.subscribe / sync_active_branch
      │
      ▼
read durable snapshot/read model/scoped data
      │
      ├─ if stale or recovery-needed: schedule session_work(needs_drive=true)
      │
      ▼
return data
```

This is especially important with proactive frontend polling. Polling should not trigger expensive runtime reconstruction or contend with send.

Read endpoints should be classified by intended cost:

| Endpoint / operation | Target behavior |
| --- | --- |
| `session.get` | Read snapshot/read model only; schedule recovery if needed. |
| `events.subscribe` | Subscribe/replay stored events only; schedule recovery if needed. |
| `session.sync_active_branch` | Branch-scoped query only; no runtime recovery. |
| `transcript.turns` / `turn_detail` | Bounded active-branch queries only; no runtime recovery. |
| `history.tree` | Explicit full-tree/debug UI; may be O(history), never proactive. |
| `history.context` | Explicit debug/provider-context endpoint; use context manifest or label as heavy. |
| `turn.resume` / `history.switch` | Mutations requiring idle; use durable state checks and scheduling, not full replay in normal send path. |
| `compaction.request` | Use current context manifest/branch query; avoid full transcript tree load. |

Some endpoints may remain intentionally heavy, but they should be labeled and avoided by proactive polling/background warming.

## Branch intent and correctness

If follow-up inputs are always durably queued first, the queue rows need enough branch intent to remain correct even if active history changes before consumption.

Queued input rows should record fields like:

```text
queued_inputs
  id
  session_id
  priority
  content
  client_input_id
  expected_active_leaf_id
  base_leaf_id
  target_leaf_id / target_branch_revision
  status
  created_at
```

The enqueue transaction should validate cheap invariants using durable DB state:

```text
begin
  lock session row
  compare sessions.active_leaf_id to expected_active_leaf_id when provided
  insert queued input with branch intent
  insert event
  upsert session_work
commit
```

The worker should re-check branch intent before consuming:

```text
take queued input
      │
      ▼
current active leaf still matches input target?
      │
      ├─ yes: apply input
      └─ no: mark conflict / rebase / attach to original branch / require user resolution
```

The exact policy needs product/design choice, but it must be explicit. Silent consumption onto the wrong branch would be bad.

Policy details to decide explicitly:

- Normal follow-ups should probably attach to the active leaf observed at enqueue time.
- If the active leaf changes before consumption, mark the input `conflicted` or keep it queued with conflict metadata rather than silently applying it elsewhere.
- Steer inputs may need different rules because they target an active running turn, not a terminal branch.
- Internal daemon observations, such as parent partial/final delegation wakeup observations, need deterministic idempotency and a separate internal policy. They should not be user-editable and should carry enough delegation attempt/subagent identity to support stale-partial cancellation and boot repair.
- `history.switch` and `turn.resume` should either remain prohibited while relevant queued inputs exist, or define how queued branch-intent rows are updated/conflicted.

## Completion-as-command flow

Model and tool completions should not fail merely because the session runtime is no longer warm in memory. Completions should be accepted durably and applied by a worker after checkpoint resume.

Target flow:

```text
provider/tool finishes
      │
      ▼
begin transaction
  validate action_row_id / attempt_id is still running
  record completion command/result or mark action completion-pending
  upsert session_work(needs_drive=true, wake_reason='action_completed')
commit
notify scheduler
      │
      ▼
worker resumes checkpoint
      │
      ▼
worker applies completion to runtime state
      │
      ▼
worker persists transcript outputs/actions/checkpoint
```

Attempt fencing remains required. Stale provider/tool completions should be ignored or recorded as stale according to existing action semantics.

## Session work scheduling

Add a durable scheduling primitive so RPC handlers do not have to drive sessions synchronously:

```text
session_work
  session_id primary key
  needs_drive boolean
  wake_reason text
  lease_owner text
  lease_until timestamptz
  retry_count integer
  next_retry_at timestamptz
  updated_at timestamptz
```

`input.queue` and other durable command paths would upsert `needs_drive=true`. A scheduler/worker loop would claim leases and run the session driver.

The scheduler design should specify:

- lease owner identity and lease expiration,
- retry/backoff policy,
- maximum concurrent sessions,
- fairness across projects and subagents,
- wake notification mechanism after commit,
- boot sweep query,
- predicates that set `needs_drive`,
- whether workers are per-session actors or a global worker pool,
- event publication after commit.

Crash recovery:

```text
on daemon boot
  │
  ▼
find sessions with:
  - session_work.needs_drive=true
  - active queued/consuming inputs
  - unfinished actions
  - stale leases
  │
  ▼
schedule workers
```

This makes driving retryable and removes the requirement that request handlers repair or continue execution inline.

Boot recovery should move away from blindly marking all unfinished actions stale. With durable leases, boot should:

- expire worker/session leases,
- reset expired `consuming` inputs,
- requeue pending actions that were not externally running,
- mark only non-resumable running attempts stale/interrupted according to action kind,
- schedule `session_work` for sessions with queued inputs, unfinished actions, stale checkpoints, or recovery-needed leaves.

## Migration plan

### Phase 1: Instrumentation

Add detailed timings around `input.queue` and runtime loading:

```text
input.queue total
  ├─ acquire driver lock
  ├─ recover_if_needed
  ├─ has_unfinished_actions
  ├─ has_queued_inputs
  ├─ ensure_expected_active_leaf
  ├─ ensure_active_loaded
  │    ├─ load_session_config
  │    ├─ ensure_workspace
  │    ├─ load_stored_session
  │    └─ AgentSession::from_stored_session
  ├─ enqueue runtime input
  ├─ persist_active_outputs
  ├─ sync_active_branch
  └─ dispatch
```

Also instrument read endpoints that currently call `recover_if_needed()`. This proves whether slow sends are caused by lock contention, runtime reconstruction, branch sync, dispatch, or another step.

### Phase 2: Side-effect-light read APIs

Change read APIs to avoid synchronous recovery/drive where possible. If they observe stale/recovery-needed durable state, they should schedule `session_work` and return the latest durable read model.

This protects proactive polling from becoming a hidden runtime wakeup trigger.

### Phase 3: Always durable-queue user inputs

Make `input.queue` use the durable queued-input path for idle sessions too. Preserve idempotency through `client_input_id` and validate expected active leaf in the DB transaction.

This phase creates the right command boundary, but by itself it does not remove all linear work. The worker may still reconstruct long sessions until later phases land.

This phase should cover all input-like commands, not only top-level follow-ups:

- top-level user follow-ups,
- subagent steers, including read-only subagents from #205,
- daemon-authored delegation partial/final wakeup observations from #206,
- future daemon-authored observations.

### Phase 4: Durable claims and completion commands

Add true queued-input claim/lease semantics and make provider/tool completions durable commands. This phase makes background workers safe across crashes, retries, and daemon restarts.

Durable claims must preserve #206's stale partial-wakeup cancellation semantics and #205's work-aware terminality. A queued or consuming steer/wakeup is real work and should prevent premature delegation completion or read-only snapshot cleanup.

### Phase 5: Runtime checkpoints

Persist a compact runtime checkpoint after each successful output batch. Make runtime loading prefer checkpoints and only fall back to full transcript reconstruction for old/missing/corrupt checkpoint cases.

### Phase 6: Split `AgentSession` state from full transcript archive

Ensure the checkpoint does not simply serialize the full transcript into another blob. `AgentSession` should own execution continuation, while transcript storage/history lives in the store/read model.

### Phase 7: Materialized current context

Maintain a current model-context manifest incrementally. Provider dispatch loads this manifest and its referenced entries, not the full transcript tree. Update all current context consumers, including pending action redispatch after restart and compaction paths.

## Edge cases to design explicitly

1. **Branch changes between enqueue and consumption.**
   - A queued input should not silently apply to a different active leaf than the user saw.

2. **Duplicate sends / retries.**
   - `client_input_id` idempotency must work whether the input is queued, consuming, consumed, failed, or conflicted.

3. **Daemon crash while consuming input.**
   - `consuming` rows need durable lease/claim reset semantics. Existing helpers are not enough by themselves; workers need atomic claim/consume state.

4. **Checkpoint corruption or version mismatch.**
   - Fallback to transcript replay should be available as repair/migration, but should be observable and not silently happen on every hot path.

5. **Provider continuation IDs.**
   - Some providers may support continuation/responses APIs. The checkpoint/context design must retain enough provider-specific continuation metadata without forcing provider replay archive loads.

6. **Tool/action recovery.**
   - Pending actions should be represented durably so recovery can inspect indexed action rows rather than reconstructing transcript history.

7. **Compaction boundaries.**
   - Compaction should update both transcript/read models and runtime/context continuation. Boundary compaction should not invalidate queued input branch intent unexpectedly.

8. **Subagents and delegation wakeups.**
   - Parent wakeup observations are durable queued inputs today. The new command/worker boundary should preserve exactly-once wakeup semantics and delegation completion behavior.
   - Delegation barrier side effects and read-only workspace cleanup should not rely on inline `recover_if_needed()` calls from reads/sends.
   - Partial wakeups from #206 should remain one-at-a-time and stale-partial-aware while the delegation remains running. The parent must get a chance to steer/cancel before subsequent terminal sibling wakeups are consumed.
   - Partial wakeup candidate selection must use work-aware child execution terminality, not `active_leaf_is_turn_boundary` alone.
   - Final completion/cancellation must obsolete not-yet-applied partial wakeup commands, including future `consuming` rows whose lease could otherwise expire and reset to `queued`.
   - Read-only subagent steering from #205 means queued steer inputs and unfinished actions keep read-only subagents execution-nonterminal for progress/barrier purposes.
   - Already-published partial/final observations and terminal-child final-message refs may remain historical facts after cancellation; the doc/API should define which artifact refs stay readable.

9. **Archived sessions.**
   - Resume/unarchive policy should remain explicit and cheap. Archived sessions may still require validation before accepting a follow-up.

10. **Read model freshness.**
    - If read APIs no longer recover inline, UI snapshots need a way to display stale/recovering/needs-drive state honestly.

11. **Scheduler fairness and backpressure.**
    - Durable work scheduling needs concurrency limits and fairness across sessions/projects/subagents.

12. **Memory cache invalidation.**
    - In-memory active sessions and persisted checkpoints need clear revision checks so stale active runtimes cannot overwrite newer DB state.

13. **Manual and boundary compaction.**
    - Compaction currently may require full-history context construction. It should become a context-manifest consumer or be labeled as an explicit heavy operation.

14. **Provider replay archive separation.**
    - Runtime continuation and model context should not load full provider replay audit blobs unless provider-native continuation/debug/retry explicitly needs them.

15. **Workspace setup.**
    - Send should not synchronously ensure workspaces. Workspace setup belongs in the worker before execution.

16. **Boot recovery semantics.**
    - Durable worker leases and action attempts should replace broad stale marking where possible.

17. **History switch/resume with queued inputs.**
    - Switching active leaf while queued inputs exist must be prohibited or produce explicit conflicts/rebinding behavior.

## Observability and guardrails

Add metrics/logs for:

- `input.follow_up` total latency and DB transaction latency,
- scheduler wake latency from queued-input commit to worker claim,
- queued-input claim latency and expired consuming lease resets,
- checkpoint load latency, size, version, and fallback reason,
- fallback full transcript replay count and reason,
- context manifest rebuild count/latency,
- queued-input conflict count,
- stale active runtime checkpoint-write rejections,
- pending action redispatch/recovery count,
- read endpoint calls that schedule recovery instead of recovering inline,
- explicit heavy endpoint usage and duration.

## Non-goals / insufficient fixes

### Returning before drive only

Returning after enqueue is necessary for a better command boundary, but insufficient if the worker still reconstructs all history every time.

### Keeping sessions warm in memory only

A warm cache helps until daemon restart, memory pressure, cleanup, or a cold subagent. It does not solve cold-start linear work.

### Adding indexes only

Indexes are important for queue/event/action/read-model queries. They do not fix JSON deserialization or `AgentSession` reconstruction from a large `Vec<StoredTranscriptEntry>`.

### More aggressive compaction only

Compaction helps bound current model context, but basic send performance should not require the whole historical archive to remain small.

## Open questions

1. What is the minimal `AgentSession` continuation state needed to resume without full transcript history?
2. Can current provider runtimes be adapted to consume a context manifest directly?
3. Should queued follow-ups attach to the active leaf at enqueue time, or to a branch revision abstraction?
4. How should conflicting queued inputs surface in the UI?
5. Which read APIs can safely stop calling `recover_if_needed()` first, and which require stronger consistency?
6. How should checkpoints be encrypted/serialized/versioned if they contain provider-specific continuation data?
7. What metrics should alert when fallback full transcript reconstruction occurs?
8. Can compaction produce/update context manifests incrementally enough to avoid large rebuilds?
9. Should queued-input conflicts get a new `conflicted` status, or be represented as queued rows with conflict metadata?
10. Which action kinds are resumable after boot and which should be marked stale/interrupted?
11. Should provider/tool completions be stored as separate command rows or as action status/result transitions?
12. What UI affordance should show "durably accepted but not yet consumed into transcript"?

## Summary

The desired architecture is command-driven and checkpointed:

```text
Send path:
  durable input command + schedule work

Drive path:
  resume compact continuation + consume command + persist next continuation

Read path:
  durable read models/scoped queries, no runtime recovery

Provider path:
  current context manifest, not full historical transcript replay
```

This removes total-history-linear runtime reconstruction from hot paths while preserving correctness, crash safety, and observability.
