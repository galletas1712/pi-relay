# Minimal workflow orchestration plan

Status: proposed. Last reviewed 2026-06-16 (rev: subagent results propagate
through a daemon-written `handoff/` directory — final message + full transcript
per subagent as files — with a short steer notification pointing the parent at
them; the parent parks but stays responsive to the user; read-only (RO) and full
subagent split kept; recovery is just "continue where we left off", no git, no
rollback in v1).

## Summary

Keep the architecture small. The model:

- There is **one durable workspace** — the parent's set of workspace dirs under
  the cwd root. It has **at most one writer**: the parent, or the single **full**
  subagent of a full stage. The full subagent writes those dirs **in place**; its
  edits are simply the parent's state when it finishes. Nothing is merged,
  adopted, or rolled back.
- **read-only (RO)** subagents never write the durable workspace. Each runs in
  its own **disposable btrfs snapshot** of the workspace dirs (reflink/copy
  fallback; machinery already in `instantiate.rs`). It may build/run/test inside
  that snapshot, but the snapshot is **destroyed when the subagent returns**. RO
  is "read-only with respect to the parent filesystem", not literally non-writing.
- Work happens in **stages**: either *one full subagent* or *a parallel fan-out
  of RO subagents* — never both, and never multiple full (writing) subagents.
- Results propagate through a **handoff directory** (below), not through model
  context. The parent **does not busy-wait**: after launching a stage it parks,
  and the daemon delivers a short **steer** when the stage completes. The parent
  stays responsive to the user the whole time.
- A **workflow** is a named template that *suggests* a sequence of stages. The
  parent has full discretion over which stage to run, whether to re-run one, and
  when to stop.

### The two roles

| | read-only (RO, ephemeral) | full (durable) |
| --- | --- | --- |
| Workspace | own disposable snapshot | the parent's dirs, in place |
| May write? | yes, only inside its snapshot | yes, the real workspace |
| On return | snapshot destroyed | edits persist as the parent's state |
| Durable file output | none (snapshot is gone) | yes, in the workspace |
| Result to parent | final message + transcript via handoff dir | same, plus its in-place edits |
| Steerable/interruptible? | no (fire-and-forget) | yes |
| Per stage | many, in parallel | exactly one |

### What makes this safe and simple

> RO subagents are isolated in throwaway snapshots, so any number run in parallel
> without touching the durable workspace. The durable workspace has a single
> writer in time — the parent, or the one full subagent — with no merge, no
> adoption, and no conflict resolution anywhere.

The single-writer guarantee is exact for RO stages (RO never touches the durable
workspace). For a **full** stage it relies on a soft rule: the parent stays
responsive to the user but its prompt tells it the full subagent owns the
workspace, so it supervises/reads/plans rather than editing until the subagent
returns. We do not hard-enforce that (consistent with the runtime not policing
tool behavior elsewhere); see Open Questions.

### What this keeps, deletes, and accepts

Keeps (reusing existing code): btrfs/reflink/copy forking (`instantiate.rs`) for
RO snapshots; subagent sessions and `sessions.parent_session_id`; subagent
steer/interrupt and lifecycle events; the steer queue; session resume/repair.

Deletes: daemon-side `git merge`, cross-task git source refs, multi-parent
lineage; snapshot adoption/swap; the `artifacts` table; the `sources`/`from_task`
workspace modes; the pure `advance()` state machine as the sole owner of control
flow; workflow variables, transition proposals, budgets, leases, graph DSL;
`parallel_race` and any parallel-writer pattern.

Accepts as tradeoffs:

- **No parallel writers.** Only one full (writing) subagent at a time. Parallel
  work is RO only.
- **No rollback / no git recovery in v1.** The full subagent writes in place. On
  crash or interruption we **continue the session where it left off** (the
  runtime already recovers crashed tails to turn boundaries and supports
  steer/continue). Rollback (snapshot-before-full-stage) is deferred.
- **RO subagents return no files.** Their snapshot is destroyed on return, so
  durable file output is a full-subagent capability. Their final message and full
  transcript are still captured to the handoff directory by the daemon.

## The three invariants

1. **A stage is homogeneous.** One full subagent, or a fan-out of RO subagents —
   never a mix, never more than one full. This keeps the durable workspace
   single-writer.
2. **The parent never busy-waits, but it is not frozen.** After launching a stage
   the parent parks (goes idle; it does not spin or poll). It stays a normal
   interactive session: user input drives it to make progress while the stage
   runs. Stage completion is delivered as a high-priority **steer** (so it is
   seen promptly, even mid-turn), not a follow-up. During a full stage the parent
   defers workspace edits to the full subagent (soft rule).
3. **Workflows are stages, but the parent has discretion.** A workflow is an
   ordered list of suggested stages. The parent decides which to run, whether to
   re-run, and when the run is done. The daemon supplies mechanism (typed
   subagents, snapshots, the handoff dir, steer notifications, durable stage
   records); the parent supplies policy.

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

Guardrails: handoffs are the durable workspace plus the handoff directory;
control flow is the parent reasoning over durable stage records and steer
notifications, never a running script or a poll loop.

## Subagent types

### Read-only (RO, ephemeral) subagent

- Runs in its own disposable snapshot of the workspace dirs. **May write inside
  its snapshot** (so it can build, run, and test), but the snapshot is destroyed
  when it returns. "read-only" = cannot change the durable workspace.
- **Returns no files.** Its durable output is its final message plus its full
  transcript, both captured to the handoff directory by the daemon (from the
  session transcript in Postgres — so capture does not depend on the snapshot
  still existing). It should still write a strong summary in its final message,
  since that is what the parent reads first.
- **Cannot be steered or interrupted individually** (fire-and-forget; runs to a
  terminal result, which keeps the stage barrier clean). A whole RO stage can be
  cancelled.
- Many run in parallel in one stage, each isolated.

### Full (durable) subagent

- Writes the parent's workspace **in place** — no snapshot, no adoption. Its
  edits are the parent's state as soon as it finishes; it may leave files behind
  and reference them, since they persist in the workspace.
- Exactly one per stage; it is the durable workspace's writer for that stage.
- **Can be steered and interrupted** by the human and, where useful, the parent.
- Its final message and transcript are also captured to the handoff directory,
  for uniformity with RO stages.

### Both types are non-recursive

**Subagents cannot spawn subagents.** Hard rule. Only the top-level parent
orchestrates stages. If a workflow needs decomposition, that is another stage the
parent runs. This keeps the single-writer reasoning one level deep.

## The handoff directory

Subagent results reach the parent through files, not through model context, so
context stays bounded no matter how large a fan-out or a transcript is.

- The cwd root is not itself a workspace; the workspace dirs live under it. The
  daemon owns one more directory under the cwd root — the **handoff directory**
  (e.g. `<cwd>/.pi-handoff/`). It is not a workspace: it is never forked,
  snapshotted, or part of any git repo.
- On stage completion, for **every** subagent in the stage (success or failure),
  the daemon writes:

  ```text
  <cwd>/.pi-handoff/<stage_id>/<subagent>/final_message.md
  <cwd>/.pi-handoff/<stage_id>/<subagent>/transcript.md
  ```

  These are rendered from the subagent's durable transcript, so they exist even
  after an RO snapshot is gone and even when the subagent crashed.
- The daemon then delivers the notification by enqueuing a **short steer** to the
  parent (which appears as a user message in the parent's transcript). It names
  the stage, says how many subagents succeeded/failed, and gives the handoff
  path — it does **not** inline the messages. Example:

  ```text
  Stage stage_7 (reviewer fan-out) finished: 3 ok, 1 failed.
  Per-subagent final_message.md and transcript.md are under
  <cwd>/.pi-handoff/stage_7/. Failed: reviewer-c (see its transcript.md).
  ```

- The parent reads `final_message.md` files first (summaries) and opens
  `transcript.md` only when it needs detail — using its normal file tools. For a
  full stage, the full subagent's actual edits are already in the workspace; the
  handoff files are its summary/transcript.

There is no `artifacts` table and no structured artifact API in v1; the handoff
directory plus the durable workspace are the entire handoff surface. A structured
view can be layered on later if needed.

## Stages

A **stage** is one step of a run: `kind = full` (one full subagent) or
`kind = readonly_fanout` (one or more RO subagents).

Lifecycle:

```text
parent calls stage.start_full / stage.start_readonly_fanout
  -> full stage: start one subagent in the parent's workspace dirs (in place)
     RO stage: snapshot the workspace dirs per subagent; start each in its snapshot
  -> parent parks (idle) but remains responsive to the user
  -> subagents run; as each RO subagent returns, destroy its snapshot
  -> the daemon BLOCKS until every subagent in the stage is terminal
     (done/failed/cancelled/crashed) -- one barrier, not per-subagent
  -> the daemon writes final_message.md + transcript.md for every subagent into
     the handoff dir, then enqueues ONE short steer to the parent pointing there
  -> parent (woken promptly by the steer) reads the handoff files and decides the
     next stage, a re-run, or done
```

The barrier means a fan-out delivers **partial results and failures together** in
a single notification — never a stream of per-subagent notifications. A failed
subagent simply appears in the handoff dir with its full transcript and is marked
failed in the steer.

## Snapshots, recovery, and (future) rollback

- **Snapshots are for RO fan-out only**, so parallel RO subagents get a private,
  stable view and can build/test without touching the durable workspace or each
  other. btrfs subvolume snapshot where available; reflink/copy otherwise.
- **GC on return.** An RO subagent's snapshot is destroyed when it returns.
  Snapshots never accumulate, so there is no disk-pressure policy to design; the
  cost (no file output from RO) is by design.
- **The full subagent uses no snapshot**; it writes the durable workspace in
  place.
- **Recovery is "continue where we left off".** Sessions are durable, resumable,
  and repairable; the runtime already recovers crashed tails to turn boundaries
  and supports steer/interrupt/continue. A subagent (or the parent) that crashes
  or is interrupted resumes from its recovered state. We do **not** use git to
  recover and we do **not** roll back; a full subagent's partial in-place edits
  remain and it continues from them.
- **Rollback is deferred.** A future version may snapshot the durable workspace
  before a full stage to allow discarding its changes; explicitly out of scope.

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
    { "kind": "readonly_fanout", "roles": ["reviewer"], "hint": "review the change; summarize in the final message" },
    { "kind": "full",            "role": "tester",      "hint": "run tests; fix or report" }
  ],
  "guidance": "If review requests changes, re-run the implementer stage. If tests fail on a code issue, return to implement/review."
}
```

The parent reads the template and runs the stages with discretion. Because the
template is only guidance, there is no `advance()` to keep in sync with the
model's judgment. Bundled templates ship as static Rust/JSON first.

Bundled templates to ship first:

- `explore` — one RO fan-out; the parent synthesizes from the handoff files.
- `implement_review` — full implement, RO review, repeat at discretion.
- `implement_review_test` — as above plus a full test stage.
- `kubernetes_e2e` — a single full stage with the `kubernetes-tester` role and
  safety rules.

There is no `parallel_race` template: it requires parallel writers, which this
model does not support.

## Minimal durable schema

Reuse `sessions`, `sessions.parent_session_id`, and the existing subagent
workspace-fork metadata. Add one table and two columns. The handoff directory is
derived from the session cwd; it needs no schema.

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

No adoption/rollback bookkeeping (full writes in place; RO snapshots are
transient). No `runs`, `tasks`, `artifacts`, workspace, or lease tables. A "run"
is just a parent session and its ordered stages.

## Stage runner

Small and single-purpose: **detect the stage barrier, write the handoff files,
and steer the parent exactly once.**

```rust
async fn on_subagent_terminal(stage_id: &str) -> Result<()> {
    let mut tx = store.begin().await?;
    let stage = store.lock_stage(&mut tx, stage_id).await?;       // select ... for update
    if stage.status != Running { return tx.commit().await; }      // already handled
    let subs = store.subagents_for_stage(&mut tx, stage_id).await?;
    if subs.iter().any(|s| !s.is_terminal()) { return tx.commit().await; }  // barrier

    // single-flight: only the running->done transition (CAS on attempt_id) acts.
    store.finish_stage(&mut tx, stage_id, outcome_of(&subs), stage.attempt_id).await?;
    handoff::write_files(&stage, &subs).await?;                   // final_message.md + transcript.md each
    let steer = compose_handoff_steer(&stage, &subs);             // short pointer text
    store.enqueue_parent_steer(&mut tx, &stage.parent_session_id, steer).await?;
    tx.commit().await?;
    Ok(())
}
```

RO snapshots are reclaimed per subagent as each returns (independent of the
barrier). The handoff files are rendered from the durable transcript, so they do
not depend on a snapshot still existing.

Properties (reusing existing patterns): single-flight per stage via the stage row
lock; idempotent (a `Done` stage short-circuits, so the parent is steered once);
attempt-fenced; crash-safe via a startup sweep over `running` stages whose
subagents are all terminal. The runner never decides the next stage.

## Tools

```text
stage.start_full            -> { role, prompt }                 ; one full subagent (in place)
stage.start_readonly_fanout -> { tasks: [{role, prompt}, ...] } ; N RO subagents (snapshots)
stage.status                -> inspect a stage and its subagents
stage.cancel                -> cancel an in-flight stage
workflow.list               -> list templates
workflow.describe           -> a template's suggested stages
```

Steer/interrupt of the single full subagent reuses the existing subagent path; RO
subagents reject steer/interrupt by type. System-prompt rules for the parent:
launch at most one stage per turn, then end your turn; do not poll; do not start
a second stage while one runs; never mix full and RO in one stage; while a full
subagent runs, supervise and read but do not edit the workspace yourself; when a
handoff steer arrives, read the handoff files before deciding the next stage. The
Python REPL remains a raw escape hatch only.

## Human-in-the-loop

The parent session is the user's session and stays responsive: the user can steer
the parent while a stage runs, and the parent asks the user directly when it needs
a decision. A full subagent that needs the human ends with
`suggested_next = human_needed`, which appears in its handoff `final_message.md`
and the steer, and the parent relays it. To intervene in a running stage the user
cancels it or steers the full subagent directly. No blocked-run table, no signal
artifact.

## Testing

Reuse the dev harness that resolves model actions deterministically
(`harness.model.complete` / `harness.model.fail`).

- **Barrier + handoff + steer (real Postgres):** drive subagents to terminal
  states; assert one steer is delivered only after all subagents are terminal;
  assert `final_message.md`/`transcript.md` exist for every subagent including
  failed ones; assert re-delivered events and restart sweeps do not double-steer;
  assert a stale `attempt_id` cannot re-fire.
- **Steer, not follow-up:** the notification is delivered at steer priority and is
  seen promptly (mid-turn after a tool batch if the parent is running).
- **In-place full writes:** a full subagent's edits are visible in the parent's
  workspace after the stage.
- **RO isolation + GC:** an RO subagent's writes never reach the durable
  workspace; its snapshot is gone after return; its `final_message.md`/
  `transcript.md` survive in the handoff dir.
- **Homogeneity / single-flight:** `stage.start_*` rejects mixed stages, more than
  one full subagent, and a second concurrent stage.
- **Continue-where-left-off:** a crashed/interrupted subagent resumes the same
  session from its recovered turn boundary; no git, no rollback.
- **Typed outcomes:** a `suggested_next` outside the template's set is recorded as
  a subagent error.

## Migration and coexistence

| Surface | Today | Steady state |
| --- | --- | --- |
| `stage.*` + `workflow.*` | new | the way to run staged and parallel-RO work |
| Python REPL `subagents.*` (busy-wait) | current primary | raw escape hatch; undocumented for orchestration |

Sequence: ship typed subagents + stages + handoff/steer notifications
(Phases 0–3); repoint the `PI.md` "Subagent delegation" guidance at `stage.*`,
the handoff directory, and park-but-stay-responsive; stop teaching
`subagents.wait`. Full removal of the REPL orchestration path is a later
follow-up.

## Implementation phases

### Phase 0: lifecycle foundation

- Land PR #150 (parent-visible child lifecycle events) on `main`.
- Update `architecture.md` to retire the "no subagent orchestration" non-goal.

### Phase 1: typed subagents

- Add `subagent_type` (full | read_only) to sessions.
- **full** subagents run in the parent's workspace dirs in place (stop forking a
  workspace for them).
- **RO** subagents run in a forked snapshot (reuse `instantiate.rs`) destroyed on
  return.

### Phase 2: stages and the homogeneity rule

- Add the `stages` table and `stage_id` on sessions.
- Add `stage.start_full`, `stage.start_readonly_fanout`, `stage.status`,
  `stage.cancel`; enforce homogeneity, a single full subagent, and
  one-stage-at-a-time.

### Phase 3: handoff directory, barrier, and steer notifications

- Implement the handoff directory writer (final_message.md + transcript.md per
  subagent, rendered from the durable transcript).
- Stage runner: barrier over all subagents, single-flight completion, one steer
  to the parent pointing at the handoff dir, attempt fencing, crash-recovery
  sweep.
- System-prompt park-but-stay-responsive instructions; deterministic idempotency
  tests.

### Phase 4: workflow templates

- `workflow.list` / `workflow.describe` over bundled static templates.
- Ship `explore`, `implement_review`, `implement_review_test`, `kubernetes_e2e`;
  type each template's outcomes.

### Phase 5: UI

- Run board: parent session -> stages -> subagents with status and links to their
  handoff files; show the full subagent's in-place changes.
- Controls: cancel stage, steer the full subagent, re-run a stage.

## Open questions

1. **Concurrent parent + full-subagent writes.** Because the parent stays
   responsive during a full stage, it *could* edit the workspace while the full
   subagent does. RO stages are conflict-free; for full stages this rests on a
   soft prompt rule. Acceptable for v1, or do we eventually hard-park the parent's
   write tools during a full stage?
2. **Handoff directory cleanup.** `handoff/<stage_id>/...` files accumulate under
   the cwd root across stages. Reclaim on run end, on parent-session delete, or by
   TTL? They are also handy history, so deletion should be deliberate.
3. **Handoff file format.** `transcript.md` rendering (human-readable vs a
   machine-greppable form), and whether to also drop a compact `index.json` per
   stage that the parent can read in one shot.
4. **Multi-dir snapshot consistency.** An RO subagent must snapshot all workspace
   subdirectories at one consistent point. Low stakes (disposable) but do it
   atomically.
5. **Handoff dir and tooling visibility.** Confirm the handoff dir is excluded
   from workspace git repos (it is a sibling of the workspace dirs, so naturally
   outside them) and from RO snapshots, and that the parent's file tools can read
   it.

## Design rules

1. One durable workspace, single writer in time (parent or the one full
   subagent). RO never touches it.
2. The full subagent writes in place. No adoption, no merge, no rollback in v1.
3. RO subagents run in disposable snapshots, may build/test in them, and are GC'd
   on return; they return no files.
4. A stage is homogeneous: one full, or many RO. Never parallel writers.
5. The parent parks (no spin) but stays responsive; completion arrives as a
   steer, not a follow-up.
6. Subagents cannot spawn subagents.
7. Results propagate through the handoff directory (final message + transcript
   per subagent) plus, for full stages, the durable workspace. No artifact store,
   no variable store, no context dump.
8. Recovery is "continue where we left off" — no git recovery, no rollback in v1.
9. Workflows are templates that suggest stages; the parent owns sequencing.
10. The daemon owns mechanism (typed subagents, snapshots, handoff dir, steers,
    durable stages); the model owns policy (which stage next, re-run, stop).
