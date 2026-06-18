# Minimal workflow orchestration plan

Status: proposed. Last reviewed 2026-06-16 (rewrite: shared-filesystem subagents,
read-only vs full subagent types, time-serialized single-writer model,
notification-driven parent parking, workflows as parent-driven stage templates;
removed workspace isolation, snapshots, git merge, multi-parent lineage, and the
artifact table).

## Summary

Keep the architecture small. Orchestration needs far less than earlier drafts
assumed once we serialize writes **in time** instead of isolating them **in
space**.

The model is:

- Subagents run in the **same filesystem** as the parent. No btrfs subvolume, no
  snapshot, no fork, no per-subagent workspace, no git merge, no multi-parent
  lineage, no artifact table.
- There are exactly **two subagent types**: **read-only** (cannot write, cannot
  be steered/interrupted) and **full** (can write, can be steered/interrupted).
- Work happens in **stages**. A stage is either *one full subagent* or *a
  parallel fan-out of read-only subagents* — never both.
- The parent **does not wait or poll**. After launching a stage it ends its turn
  and parks. The daemon notifies it when the stage completes (the "background
  agents" pattern). The parent then decides what to do next.
- A **workflow** is just a named template that *suggests* a sequence of stages.
  The parent has full discretion over which stage to run, whether to re-run one,
  and when to stop.

### The one property that makes shared filesystem safe

> **At most one agent can write the workspace at any instant.**

This holds by construction from the three invariants below:

| Who is active | Writers | Why |
| --- | --- | --- |
| One full subagent (parent parked) | the full subagent only | inv. 1 forbids a co-active read-only fan-out; inv. 2 parks the parent |
| N read-only subagents (parent parked) | none | read-only cannot write; inv. 2 parks the parent |
| The parent, between stages | the parent only | a stage's subagents are all terminal before the parent resumes |

There is never a moment with two writers, so no isolation, snapshotting, or
merging is needed. This is the whole point of the design.

### What this deletes from earlier drafts

- workspace source modes (`fork_parent` / `from_task` / `sources`);
- btrfs/reflink snapshot-per-subagent and the workspace fork primitives for
  subagents;
- cross-task git source refs and any daemon-side `git merge`;
- multi-parent task lineage;
- the `artifacts` table and the artifact-kind enumeration;
- the pure `advance()` workflow state machine as the sole owner of control flow;
- workflow variables, transition proposals, budgets, leases, graph DSL.

### What this accepts as a tradeoff

Parallel work can only be **read-only**. You cannot run N full subagents editing
code in parallel, because they would share one filesystem. `parallel_race` /
"N candidate implementations in parallel" is therefore **not supported** in this
model. The intended substitute is: parallel read-only exploration to gather
options, then a single full stage to implement the chosen one. This is a
deliberate, conscious loss of capability in exchange for deleting all isolation
machinery. See Open Questions.

## The three invariants

1. **A stage is homogeneous.** Within one fan-out there is either a single full
   subagent or a set of read-only subagents — never a mix. (This is what keeps
   "at most one writer" true during a stage.)
2. **The parent never busy-waits.** After it launches a stage, the parent ends
   its turn and becomes idle ("parked"). The daemon delivers a completion
   notification as new input when the stage finishes, and the parent resumes
   then. The parent is instructed in its system prompt to expect this and to
   stop after launching a stage. While a stage is in flight the parent is not
   running, so it is not a second writer.
3. **Workflows are stages, but the parent has discretion.** A workflow is an
   ordered list of suggested stages. The parent decides which stage to run next,
   whether a stage needs re-execution, and when the run is done. The daemon
   supplies the mechanism (typed subagents, the homogeneity rule, completion
   notifications, durable stage records); the parent supplies the policy.

## Relationship to the stated architecture

This plan reverses a documented non-goal. `architecture.md` lists "Do not
include subagent orchestration" (Goal 6) and "Hierarchical subagent
orchestration" under "Not implemented by design", and "Removed Pieces" still
names the deleted `agent-orchestrator` crate. That guidance predates durable
subagent sessions, parent links, and lifecycle events, all of which have since
shipped. Before Phase 1, `architecture.md` must be updated in the same change so
the docs and this plan agree. The durable runtime stays small; orchestration
becomes an explicit, persisted concept instead of a forbidden one.

### This is the third orchestration attempt; do not repeat the first two

The runtime has already tried and discarded two designs:

1. **`agent-orchestrator` crate + `SessionRegistry` (removed).** Control flow
   lived in a process-local object graph. *Lesson: durable Postgres state, not a
   live object graph, is the source of truth.*
2. **`workflow_variables` + `work.*` RPCs + Python workflow SDK (deleted).**
   Control flow polled named variables and lived in editable Python templates.
   *Lesson: no general-purpose variable store, and no model-authored
   control-flow scripts as the normal path.*

A third lesson comes from the current Python REPL `subagents.*` API: the parent
**busy-waits** (`subagents.wait(...)`) while children run. *Lesson: the parent
must park and be woken by a notification, never spin.* Invariant 2 exists
specifically to retire busy-waiting.

Guardrails that follow:

- No general cross-subagent state store. Handoffs are the subagent's final
  result plus the shared filesystem (see "Handoffs").
- Control flow is the parent reasoning over **durable stage records** and
  **completion notifications**, never a running Python script and never a poll
  loop.

## Subagent types

### Read-only subagent

- **Cannot write the workspace.** This must be *enforced*, not advised, because
  read-only subagents run in parallel against a shared filesystem; a stray write
  would corrupt every sibling's view. (Enforcement mechanism is Open Question 1.)
- **Cannot be steered or interrupted individually.** It is fire-and-forget; it
  runs to a terminal result. This keeps the stage's completion barrier clean.
- Used for investigation, code reading, review, classification, and any analysis
  that does not need to build, test, or modify files.
- Many can run in parallel in a single stage.

### Full subagent

- **Can write the workspace.** Exactly one runs at a time (a full stage is a
  single subagent).
- **Can be steered and interrupted** by the human and, where useful, by the
  parent. It is the long-lived collaborator that does the real edits, builds,
  and tests.
- Non-recursive by default: a full subagent does not itself orchestrate stages
  in v1 (keeps the one-writer argument one level deep). Recursion is deferred —
  see Open Questions.

A stage's subagents are non-recursive workers, not sub-orchestrators. If a
workflow needs decomposition, that is another stage the parent runs.

## Stages

A **stage** is one step of a run:

- `kind = full` — exactly one full subagent.
- `kind = readonly_fanout` — one or more read-only subagents.

Stage lifecycle:

```text
parent calls stage.start_full / stage.start_readonly_fanout
  -> daemon creates a durable stage row + child sessions (read-only or full)
  -> parent ends its turn and parks (idle)
  -> subagents run against the shared filesystem
  -> when every subagent in the stage is terminal, the daemon:
       - marks the stage done (single-flight transition),
       - composes a completion notification (each subagent's result),
       - enqueues that notification as input to the parent,
       - drives the parent.
  -> parent wakes, reads results, decides the next stage (or finishes)
```

A stage is **terminal** when all of its subagents are terminal
(`done`/`failed`/`cancelled`/`crashed`). The notification reports successes and
failures; the parent decides whether to re-run, run a different stage, or stop.

The parent may cancel an in-flight stage (cancels all its subagents). It cannot
steer an individual read-only subagent; it can steer the single full subagent of
a full stage.

## No busy-wait: notifications and parking

Invariant 2 is implemented with the existing input/queue machinery, not a new
control loop:

- `stage.start_*` returns immediately with a `stage_id`. The parent's system
  prompt instructs it to then **end its turn** — produce a short "launched stage
  X, waiting" message and stop. The parent session goes idle.
- On stage completion the daemon **enqueues a system-originated input** into the
  parent (the completion notification) and drives it, exactly as a queued
  follow-up would be delivered to an idle session today.
- If the stage finishes before the parent's launching turn ends, the
  notification simply queues and is delivered when the parent next idles. No
  race, no poll.

The notification is the handoff surface for the parent: it carries each
subagent's terminal result (status, summary, `suggested_next`) and a link to
each subagent session for deeper inspection. The parent does not need file
diffs, because the subagents' file changes are already present in the shared
workspace.

This is the "background agents" pattern: launch, park, get notified, continue.

## Workflows as stage templates

A workflow is a **named, discoverable template** that suggests an ordered list
of stages. It is not a compiled state machine and it does not own control flow.

```text
workflow.list      -> compact list of templates (id, title, description)
workflow.describe  -> the suggested stages for a template
```

`workflow.describe` returns something like:

```json
{
  "id": "implement_review_test",
  "title": "Implement, review, then test",
  "stages": [
    { "kind": "full",            "role": "implementer", "hint": "implement the change" },
    { "kind": "readonly_fanout", "roles": ["reviewer"], "hint": "review the diff" },
    { "kind": "full",            "role": "tester",      "hint": "run tests; fix or report" }
  ],
  "guidance": "If review requests changes, re-run the implementer stage. If tests fail on a code issue, return to implement/review."
}
```

The parent reads the template and runs the stages with discretion: skip, re-run,
reorder, or stop based on each stage's results. Bundled templates ship as static
Rust/JSON; disk/user templates can come later. Because the template is only
guidance, there is no `advance()` to keep in sync with the model's judgment.

Bundled templates to ship first (all expressible as full/read-only stages):

- `explore` — one read-only fan-out, then the parent synthesizes.
- `implement_review` — full implement, read-only review, repeat at parent's
  discretion.
- `implement_review_test` — as above plus a full test stage.
- `kubernetes_e2e` — a single full stage with the `kubernetes-tester` role and
  safety rules (it writes/builds and talks to a cluster, so it is full, not
  read-only).

`hill_climb` and `parallel_race` from earlier drafts are intentionally **not**
included: both assume parallel writers, which this model does not support.

## Shared filesystem

Subagents use the parent session's workspace directories directly. There is no
per-subagent workspace instantiation for subagents (the project-session workspace
materialization path is unchanged; only subagent forking is removed).

Consequences:

- A full subagent's edits are immediately visible to the parent when the stage
  completes — nothing to import or merge.
- Read-only subagents see a stable tree, because nothing else writes while they
  run (the one-writer property).
- Durable code changes are committed with the parent's normal git workflow by
  whichever agent did the writing; the daemon never auto-commits, snapshots, or
  merges. Git is used by agents inside the shared workspace as usual, not as a
  cross-subagent transport.
- `read_parent`/`fork_parent`/`from_task`/`sources` modes are all gone; there is
  only "the workspace".

## Handoffs

There is no artifact table. A handoff is two things, both already available:

1. **The shared filesystem.** Code, evidence files, scratch output — whatever a
   subagent produced is simply present in the workspace for the parent and the
   next stage to read.
2. **The subagent's terminal result** — a small structured object the subagent
   emits when it finishes: `{ status, summary, suggested_next? }`. This is the
   "last assistant message" the parent receives in the completion notification.
   `suggested_next` is typed against the workflow template's declared outcomes,
   not free text, so the parent's branching is over a known set rather than
   prose.

Anything durable that is not code (e.g. Kubernetes logs/events) is written to a
file in the workspace by the subagent and/or summarized in its terminal result.
There is no separate evidence store. See Open Question 5.

## Minimal durable schema

The durable footprint is intentionally tiny. Reuse `sessions` and
`sessions.parent_session_id`; add one table and two columns.

### `stages`

```text
stages
  id text primary key
  parent_session_id text not null references sessions(id) on delete cascade
  workflow_id text null           -- template the parent was following, if any
  label text null                 -- e.g. "explore auth options"
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

That is the whole schema delta. No `runs`, `tasks`, `artifacts`, workspace, or
lease tables. A "run" is just a parent session and its ordered stages; the run
board renders parent session -> stages -> subagents (+ their terminal results).

## Stage runner

The runner is small and does exactly one job with the same rigor `agent-store`
applies to sessions: **detect stage completion and notify the parent, exactly
once.**

```rust
async fn on_subagent_terminal(stage_id: &str) -> Result<()> {
    let mut tx = store.begin().await?;
    let stage = store.lock_stage(&mut tx, stage_id).await?;     // select ... for update
    if stage.status != Running { return tx.commit().await; }    // already handled
    let subs = store.subagents_for_stage(&mut tx, stage_id).await?;
    if subs.iter().any(|s| !s.is_terminal()) { return tx.commit().await; }

    // single-flight: only the transition that flips running->done enqueues.
    store.set_stage_status(&mut tx, stage_id, Done, stage.attempt_id).await?; // CAS on attempt_id
    let notice = compose_completion_notice(&stage, &subs);       // results + session links
    store.enqueue_parent_input(&mut tx, &stage.parent_session_id, notice).await?;
    tx.commit().await?;          // then drive the parent outside the lock
    Ok(())
}
```

Properties, all reusing existing patterns:

- **Single-flight per stage** via the stage row lock, mirroring the per-session
  row lock.
- **Idempotent**: re-delivery of a subagent lifecycle event re-runs the check; a
  stage already `Done` short-circuits, so the parent is notified once.
- **Attempt-fenced**: the `running -> done` transition is a compare-and-set on
  `attempt_id`, so a stale completion cannot re-fire a notification.
- **Crash-safe**: on daemon restart, a sweep re-evaluates `running` stages whose
  subagents are all terminal and delivers the missed notification.

The runner never decides the next stage. That is the parent's job (inv. 3).

## Tools

Orchestration tools (parent / top-level agent):

```text
stage.start_full         -> { role, prompt }            ; one full subagent
stage.start_readonly_fanout -> { tasks: [{role, prompt}, ...] }  ; N read-only subagents
stage.status             -> inspect a stage and its subagents
stage.cancel             -> cancel an in-flight stage (all its subagents)
workflow.list            -> list templates
workflow.describe        -> a template's suggested stages
```

Steering/interrupt of the single full subagent reuses the existing subagent
steer/interrupt path. Read-only subagents reject steer/interrupt by type.

The parent is told, in its system prompt: launch at most one stage per turn,
then end your turn and wait for the completion notification; never poll, never
start a second stage while one is running, and never mix full and read-only work
in one stage.

The Python REPL remains only as a raw escape hatch; `subagents.wait(...)`
busy-waiting is no longer the orchestration path. See Migration.

## Human-in-the-loop

Human interaction is lighter here because the parent session is the user's
session:

- The parent asks the user directly when it needs a decision.
- A full subagent that needs the human ends with
  `suggested_next = human_needed`; that surfaces in the completion notification,
  and the parent relays it to the user.
- While a stage is in flight the parent is parked. User input sent in the
  meantime queues (existing queue semantics) and is seen when the parent next
  resumes; to abort early the user cancels the stage. (Confirm in Open
  Question 4.)

No blocked-run table, no signal artifact, no separate human-request store.

## Testing

The subsystem must be testable without real model calls, reusing the dev harness
that already resolves model actions deterministically (`harness.model.complete`
/ `harness.model.fail`).

- **Stage completion / notification (real Postgres).** Drive subagents to
  terminal states via the harness and assert the parent receives exactly one
  notification; assert re-delivered lifecycle events and restart sweeps do not
  double-notify; assert a stale `attempt_id` cannot re-fire. Inspect both the
  stage/session rows and the emitted events.
- **Homogeneity enforcement.** `stage.start_*` rejects mixing full and read-only
  in one stage, and rejects a second stage while one is running.
- **Read-only enforcement.** A read-only subagent attempting to write the
  workspace fails (mechanism per Open Question 1) — this is a correctness test,
  not a nicety.
- **Parking / resume.** A parent that launches a stage goes idle and is re-driven
  only by the completion notification.
- **Typed outcomes.** A `suggested_next` outside the template's declared outcome
  set is recorded as a subagent error, not silently matched.

## Migration and coexistence

| Surface | Today | Steady state |
| --- | --- | --- |
| `stage.*` + `workflow.*` templates | new | the way to run multi-step and parallel-read work |
| Python REPL `subagents.*` (busy-wait) | current primary API | demoted to a raw escape hatch; no longer documented for orchestration |

Sequence:

1. Ship typed subagents, stages, and notifications (Phases 0–3).
2. Repoint the `PI.md` "Subagent delegation" guidance at `stage.*` and the
   park-and-wait pattern. Remove the eager `subagents.spawn/wait` guidance.
3. Keep the Python REPL available but stop teaching `subagents.wait` as the way
   to orchestrate. Removing it entirely is a later follow-up.

## Implementation phases

### Phase 0: lifecycle foundation

- Land PR #150 (parent-visible child lifecycle events) on `main`; its tip is not
  yet there.
- Keep using `sessions.parent_session_id`.
- Update `architecture.md` to retire the "no subagent orchestration" non-goal.

### Phase 1: typed subagents on the shared filesystem

- Add `subagent_type` (full | read_only) to sessions.
- Make subagents run in the parent's workspace directories; **remove subagent
  workspace forking** (the btrfs/reflink fork-for-subagents path).
- Implement read-only enforcement (Open Question 1).

### Phase 2: stages and the homogeneity rule

- Add the `stages` table and `stage_id` on sessions.
- Add `stage.start_full`, `stage.start_readonly_fanout`, `stage.status`,
  `stage.cancel`.
- Enforce inv. 1 (homogeneous stage) and "one stage at a time per parent".

### Phase 3: notifications and parking

- Implement the stage runner: single-flight completion detection, completion
  notification enqueued to the parent, attempt-fenced, crash-recovery sweep.
- Add the system-prompt instructions for park-and-wait.
- Deterministic tests for completion/notification idempotency.

### Phase 4: workflow templates

- Add `workflow.list` / `workflow.describe` over bundled static templates.
- Ship `explore`, `implement_review`, `implement_review_test`, `kubernetes_e2e`.
- Type each template's outcomes and validate `suggested_next`.

### Phase 5: UI

- Run board: parent session -> stages -> subagents with terminal results.
- Show in-flight stage, per-subagent status, and pending human relays.
- Controls: cancel stage, steer the full subagent, re-run a stage.

## Open questions

1. **How is read-only enforced?** This is the linchpin of correctness: parallel
   read-only subagents share the filesystem, so a write by one corrupts all.
   Options, lightest to strongest:
   - *Advisory only* — read-only agents get no `edit` tool and are told not to
     write; `bash` can still write, so a buggy agent can corrupt the run.
     Simplest, weakest.
   - *Read-only workspace mount* — the workspace is bind-mounted read-only for
     read-only subagents' tool execution (scratch/tmp stays writable). Honors
     "same filesystem, no snapshot/subvolume" while actually preventing writes.
     My recommended default.
   - *OS user / perms* — run read-only subagents as a uid lacking write
     permission on the workspace. Strong but host-specific.
   A consequence of any real enforcement: read-only agents **cannot build or run
   tests** (those write `target/`, caches). So "build/test" is always a full
   stage. Confirm that rule.

2. **Is dropping parallel writes acceptable?** This model removes
   `parallel_race` and any N-parallel-implementer pattern. The substitute is
   parallel read-only exploration feeding a single full implementation stage. Is
   that an acceptable permanent limitation, or do we need an isolated-parallel
   escape hatch later (which would reintroduce snapshots)?

3. **How deterministic/replayable must runs be?** Parent-driven sequencing is
   less replayable than a compiled `advance()` state machine: the "next stage"
   is an LLM decision, not a pure function. Durable stage records make it
   *recoverable*, but not *deterministic*. For a personal runtime this is likely
   fine; confirm we are not giving up a replay/repro property we want.

4. **What happens to user input during an active stage?** Default proposal: the
   parent stays parked and user follow-ups queue until the stage completes; the
   user cancels the stage to intervene early. Alternative: user input
   auto-cancels or pauses the stage. Which is the right default?

5. **Where does non-code evidence live?** With no artifact table, durable
   non-code output (k8s logs/events, profiling) is either committed as files in
   the workspace or summarized in the subagent's terminal result. Is that
   sufficient, or do we want a tiny optional evidence blob keyed to a subagent?

6. **What does "re-run a stage" mean without snapshots?** Re-running does not
   roll back the filesystem — the previous stage's writes persist. A clean
   re-run requires the parent or user to `git reset`/`stash` first. Is
   forward-only re-execution acceptable, or do we need a cheap rollback (which
   pulls snapshots back in)?

7. **Can full subagents orchestrate their own stages (recursion)?** Deferred in
   v1 to keep the one-writer argument one level deep. If we later allow it, the
   parking discipline must hold recursively (a full subagent that launches a
   sub-stage must itself park). Worth deciding before the prompt/tooling
   hardcodes "only the top-level parent orchestrates".

8. **Notification size and shape.** What exactly goes in the completion notice —
   only `summary` + `suggested_next` + session links, or also a bounded slice of
   each subagent's output? How is it truncated for a large fan-out so it does not
   blow the parent's context?

9. **Failure policy for a fan-out.** If 3 of 5 read-only subagents fail, does the
   stage report `done` with partial results, or `failed`? Proposed: always
   deliver partial results and let the parent decide; the stage status reflects
   "completed with failures". Confirm.

## Design rules

1. Serialize writes in time, not space: at most one writer at any instant.
2. Subagents share the parent filesystem. No snapshot, fork, or merge.
3. A stage is homogeneous: one full subagent, or many read-only subagents.
4. The parent never polls or busy-waits; it parks and is notified.
5. Read-only means *enforced* read-only, because reads are concurrent.
6. Handoffs are the shared filesystem plus the subagent's typed terminal result.
   There is no artifact store and no cross-subagent variable store.
7. Workflows are templates that suggest stages; the parent owns sequencing.
8. The daemon owns mechanism (types, homogeneity, notifications, durable stages);
   the model owns policy (which stage next, re-run, stop).
9. Subagents are non-recursive in v1.
10. Build/test work is a full stage, never a read-only fan-out.
