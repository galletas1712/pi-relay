# Minimal workflow orchestration plan

Status: proposed. Last reviewed 2026-06-16 (rev: one durable workspace written
in place; the single full subagent writes the parent's dirs directly, read-only
subagents fan out into disposable btrfs snapshots that are destroyed on return
and hand back only their final message; no merge, no adoption, no rollback in
v1; no parallel writers; subagents are non-recursive; retry == resume the
existing session).

## Summary

Keep the architecture small. Earlier drafts swung between heavy workspace
isolation (forks + git merge + multi-parent lineage) and a fully shared
filesystem (which needed hard-to-enforce read-only sandboxing). This revision is
the simplest synthesis:

- There is **one durable workspace** — the parent's. It always has **at most one
  writer**: either the parent, or the single **full** subagent of a full stage.
  They are never concurrent, because the parent parks while a stage runs. The
  full subagent writes the parent's directories **in place**; its edits are
  simply the parent's state when it finishes. Nothing is merged or adopted.
- **read-only** subagents never touch the durable workspace. Each runs in its own
  **disposable btrfs snapshot** (reflink/copy fallback on non-btrfs hosts —
  machinery that already exists in `instantiate.rs`). It may write inside that
  snapshot (so it can build, run, and test), but the snapshot is **destroyed
  when the subagent returns**. Its only output is its **final message**.
- Work happens in **stages**. A stage is either *one full subagent* or *a
  parallel fan-out of read-only subagents* — never both, and never multiple full
  (writing) subagents.
- The parent **does not wait or poll**. After launching a stage it ends its turn
  and parks; the daemon notifies it when the stage completes (the "background
  agents" pattern). The parent then decides what to do next.
- A **workflow** is a named template that *suggests* a sequence of stages. The
  parent has full discretion over which stage to run, whether to re-run one, and
  when to stop.

### The two clean roles

| | read-only (ephemeral) | full (durable) |
| --- | --- | --- |
| Workspace | own disposable snapshot | the parent's dirs, in place |
| May write? | yes, but only inside its snapshot | yes, the real workspace |
| On return | snapshot destroyed | edits persist as the parent's state |
| Handoff | final message only (text) | in-place file changes + final message |
| Steerable/interruptible? | no (fire-and-forget) | yes |
| How many per stage | many, in parallel | exactly one |

### What makes this safe and simple

> The one durable workspace has a single writer at any instant (parent or the one
> full subagent), guaranteed by parking. Read-only subagents are isolated in
> throwaway snapshots, so any number can run in parallel without touching the
> durable workspace or each other. There is no merge, no adoption, and no
> conflict resolution anywhere in the system.

### What this keeps, deletes, and accepts

Keeps (reusing existing code): btrfs/reflink/copy workspace forking
(`instantiate.rs`) for read-only snapshots; subagent sessions and
`sessions.parent_session_id`; subagent steer/interrupt and lifecycle events;
session resume/repair for retries.

Deletes: daemon-side `git merge`, cross-task git source refs, multi-parent
lineage; snapshot **adoption**/swap; the `artifacts` table; the
`sources`/`from_task` workspace modes; the pure `advance()` workflow state
machine as the sole owner of control flow; workflow variables, transition
proposals, budgets, leases, graph DSL; `parallel_race` and any parallel-writer
pattern.

Accepts as tradeoffs:

- **No parallel writers.** Only one full (writing) subagent at a time. Parallel
  work is read-only only. The substitute for "try several implementations" is
  parallel read-only exploration feeding one full implementation stage.
- **No rollback in v1.** The full subagent writes in place, so a botched full
  stage is recovered with git or by steering/repairing the subagent, not by an
  automatic undo. A future version could snapshot the durable workspace before a
  full stage to allow discarding its changes; this is deliberately deferred (see
  Snapshots).
- **read-only subagents cannot return files.** Their snapshot (including anything
  they built or wrote) is destroyed on return, so durable file output is a
  full-subagent capability only. read-only subagents pack everything the parent
  needs into their final message.

## The three invariants

1. **A stage is homogeneous.** One full subagent, or a fan-out of read-only
   subagents — never a mix, and never more than one full subagent. This is what
   keeps the durable workspace single-writer.
2. **The parent never busy-waits.** After launching a stage the parent ends its
   turn and parks (idle). The daemon delivers a completion notification as new
   input when the stage finishes. Parking is what guarantees the parent and the
   full subagent never write the durable workspace at the same time, and it gives
   the background-agents UX.
3. **Workflows are stages, but the parent has discretion.** A workflow is an
   ordered list of suggested stages. The parent decides which to run, whether to
   re-run, and when the run is done. The daemon supplies mechanism (typed
   subagents, snapshots, homogeneity, notifications, durable stage records); the
   parent supplies policy.

## Relationship to the stated architecture

This plan reverses a documented non-goal. `architecture.md` lists "Do not
include subagent orchestration" (Goal 6) and names the removed
`agent-orchestrator` crate under "Removed Pieces". That guidance predates durable
subagent sessions, parent links, and lifecycle events, which have shipped. Before
Phase 1, `architecture.md` must be updated in the same change so the docs and
this plan agree.

### This is the third orchestration attempt; do not repeat the first two

1. **`agent-orchestrator` crate + `SessionRegistry` (removed).** Control flow
   lived in a process-local object graph. *Lesson: durable Postgres state is the
   source of truth.*
2. **`workflow_variables` + `work.*` RPCs + Python workflow SDK (deleted).**
   Control flow polled named variables and lived in editable Python templates.
   *Lesson: no general variable store, no model-authored control-flow scripts.*

A third lesson from the current Python REPL `subagents.*` API: the parent
**busy-waits** (`subagents.wait`). *Lesson: park and be notified, never spin
(invariant 2).*

Guardrails: handoffs are the durable workspace plus the subagent's final result
(see "Handoffs"); control flow is the parent reasoning over durable stage records
and notifications, never a running script or a poll loop.

## Subagent types

### Read-only (ephemeral) subagent

- Runs in its own disposable snapshot of the parent workspace. **May write inside
  its snapshot** (so it can build, run, and test), but the snapshot is destroyed
  when it returns. "read-only" means it cannot change the durable workspace, not
  that it cannot write at all.
- **Returns only its final message.** Because its snapshot is gone on return, it
  must pack all findings/context into that message. The message is durable (it
  lives in the session transcript), so the parent still receives it in the
  notification.
- **Cannot be steered or interrupted individually** (fire-and-forget; runs to a
  terminal result, which keeps the stage barrier clean). A whole read-only stage
  can be cancelled.
- Many run in parallel in one stage, each isolated.

### Full (durable) subagent

- Writes the parent's workspace **in place** — no snapshot, no adoption. Its
  edits are the parent's state as soon as it finishes. It may leave files behind
  (code, logs, reports) and report their paths in its final message.
- Exactly one per stage; the parent is parked, so it is the only writer.
- **Can be steered and interrupted** by the human and, where useful, the parent.

### Both types are non-recursive

**Subagents cannot spawn subagents.** Hard rule, not a default. Only the
top-level parent orchestrates stages. If a workflow needs decomposition, that is
another stage the parent runs. This keeps the single-writer reasoning one level
deep and avoids nested parking.

## Stages

A **stage** is one step of a run: `kind = full` (one full subagent) or
`kind = readonly_fanout` (one or more read-only subagents).

Lifecycle:

```text
parent calls stage.start_full / stage.start_readonly_fanout
  -> full stage: start one subagent in the parent's workspace dirs (in place)
     read-only stage: snapshot the parent workspace once per subagent and start
       each in its own disposable snapshot
  -> parent ends its turn and parks (idle)
  -> subagents run
  -> as each read-only subagent returns, destroy its snapshot (its final
     message is already durable in the transcript)
  -> when every subagent in the stage is terminal, the daemon:
       - composes a completion notification (each subagent's terminal result;
         for a full stage, the durable file paths it reported),
       - enqueues that notification as input to the parent and drives it.
  -> parent wakes, reads results, decides the next stage (or finishes)
```

A stage is terminal when all its subagents are terminal
(`done`/`failed`/`cancelled`/`crashed`). A failed full subagent leaves whatever
partial edits it made in the durable workspace (no rollback in v1); the parent
recovers with git or by continuing the subagent.

## Snapshots, retries, and (future) rollback

- **Snapshots are for read-only fan-out only.** They exist so parallel read-only
  subagents get a private, stable view and can build/test without touching the
  durable workspace or each other. btrfs subvolume snapshot where available,
  reflink/copy otherwise.
- **GC on return.** A read-only subagent's snapshot is destroyed the moment the
  subagent returns. Snapshots never accumulate, so there is no disk-pressure GC
  policy to design. The cost is that read-only subagents cannot hand back
  files — by design, that is what full subagents are for.
- **The full subagent uses no snapshot.** It writes the durable workspace in
  place.
- **Retry == resume the session.** There is no "restart from a clean snapshot".
  Sessions are durable, resumable, and repairable (the runtime already recovers
  crashed tails to turn boundaries and supports steer/interrupt/continue), so a
  subagent that fails or needs more work is continued, steered, or repaired in
  place. Re-running a stage is the parent's discretion (invariant 3); it runs
  forward on the current workspace, it does not roll anything back.
- **Rollback is future work.** Because the full subagent writes in place, v1 has
  no automatic undo. A later version may snapshot the durable workspace before a
  full stage so its changes can be discarded; this is intentionally out of scope
  now.

Non-btrfs hosts degrade to reflink/copy snapshots for read-only fan-out;
correctness is the same, only slower.

## Workflows as stage templates

A workflow is a **named, discoverable template** suggesting an ordered list of
stages. It is not a compiled state machine and does not own control flow.

```text
workflow.list      -> compact list of templates (id, title, description)
workflow.describe  -> the suggested stages + guidance for a template
```

Example:

```json
{
  "id": "implement_review_test",
  "title": "Implement, review, then test",
  "stages": [
    { "kind": "full",            "role": "implementer", "hint": "implement the change in place" },
    { "kind": "readonly_fanout", "roles": ["reviewer"], "hint": "review the change; report in the final message" },
    { "kind": "full",            "role": "tester",      "hint": "run tests; fix or report" }
  ],
  "guidance": "If review requests changes, re-run the implementer stage. If tests fail on a code issue, return to implement/review."
}
```

The parent reads the template and runs the stages with discretion. Because the
template is only guidance, there is no `advance()` to keep in sync with the
model's judgment. Bundled templates ship as static Rust/JSON first; disk/user
templates later.

Bundled templates to ship first:

- `explore` — one read-only fan-out, then the parent synthesizes from the
  returned messages.
- `implement_review` — full implement, read-only review, repeat at discretion.
- `implement_review_test` — as above plus a full test stage.
- `kubernetes_e2e` — a single full stage with the `kubernetes-tester` role and
  safety rules.

There is no `parallel_race` template: it requires parallel writers, which this
model does not support.

## Handoffs

There is no artifact table. A handoff is:

1. **The durable workspace**, for full stages: the full subagent's edits are
   already present in the parent's directories. It reports relevant file paths
   (code touched, logs written) in its final message and the parent reads them
   directly.
2. **The subagent's terminal result** — `{ status, summary, suggested_next? }`,
   delivered in the completion notification. `suggested_next` is typed against
   the workflow template's declared outcomes, so the parent branches over a known
   set rather than prose.
3. For **read-only** subagents, the terminal message is the *entire* handoff.
   Anything the subagent learned must be in that message; its files do not
   survive. The subagent's system prompt tells it to summarize findings (and
   quote the key evidence inline) before ending its turn.

A structured artifacts view can be added later if full-subagent file outputs
ever need first-class UI; for v1, file paths in the message are enough.

## Minimal durable schema

Reuse `sessions`, `sessions.parent_session_id`, and the existing subagent
workspace-fork metadata. Add one table and two columns.

### `stages`

```text
stages
  id text primary key
  parent_session_id text not null references sessions(id) on delete cascade
  workflow_id text null           -- template the parent was following, if any
  label text null
  kind text not null              -- full | readonly_fanout
  status text not null            -- running | done | cancelled | failed
  attempt_id text not null        -- fences the completion transition
  created_at timestamptz not null default now()
  updated_at timestamptz not null default now()
```

### `sessions` additions

```text
sessions
  ...
  stage_id text null references stages(id)   -- the stage this subagent belongs to
  subagent_type text null                    -- full | read_only (null for top-level)
```

No adoption/rollback bookkeeping is needed (full writes in place; read-only
snapshots are transient and GC'd on return). No `runs`, `tasks`, `artifacts`,
workspace, or lease tables. A "run" is just a parent session and its ordered
stages.

## Stage runner

Small and single-purpose: **detect stage completion, reclaim read-only
snapshots, and notify the parent exactly once.**

```rust
async fn on_subagent_terminal(stage_id: &str) -> Result<()> {
    let mut tx = store.begin().await?;
    let stage = store.lock_stage(&mut tx, stage_id).await?;       // select ... for update
    if stage.status != Running { return tx.commit().await; }      // already handled
    let subs = store.subagents_for_stage(&mut tx, stage_id).await?;
    if subs.iter().any(|s| !s.is_terminal()) { return tx.commit().await; }

    // single-flight: only the running->done transition (CAS on attempt_id) acts.
    store.finish_stage(&mut tx, stage_id, Done, stage.attempt_id).await?;
    let notice = compose_completion_notice(&stage, &subs);        // results + (full) file paths
    store.enqueue_parent_input(&mut tx, &stage.parent_session_id, notice).await?;
    tx.commit().await?;
    Ok(())
}
```

Read-only snapshots are reclaimed per subagent as it returns (independently of
the stage barrier), so the durable handoff never depends on a snapshot still
existing — the message is in the transcript.

Properties (reusing existing patterns): single-flight per stage via the stage
row lock; idempotent (a `Done` stage short-circuits, so the parent is notified
once); attempt-fenced; crash-safe via a startup sweep over `running` stages whose
subagents are all terminal. The runner never decides the next stage.

## Tools

```text
stage.start_full            -> { role, prompt }                 ; one full subagent (in place)
stage.start_readonly_fanout -> { tasks: [{role, prompt}, ...] } ; N read-only subagents (snapshots)
stage.status                -> inspect a stage and its subagents
stage.cancel                -> cancel an in-flight stage
workflow.list               -> list templates
workflow.describe           -> a template's suggested stages
```

Steer/interrupt of the single full subagent reuses the existing subagent path;
read-only subagents reject steer/interrupt by type. System-prompt rule for the
parent: launch at most one stage per turn, then end your turn and wait for the
notification; never poll, never start a second stage while one runs, never mix
full and read-only in one stage. The Python REPL remains a raw escape hatch only.

## Human-in-the-loop

Lighter, because the parent session is the user's session: the parent asks the
user directly; a full subagent that needs the human ends with
`suggested_next = human_needed`, which surfaces in the notification and the
parent relays it. While a stage is in flight the parent is parked and user input
queues (existing queue semantics); to intervene early the user cancels the stage,
or steers the full subagent directly. No blocked-run table, no signal artifact.
(Confirm input behavior in Open Question 1.)

## Testing

Reuse the dev harness that resolves model actions deterministically
(`harness.model.complete` / `harness.model.fail`).

- **Completion / notification (real Postgres):** drive subagents to terminal
  states; assert the parent is notified exactly once; assert re-delivered events
  and restart sweeps do not double-notify; assert a stale `attempt_id` cannot
  re-fire.
- **In-place full writes:** a full subagent's edits are visible in the parent's
  workspace after the stage; the parent and full subagent never write
  concurrently (parking).
- **read-only isolation + GC:** a read-only subagent's writes never reach the
  durable workspace; its snapshot is gone after return; its final message
  survives in the transcript and reaches the parent.
- **Homogeneity / single-flight:** `stage.start_*` rejects mixed stages, more
  than one full subagent, and a second concurrent stage.
- **Parking / resume:** a parent that launches a stage idles and is re-driven
  only by the completion notification.
- **Retry == resume:** continuing a failed subagent re-engages the same session
  rather than creating a fresh one.
- **Typed outcomes:** a `suggested_next` outside the template's set is recorded
  as a subagent error.

## Migration and coexistence

| Surface | Today | Steady state |
| --- | --- | --- |
| `stage.*` + `workflow.*` | new | the way to run staged and parallel-read work |
| Python REPL `subagents.*` (busy-wait) | current primary | raw escape hatch; undocumented for orchestration |

Sequence: ship typed subagents + stages + notifications (Phases 0–3); repoint the
`PI.md` "Subagent delegation" guidance at `stage.*` and park-and-wait; stop
teaching `subagents.wait`. Full removal of the REPL orchestration path is a later
follow-up.

## Implementation phases

### Phase 0: lifecycle foundation

- Land PR #150 (parent-visible child lifecycle events) on `main`.
- Update `architecture.md` to retire the "no subagent orchestration" non-goal.

### Phase 1: typed subagents

- Add `subagent_type` (full | read_only) to sessions.
- **full** subagents run in the parent's workspace dirs in place (stop forking a
  workspace for them).
- **read-only** subagents run in a forked snapshot (reuse `instantiate.rs`) that
  is destroyed when the subagent returns; they return only their final message.

### Phase 2: stages and the homogeneity rule

- Add the `stages` table and `stage_id` on sessions.
- Add `stage.start_full`, `stage.start_readonly_fanout`, `stage.status`,
  `stage.cancel`; enforce homogeneity, single full subagent, and
  one-stage-at-a-time.

### Phase 3: notifications and parking

- Stage runner: single-flight completion, per-subagent read-only snapshot GC,
  completion notification to the parent, attempt fencing, crash-recovery sweep.
- System-prompt park-and-wait instructions; deterministic idempotency tests.

### Phase 4: workflow templates

- `workflow.list` / `workflow.describe` over bundled static templates.
- Ship `explore`, `implement_review`, `implement_review_test`, `kubernetes_e2e`;
  type each template's outcomes.

### Phase 5: UI

- Run board: parent session -> stages -> subagents with terminal results (and,
  for full stages, the reported file paths).
- Controls: cancel stage, steer the full subagent, re-run a stage.

## Open questions

1. **User input during an active stage.** Default: the parent stays parked, user
   follow-ups queue, and the user cancels the stage (or steers the full subagent)
   to intervene early. Is that the right default?
2. **read-only message size.** Since read-only subagents pack everything into one
   message and a fan-out returns N of them, bound the completion notification
   (and decide truncation/summarization) so a large fan-out cannot blow the
   parent's context.
3. **Naming.** "read-only" subagents can write their snapshot; is "ephemeral" or
   "disposable" a less misleading public name? (Mechanics are unchanged either
   way.)
4. **Multi-dir snapshot consistency.** A session can have several workspace
   subdirectories; a read-only subagent must snapshot all of them at one
   consistent point. Low stakes (disposable), but worth doing atomically.
5. **Fan-out failure policy.** If some read-only subagents fail, deliver partial
   results and let the parent decide (proposed), with stage status "completed
   with failures". Confirm.
6. **Crash during a full stage.** The full subagent writes in place with no
   rollback, so a daemon/subagent crash can leave partial edits. v1 recovery is
   git or continuing the subagent; confirm that is acceptable until snapshot
   -before-full-stage rollback is added later.

## Design rules

1. One durable workspace, single writer in time (parent or the one full
   subagent). Parking enforces it.
2. The full subagent writes in place. No adoption, no merge, no rollback in v1.
3. read-only subagents run in disposable snapshots, may build/test in them, and
   are GC'd on return; they hand back only their final message.
4. A stage is homogeneous: one full subagent, or many read-only subagents. Never
   parallel writers.
5. The parent never polls or busy-waits; it parks and is notified.
6. Subagents cannot spawn subagents.
7. Handoffs are the durable workspace (full) plus the subagent's typed terminal
   result. No artifact store, no variable store.
8. Retry means resume/repair the existing session, not start a fresh one.
9. Workflows are templates that suggest stages; the parent owns sequencing.
10. The daemon owns mechanism (typed subagents, snapshots, homogeneity,
    notifications, durable stages); the model owns policy (which stage next,
    re-run, stop).
