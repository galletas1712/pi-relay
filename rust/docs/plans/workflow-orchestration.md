# Minimal workflow orchestration plan

Status: proposed. Last reviewed 2026-06-16 (rev: subagents run in disposable
btrfs snapshots instead of a shared filesystem — isolation enforces
"read-only", gives cheap rollback, and reuses the existing workspace-fork
machinery; nothing is ever merged; recursion is banned; non-code evidence is
passed as absolute paths in the subagent's final message).

## Summary

Keep the architecture small. The earlier drafts oscillated between heavy
workspace isolation (forks + git merge + multi-parent lineage) and a fully
shared filesystem (which needed hard-to-enforce read-only sandboxing). This
revision takes the good half of each: **cheap per-subagent snapshots for
isolation, but no merge — ever.**

The model:

- Each subagent runs in its **own btrfs snapshot** of the parent's workspace
  (reflink/copy fallback on non-btrfs hosts — this machinery already exists in
  `instantiate.rs`). Snapshots are cheap and isolated.
- There are two subagent types, distinguished by **what happens to their
  snapshot when they finish**:
  - **read-only (disposable):** the snapshot is **discarded**, never adopted.
    The agent may still write inside its sandbox (so it can build, run, and
    test), but those writes never reach the parent. Isolation is what makes
    "read-only" safe — there is nothing to enforce.
  - **full:** on success the snapshot is **adopted** as the parent's new
    workspace (an atomic subvolume swap, performed while the parent is parked).
    On failure or restart it is discarded.
- Work happens in **stages**. A stage is either *one full subagent* or *a
  parallel fan-out of read-only subagents* — never both, because at most one
  snapshot can be adopted per stage.
- The parent **does not wait or poll**. After launching a stage it ends its turn
  and parks; the daemon notifies it when the stage completes (the "background
  agents" pattern). The parent then decides what to do next.
- A **workflow** is a named template that *suggests* a sequence of stages. The
  parent has full discretion over which stage to run, whether to re-run one, and
  when to stop.

### What makes this safe and simple

> Snapshots isolate, so concurrent subagents never corrupt each other. At most
> one snapshot is adopted per stage, and adoption is a **swap, never a merge** —
> so there is no conflict resolution anywhere in the system.

Because nothing is merged, the multi-parent / git-source-ref / `integrate`-agent
machinery from earlier drafts is gone, and because snapshots are throwaway, the
read-only-enforcement problem from the shared-filesystem draft is gone too.

### What this keeps, deletes, and accepts

Keeps (reusing existing code):

- btrfs snapshot / reflink / copy workspace forking (`instantiate.rs`);
- subagent sessions and `sessions.parent_session_id`;
- subagent steer/interrupt and lifecycle events.

Deletes:

- daemon-side `git merge`, cross-task git source refs, multi-parent lineage;
- the `artifacts` table and artifact-kind enumeration;
- the `sources` / `from_task` multi-ancestor workspace modes;
- the pure `advance()` workflow state machine as the sole owner of control flow;
- workflow variables, transition proposals, budgets, leases, graph DSL.

Accepts as a tradeoff:

- By current invariant a stage adopts at most one snapshot, so there is **one
  full (writing) subagent per stage**. Note that snapshot isolation now makes
  parallel *writing* candidates technically safe (run N full subagents, adopt
  one winner, discard the rest — i.e. `parallel_race`). We keep it to one full
  per stage for now, but that door is reopened if we want it (Open Question 2).

## The three invariants

1. **A stage is homogeneous.** One full subagent, or a fan-out of read-only
   subagents — never a mix. Justification is now "at most one adoption per
   stage", not "at most one writer" (snapshots already isolate writers).
2. **The parent never busy-waits.** After launching a stage the parent ends its
   turn and parks (idle). The daemon delivers a completion notification as new
   input when the stage finishes. This is strictly required for a **full** stage
   (adoption swaps the parent's workspace and must not race parent writes) and
   kept for read-only stages for a uniform background-agents UX. (Snapshot
   isolation means a non-blocking read-only fan-out is possible later — Open
   Question 4.)
3. **Workflows are stages, but the parent has discretion.** A workflow is an
   ordered list of suggested stages. The parent decides which to run, whether to
   re-run, and when the run is done. The daemon supplies mechanism (snapshots,
   adoption, homogeneity, notifications, durable stage records); the parent
   supplies policy.

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

Guardrails: handoffs are the adopted workspace plus the subagent's final result
(see "Handoffs"); control flow is the parent reasoning over durable stage records
and notifications, never a running script or a poll loop.

## Subagent types

### Read-only (disposable) subagent

- Runs in its own snapshot. **May write inside its sandbox** (so it can build,
  run, and test), but the snapshot is **discarded** — never adopted, never
  merged. That is the entire meaning of "read-only": durable-with-respect-to-the
  -parent, not literally non-writing.
- **Cannot be steered or interrupted individually** (fire-and-forget; runs to a
  terminal result, which keeps the stage barrier clean). A whole read-only stage
  can be cancelled.
- Many run in parallel in one stage, each isolated.
- The name is slightly misleading because they can write their sandbox; if we
  want a clearer term, "disposable" or "non-adopting" fits the mechanics better.
  (Open Question 6.)

### Full subagent

- Runs in its own snapshot. On **success** the daemon **adopts** the snapshot as
  the parent's workspace (subvolume swap, while the parent is parked). On failure
  or restart the snapshot is discarded and the parent's workspace is untouched.
- Exactly one per stage (one adoption per stage).
- **Can be steered and interrupted** by the human and, where useful, the parent.

### Both types are non-recursive

**Subagents cannot spawn subagents.** This is a hard rule, not a default. Only
the top-level parent orchestrates stages. If a workflow needs decomposition, that
is another stage the parent runs. This keeps the snapshot/adoption reasoning one
level deep and avoids nested parking.

## Stages

A **stage** is one step of a run: `kind = full` (one full subagent) or
`kind = readonly_fanout` (one or more read-only subagents).

Lifecycle:

```text
parent calls stage.start_full / stage.start_readonly_fanout
  -> daemon snapshots the parent workspace once per subagent and starts the
     child sessions in those snapshots
  -> parent ends its turn and parks (idle)
  -> subagents run, each isolated in its own snapshot
  -> when every subagent in the stage is terminal, the daemon:
       - for a successful full stage: adopts the full subagent's snapshot as the
         parent's workspace (keeping the pre-adoption state as a rollback
         checkpoint);
       - for a read-only stage: adopts nothing;
       - retains every non-adopted snapshot read-only (so reported artifact
         paths stay valid) until run teardown;
       - composes a completion notification (each subagent's terminal result +
         absolute artifact paths + a session link);
       - enqueues that notification as input to the parent and drives it.
  -> parent wakes, reads results, decides the next stage (or finishes)
```

A stage is terminal when all its subagents are terminal
(`done`/`failed`/`cancelled`/`crashed`). A failed full subagent is not adopted;
its snapshot is retained read-only for inspection.

## Snapshots, adoption, and rollback

- **Snapshot** — taken from the parent's current workspace at stage launch, one
  per subagent. btrfs subvolume snapshot where available; reflink/copy fallback
  otherwise (existing behavior). Cheap, isolated.
- **Adoption** — only for a successful full stage: the subagent's subvolume
  replaces the parent's workspace at the same path (atomic swap while the parent
  is parked). This is a single-writer swap, **not a merge**: no conflict
  resolution exists in the system.
- **Retention** — non-adopted snapshots (all read-only snapshots, plus failed
  full snapshots) are kept **read-only** until the run is torn down, so the
  parent can open files the subagent reported by absolute path.
- **Rollback / restart** — discard a subagent's snapshot and re-snapshot from the
  parent's current workspace, then re-run. For a full stage that was already
  adopted, restore the pre-adoption checkpoint. Because the parent is parked
  during stages, these operations never race live work.
- **Retry vs restart** — proposed definition: *restart* = fresh snapshot from the
  current parent state, run from scratch (clean). *retry* = the same as restart
  by default (do not resume a half-broken sandbox); a "continue in the same dirty
  snapshot" mode can be added later if a real need appears. (Open Question 3.)

Non-btrfs hosts degrade to reflink/copy snapshots and a directory move for
adoption; correctness is the same, only slower.

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
    { "kind": "full",            "role": "implementer", "hint": "implement the change" },
    { "kind": "readonly_fanout", "roles": ["reviewer"], "hint": "review the adopted diff" },
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

- `explore` — one read-only fan-out, then the parent synthesizes.
- `implement_review` — full implement, read-only review, repeat at discretion.
- `implement_review_test` — as above plus a full test stage.
- `kubernetes_e2e` — a single full stage with the `kubernetes-tester` role and
  safety rules.

Read-only fan-out can now run builds/tests (each in its own snapshot), so
parallel analysis that compiles or runs the test suite is fine — unlike the
prior draft, this no longer has to be a full stage.

## Handoffs

There is no artifact table. A handoff is:

1. **Filesystem state.** For a full stage, the adopted workspace *is* the
   parent's workspace — the changes are simply present. For read-only stages,
   each subagent's snapshot is retained read-only and reachable by absolute path.
2. **The subagent's terminal result** — `{ status, summary, suggested_next? }`,
   delivered in the completion notification. `suggested_next` is typed against
   the workflow template's declared outcomes, so the parent branches over a known
   set rather than prose.
3. **Absolute artifact paths.** The subagent's system prompt instructs it to
   write any non-code evidence (logs, captured events, reports) to files in its
   cwd and to **list the absolute paths in its final message before the turn
   ends**. The parent receives those paths and can open them (retained snapshots
   keep them valid). A structured artifacts view can be added later; for now,
   passing paths is enough to give the parent context.

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
  adopted_session_id text null    -- the full subagent whose snapshot was adopted
  attempt_id text not null        -- fences the completion/adoption transition
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

Snapshot subvolume paths and the rollback checkpoint reuse the existing
per-session workspace metadata; no new workspace/lease tables. A "run" is just a
parent session and its ordered stages.

## Stage runner

Small and single-purpose: **detect stage completion, perform adoption if it is a
successful full stage, and notify the parent exactly once.**

```rust
async fn on_subagent_terminal(stage_id: &str) -> Result<()> {
    let mut tx = store.begin().await?;
    let stage = store.lock_stage(&mut tx, stage_id).await?;       // select ... for update
    if stage.status != Running { return tx.commit().await; }      // already handled
    let subs = store.subagents_for_stage(&mut tx, stage_id).await?;
    if subs.iter().any(|s| !s.is_terminal()) { return tx.commit().await; }

    // single-flight: only the running->done transition (CAS on attempt_id) acts.
    let adopted = stage.kind == Full && subs[0].succeeded();
    if adopted { workspace.adopt(&stage.parent_session_id, &subs[0]).await?; }
    store.finish_stage(&mut tx, stage_id, adopted.then(|| subs[0].id), stage.attempt_id).await?;
    let notice = compose_completion_notice(&stage, &subs);        // results + paths + links
    store.enqueue_parent_input(&mut tx, &stage.parent_session_id, notice).await?;
    tx.commit().await?;     // then drive the parent outside the lock
    Ok(())
}
```

Properties (all reusing existing patterns): single-flight per stage via the stage
row lock; idempotent (a stage already `Done` short-circuits, so the parent is
notified once); attempt-fenced adoption/notification; crash-safe via a
startup sweep over `running` stages whose subagents are all terminal. The runner
never decides the next stage — that is the parent's job.

## Tools

```text
stage.start_full            -> { role, prompt }                         ; one full subagent
stage.start_readonly_fanout -> { tasks: [{role, prompt}, ...] }         ; N read-only subagents
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
queues (existing queue semantics); to intervene early the user cancels the stage.
No blocked-run table, no signal artifact. (Confirm input behavior in Open
Question 5.)

## Testing

Reuse the dev harness that resolves model actions deterministically
(`harness.model.complete` / `harness.model.fail`).

- **Completion / adoption / notification (real Postgres + btrfs):** drive
  subagents to terminal states; assert a successful full stage adopts exactly
  once and swaps the parent workspace; assert read-only stages adopt nothing;
  assert re-delivered events and restart sweeps do not double-notify or
  double-adopt; assert a stale `attempt_id` cannot re-fire.
- **Isolation:** a read-only subagent that writes does not affect the parent or
  its siblings; its files are reachable read-only by the reported path until
  teardown.
- **Rollback:** discarding a snapshot restores the parent's workspace exactly;
  restarting re-snapshots from current state.
- **Homogeneity / single-flight:** `stage.start_*` rejects mixed stages and a
  second concurrent stage.
- **Parking / resume:** a parent that launches a stage idles and is re-driven
  only by the completion notification.
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

### Phase 1: typed subagents in snapshots

- Add `subagent_type` (full | read_only) to sessions.
- Keep the existing workspace fork for every subagent's snapshot; add
  **non-adoption** for read-only and **adopt-on-success** (subvolume swap +
  rollback checkpoint) for full.
- Add read-only snapshot **retention** until run teardown.

### Phase 2: stages and the homogeneity rule

- Add the `stages` table and `stage_id` on sessions.
- Add `stage.start_full`, `stage.start_readonly_fanout`, `stage.status`,
  `stage.cancel`; enforce homogeneity and one-stage-at-a-time.

### Phase 3: notifications and parking

- Stage runner: single-flight completion, adoption, completion notification to
  the parent, attempt fencing, crash-recovery sweep.
- System-prompt park-and-wait instructions; deterministic idempotency tests.

### Phase 4: workflow templates

- `workflow.list` / `workflow.describe` over bundled static templates.
- Ship `explore`, `implement_review`, `implement_review_test`, `kubernetes_e2e`;
  type each template's outcomes.

### Phase 5: UI

- Run board: parent session -> stages -> subagents with terminal results and
  artifact paths; show adopted vs discarded snapshots.
- Controls: cancel stage, steer the full subagent, re-run / rollback a stage.

## Open questions

1. **Does the full agent work in a snapshot adopted on success, or write the
   parent's dir in place?** This rev assumes snapshot + adopt-on-success (uniform
   with read-only, gives rollback). In-place is simpler but loses clean rollback.
   Confirm the snapshot+adopt choice.
2. **Revive parallel writers?** Snapshot isolation makes N full subagents +
   adopt-one-winner (`parallel_race`) safe again. Keep the one-full-per-stage
   invariant, or reopen parallel write candidates with an explicit winner-pick
   step?
3. **Retry semantics.** Proposed: retry == restart (fresh snapshot, run from
   scratch); no "resume the dirty sandbox" mode in v1. Acceptable?
4. **Non-blocking read-only fan-out?** Because read-only snapshots are isolated,
   the parent could keep working while a read-only fan-out runs in the
   background, instead of parking. Keep parking for uniformity, or allow
   background read-only stages later?
5. **User input during an active stage.** Default: parent stays parked, user
   follow-ups queue, user cancels the stage to intervene early. Right default?
6. **Naming.** "read-only" agents can write their sandbox; is "disposable" or
   "non-adopting" a less misleading name?
7. **Snapshot GC / disk pressure.** Retained read-only snapshots accumulate per
   run. When are they reclaimed — on run teardown, on parent session close, or a
   TTL? A long run with many read-only fan-outs could hold many subvolumes.
8. **Adoption atomicity across multiple workspace dirs.** A session can have
   several workspace subdirectories. Adoption must swap them consistently (all or
   nothing) so a crash mid-adoption cannot leave a half-swapped workspace.
9. **Notification size.** Bound the completion notice for large fan-outs
   (summaries + paths + links, with truncation) so it does not blow the parent's
   context.
10. **Fan-out failure policy.** If some read-only subagents fail, deliver partial
    results and let the parent decide (proposed), with stage status "completed
    with failures".

## Design rules

1. Snapshots isolate; nothing is ever merged. Adoption is a single swap.
2. Every subagent runs in its own snapshot of the parent workspace.
3. read-only (disposable) snapshots are discarded; a successful full snapshot is
   adopted. That distinction *is* the subagent-type distinction.
4. A stage is homogeneous: one full, or many read-only (one adoption per stage).
5. The parent never polls or busy-waits; it parks and is notified.
6. Subagents cannot spawn subagents.
7. Handoffs are the adopted/retained filesystem plus the subagent's typed
   terminal result plus absolute artifact paths. No artifact store, no variable
   store.
8. Workflows are templates that suggest stages; the parent owns sequencing.
9. The daemon owns mechanism (snapshots, adoption, homogeneity, notifications,
   durable stages); the model owns policy (which stage next, re-run, rollback,
   stop).
10. Prefer rollback (discard a snapshot) over any attempt to undo writes in place.
