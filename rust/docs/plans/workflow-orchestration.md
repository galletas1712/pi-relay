# Subagent stages & workflows — implementation plan

> **Historical/superseded.** The stage/delegation implementation has landed and
> the live docs now describe current behavior. Treat this file as design history,
> not as an authoritative spec. Current workflow skills live in the top-level
> `workflows/` directory, and current subagent roles live in `subagent-roles/`.
> The draft `rust/docs/plans/workflow-skills/` copies referenced below have been
> removed after installation.

Status: **historical implementation plan.** The former gating prerequisite —
parent-visible child lifecycle events (PR #150) — has landed on `main`, so there
is no longer a blocking dependency.

## Handoff package

Everything a builder needs is under `rust/docs/plans/`:

| File | What it is | Status |
| --- | --- | --- |
| `workflow-orchestration.md` (this file) | Historical spec + build brief (Implementation guide, Appendices A/B). | superseded |
| `phase-0-doc-edits.md` | Exact `architecture.md` / `agent-daemon.md` edits, staged to apply **with** the delegation-tool landing. | superseded |
| top-level `workflows/workflow-*/SKILL.md` | The installed workflow-pattern skills. Historical references below to `workflow-skills/` were draft copies. | current location |

Build in phase order (below). The design is settled and the open questions are
all decided ("Decided defaults"); do not reopen them or the "Rejected options".
Read the **Implementation guide** for the verified code seams, the net-new pieces,
and where each lives. `subagent-source-ref-merge-plan.md` is retained only as
history (it documents the legacy `sources=` path that this design removes).

## Summary

Subagent orchestration with the smallest possible runtime:

- There is **one durable workspace** — the parent's set of workspace dirs under
  the cwd root. It has **at most one writer**: the parent, or the single **full**
  subagent of a full stage. They are never concurrent because the parent parks
  while a stage runs. The full subagent writes those dirs **in place**; its edits
  are simply the parent's state when it finishes. Nothing is merged, adopted, or
  rolled back.
- **read-only (RO)** subagents never write the durable workspace. Each runs in its
  own **disposable btrfs snapshot** of the workspace dirs (reflink/copy fallback).
  It may build/run/test inside that snapshot, but the snapshot is **destroyed when
  the subagent returns**. RO is "read-only with respect to the parent
  filesystem", not literally non-writing.
- Work happens in **stages**: either *one full subagent* or *a parallel fan-out of
  RO subagents* — never both, and never multiple full (writing) subagents.
- Results propagate through a canonical structured delegation snapshot plus a
  **handoff directory** of per-subagent files (`final_message.md`,
  `transcript.md`, and `task_prompt.md` when available), not through model
  context. The parent **does not busy-wait**: after launching a stage it parks,
  and the daemon delivers a typed daemon-authored **wakeup observation**
  containing an `inspect_delegation`-equivalent bounded snapshot when the stage
  completes. The parent stays responsive to the user the whole time.
- A **workflow** is a **skill** (`SKILL.md`) describing a possibly-cyclic state
  machine of stages. The parent loads it with `LoadSkill` and drives it with
  discretion, branching on subagents' typed outcomes. There is no workflow tool
  surface and no daemon-executed graph.

### The two roles

| | read-only (RO, ephemeral) | full (durable) |
| --- | --- | --- |
| Workspace | own disposable snapshot | the parent's dirs, in place |
| May write? | yes, only inside its snapshot | yes, the real workspace |
| On return | snapshot destroyed | edits persist as the parent's state |
| Durable file output | none (snapshot is gone) | yes, in the workspace |
| Result to parent | final message + transcript via handoff dir | same, plus its in-place edits |
| Steerable/interruptible? | steerable while active; whole stage cancellable | steerable while active; whole stage cancellable |
| Per stage | many, in parallel | exactly one |

### What makes this safe and simple

> RO subagents are isolated in throwaway snapshots, so any number run in parallel
> without touching the durable workspace. The durable workspace has a single
> writer in time — the parent, or the one full subagent — with no merge, no
> adoption, and no conflict resolution anywhere.

The single-writer guarantee is exact for RO stages (RO never touches the durable
workspace). For a **full** stage it rests on a soft rule: the parent stays
responsive to the user but its prompt tells it the full subagent owns the
workspace, so it supervises/reads/plans rather than editing until the subagent
returns. We do not hard-enforce that (consistent with the runtime not policing
tool behavior elsewhere).

## The three invariants

1. **A stage is homogeneous.** One full subagent, or a fan-out of RO subagents —
   never a mix, never more than one full. This keeps the durable workspace
   single-writer.
2. **The parent never busy-waits, but it is not frozen.** After launching a stage
   the parent parks (goes idle; it does not spin or poll). It stays a normal
   interactive session: user input drives it to make progress while the stage
   runs. Stage completion is delivered as a high-priority typed daemon wakeup
   observation (seen promptly, even mid-turn), not a follow-up. During a full
   stage the parent defers workspace edits to the full subagent (soft rule).
3. **Workflows are skills the parent interprets, not a daemon DSL.** A workflow is
   a `SKILL.md` describing a (possibly cyclic) state machine of stages; the parent
   follows it with discretion, branching on typed subagent outcomes. The daemon
   supplies mechanism (typed subagents, snapshots, the handoff dir, wakeup
   observations, durable stage records); the parent supplies policy.

## Relationship to the stated architecture

`architecture.md` already supports subagent delegation (Goal 6: "bounded
parent/child subagent delegation … without a generic injected-message routing
layer or event bus") and documents the current shape: a parent spawns forked
child sessions by role/skill (optionally with a forked-context snapshot and git
source-refs), then lists/waits/reads/steers/interrupts them, with
`subagent.{spawned,running,idle}` lifecycle events on the wire and spawn/control
flowing through the in-process Python REPL `subagents` module.

So this plan **evolves documented behavior; it does not introduce orchestration
into a void.** It changes three things the architecture currently describes, and
those doc updates ship with the implementation:

- **Control surface:** Python REPL `subagents.*` (busy-wait) → provider-visible
  delegation tool calls + daemon wakeup observations (no busy-wait).
- **Workspace handoff:** forked-context snapshots and git source-refs between
  subagents → one durable workspace (full writes in place; RO disposable
  snapshots) with file-based handoff; no merge, no source-refs.
- **Patterns:** ad hoc delegation → named workflow **skills** the parent drives.

The architecture's bounded-parent/child model and "no generic event bus" stance
are preserved: stages are still parent/child forks, and the only cross-session
signal is the existing parent-scoped lifecycle event the barrier consumes.

### This is the third orchestration attempt; do not repeat the first two

1. **`agent-orchestrator` crate + `SessionRegistry` (removed).** Control flow
   lived in a process-local object graph. *Lesson: durable Postgres state is the
   source of truth.*
2. **`workflow_variables` + `work.*` RPCs + Python workflow SDK (deleted).**
   Control flow polled named variables and lived in editable Python templates.
   *Lesson: no general variable store, no model-authored control-flow scripts, no
   daemon-executed workflow graph.*
3. **Python REPL `subagents.*` (current, shipped).** The parent **busy-waits**
   (`subagents.wait`). *Lesson: park and be notified, never spin.*

## Subagent types

### Read-only (RO, ephemeral)

- Runs in its own disposable snapshot of the workspace dirs. **May write inside
  its snapshot** (so it can build, run, and test), but the snapshot is destroyed
  when it returns.
- **Returns no files.** Its durable output is its final message plus its full
  transcript, both captured to the handoff directory by the daemon (rendered from
  the session transcript in Postgres — capture does not depend on the snapshot
  still existing).
- **Can be steered while active** through the same subagent path as full
  subagents; terminal/idle RO targets are rejected. Read-only means isolated from
  the parent workspace, not immutable conversation state. A whole RO stage can be
  cancelled.
- Many run in parallel in one stage, each isolated.

### Full (durable)

- Writes the parent's workspace **in place** — no snapshot, no adoption. Its edits
  are the parent's state as soon as it finishes; it may leave files behind and
  reference them.
- Exactly one per stage; it is the durable workspace's writer for that stage.
- **Can be steered and interrupted** by the human and, where useful, the parent.
- Its final message and transcript are also captured to the handoff directory.

### Non-recursive

**Subagents cannot spawn subagents.** Hard rule. Only the top-level parent
orchestrates stages. If a workflow needs decomposition, that is another stage the
parent runs.

### Fresh context

A subagent's context is **fresh**, not a fork of the parent's transcript. It
receives only its scoped task prompt: role `SKILL.md`, the task the parent wrote,
and the handoff/workspace paths it should read. The parent is the context router:
it decides what each subagent needs and puts it in the prompt. This is a
deliberate change from the REPL `fork_context` flag, which is **not** carried into
the stage model.

## The handoff directory

Subagent results reach the parent through files, not model context, so context
stays bounded no matter how large a fan-out or transcript is.

- The cwd root is not itself a workspace; the workspace dirs live under it. The
  daemon owns one more directory under the cwd root — the **handoff directory**
  (e.g. `<cwd>/.pi-handoff/`). It is not a workspace: never forked, snapshotted,
  or part of any git repo.
- On delegation completion, for **every** subagent (success or failure), the daemon
  writes per-subagent files; the structured control-flow snapshot comes from
  `inspect_delegation`:

  ```text
  <cwd>/.pi-handoff/<delegation_id>/<subagent>/final_message.md
  <cwd>/.pi-handoff/<delegation_id>/<subagent>/transcript.md
  ```

  All are rendered from the durable transcript, so they exist even after an RO
  snapshot is gone and even when the subagent crashed.
- The parent receives the same compact structured snapshot in the completion
  wakeup observation and can refresh it later with **`inspect_delegation`**. It
  includes per-subagent status, `outcome`, and compact handoff file
  references so the parent branches without parsing prose or reading files
  first.
- The daemon then delivers the notification by enqueuing a typed
  `daemon_tool_observation` for the parent. It names the delegation, says how
  many succeeded/failed, and carries the bounded snapshot JSON. It does **not**
  inline full transcripts or full final-message bodies:

  ```text
  Delegation delegation_... (reviewer fan-out) completed: 3 ok, 1 failed.
  Snapshot JSON (equivalent to inspect_delegation at wakeup time):
  {
    ...
    "final_message_file": "child/final_message.md",
    "outcome": "approved",
    "transcript_file": "child/transcript.md"
  }
  ```

- The parent branches on the delivered snapshot first, then reads
  `final_message.md` or opens `transcript.md` only when it needs more detail —
  with its normal file tools.
  For a full delegation, the full subagent's actual edits are already in the
  workspace.

The handoff directory is **never cleaned up automatically**; its files double as
durable run history. There is no `artifacts` table and no structured artifact API
in v1; the handoff directory plus the durable workspace are the entire handoff
surface.

## Stages

A **stage** is `kind = full` (one full subagent) or `kind = readonly_fanout` (one
or more RO subagents). Lifecycle:

```text
parent calls delegate_writing_task / delegate_readonly_tasks
  -> full stage: start one subagent in the parent's workspace dirs (in place)
     RO stage: snapshot the workspace dirs per subagent; start each in its snapshot
  -> parent parks (idle) but stays responsive to the user
  -> subagents run; as each RO subagent returns, destroy its snapshot
  -> after the expected fan-out count exists, the daemon may enqueue one active
     partial typed wakeup for a terminal child while siblings still run
  -> parent (woken by a running snapshot) steers a running/steerable child,
     cancels the delegation, or waits; it does not start unrelated work
  -> once every subagent is terminal, the daemon cancels stale queued partials,
     writes final_message.md + transcript.md for every subagent, then enqueues
     ONE terminal typed wakeup observation to the parent with the structured snapshot
  -> parent (woken by a terminal snapshot) branches on the outcome/status and decides
     the next fresh stage, done, or whether to read artifact files for more detail
```

A stage is terminal when all its subagents are terminal
(`done`/`failed`/`cancelled`/`crashed`). Partial fan-out wakeups are serialized
parent decision points: there is at most one active queued/consuming partial per
delegation attempt, and consuming one may publish the next already-terminal
sibling. The parent may cancel an in-flight stage; it can steer active subagents
while terminal/idle targets are rejected.

## Snapshots and recovery

- **Snapshots are for RO fan-out only**, so parallel RO subagents get a private,
  stable view and can build/test without touching the durable workspace or each
  other.
- **GC on return.** An RO subagent's snapshot is destroyed when it returns;
  snapshots never accumulate. The cost (no file output from RO) is by design.
- **The full subagent uses no snapshot**; it writes the durable workspace in place.
- **Recovery is "continue where we left off".** Sessions are durable, resumable,
  and repairable (the runtime recovers crashed tails to turn boundaries and
  supports steer/interrupt/continue). A subagent or parent that crashes resumes
  from its recovered state. We do **not** use git to recover and we do **not**
  roll back; a full subagent's partial in-place edits remain and it continues.
- **Rollback is deferred** (future: snapshot the durable workspace before a full
  stage to allow discarding its changes).

## Workflows are skills, not a DSL

Real workflows are **cyclic**: e.g. implementer and reviewer loop until the
reviewer is satisfied, then a tester runs; bugs send it back to the implementer
and the loop restarts. The question is who executes that control flow.

We choose **the parent, guided by a skill** — not a daemon-interpreted DSL. A DSL
would re-grow the `advance()` state machine + `workflow_variables` store deleted
twice, is brittle against messy real conditions, and contradicts invariant 3.

A workflow is a **skill** (`SKILL.md`) discovered in the skills index and loaded
with `LoadSkill` (the subagent roles are already skills). It documents a
graph-shaped state machine the parent follows with judgment, driving it with
delegation tool calls. There is **no `workflow.*` tool surface**; stages carry
an optional `workflow` label for internal correlation/ordering, not for a
visible product heading.

**Soft control flow, hard signals.** The skill reads like a state machine, but it
is parent-interpreted. What keeps it crisp is the **typed `outcome`
outcomes** each subagent reports in the `inspect_delegation` snapshot (the reviewer returns
`approved | changes_requested`; the tester returns
`pass | bugs_found | environment_issue`) — these are the edge labels the parent
branches on.

### Example: the `implement_review_test` workflow skill

```markdown
# Workflow skill: implement -> review -> test

Use when a change should be implemented, reviewed until a reviewer is satisfied,
then tested, looping back on failures. You drive this loop; branch on the typed
outcomes each subagent reports in `inspect_delegation`.

## Stages
- implementer - full subagent (writes the workspace in place)
- reviewer    - read-only subagent(s) (review only; never write)
- tester      - read-only subagent(s) (runs the suite in a disposable snapshot;
  build/test artifacts do not reach the parent workspace)

## Outcomes (outcome, in inspect_delegation)
- reviewer: approved | changes_requested
- tester:   pass | bugs_found | environment_issue

## Control flow
1. implement
2. review
   - changes_requested -> implement again (pass the reviewer notes) -> 2
   - approved          -> test
3. test
   - pass              -> DONE
   - bugs_found        -> implement again (pass the failure detail) -> 2
   - environment_issue -> re-run test once; if it recurs, ask the human
4. Termination: if review has not converged after ~3 rounds, stop and ask the
   human.

## Running each stage (one stage per turn, then end your turn)
- implement: delegate_writing_task({ role:"implementer",
    prompt:<goal + latest review/test notes>, workflow:"implement_review_test" })
- review:    delegate_readonly_tasks({ tasks:[{role:"reviewer",
    prompt:<what to review + acceptance criteria>}], workflow:"implement_review_test" })
- test:      delegate_readonly_tasks({ tasks:[{role:"tester",
    prompt:<how to test>}], workflow:"implement_review_test" })

When the completion observation arrives, branch on the delivered snapshot. Read the
relevant `final_message.md` only if you need more detail, and call
`inspect_delegation` only to refresh/recover state or inspect later/running.
Subagents start fresh, so carry the prior stage's findings into the next stage's
prompt.
```

Gates are **not** hard-enforced; mitigations are the skill's termination rules,
the human watching the Agents outline, and the typed outcomes. If a single critical
gate ever needs enforcement (e.g. "cannot finish without a tester `pass`"), add a
targeted check — not a graph engine.

Bundled workflow skills to ship first: `explore`, `implement_review`,
`implement_review_test`, `kubernetes_e2e`. There is no `parallel_race` (it needs
parallel writers).

## Durable schema

Reuse `sessions`, `sessions.parent_session_id`, and the existing subagent
workspace-fork metadata. Add one table and two columns. The handoff directory is
derived from the session cwd; it needs no schema.

### `stages`

```text
stages
  id text primary key
  parent_session_id text not null references sessions(id) on delete cascade
  workflow text null              -- workflow skill the parent was following (label only)
  label text null
  kind text not null              -- full | readonly_fanout
  status text not null            -- running | done | done_with_failures | cancelled | failed
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

Single-purpose: **detect the stage barrier, write the handoff files, and publish
one parent wakeup observation**, with the same rigor `agent-store` applies to
sessions.

```text
on a subagent of stage S reaching a terminal lifecycle state:
  lock stage S (row lock)                       # single-flight
  if S.status != running: return                # already handled
  if any subagent of S not terminal: return     # barrier not met
  set S.status = done|done_with_failures         # CAS on attempt_id
  render handoff files for every subagent of S   # per-subagent md
  enqueue a typed daemon observation to S.parent_session_id containing the structured snapshot
  commit; then drive the parent
```

Properties (reusing existing patterns): single-flight per stage via the stage row
lock; idempotent (a non-`running` stage short-circuits, so the parent wakeup is
published once); attempt-fenced; crash-safe via a startup sweep over `running` stages whose
subagents are all terminal. RO snapshots are reclaimed per subagent as each
returns (independent of the barrier); handoff files render from the durable
transcript, so they never depend on a snapshot. The runner never decides the next
stage — that is the parent's job.

## Tools

```text
delegate_writing_task            -> { role, prompt, workflow?, label? }   ; one full subagent (in place)
delegate_readonly_tasks -> { tasks:[{role,prompt}], workflow?, label? } ; N RO subagents (snapshots)
inspect_delegation                -> inspect a stage and its subagents
cancel_delegation                -> cancel an in-flight stage
```

Full schemas in Appendix A. Workflows have **no tools**: they are skills loaded
with the existing `LoadSkill`. Steer/interrupt targets use the existing subagent
path for active subagents; terminal/idle targets are rejected. RO subagents remain
read-only with respect to the parent filesystem because they run in disposable
workspace snapshots.

System-prompt rules for the parent (Appendix B): launch at most one stage per
turn then end your turn; never poll; never start a second stage while one runs;
never mix full and RO in one stage; while a full subagent runs, supervise and read
but do not edit; on a completion observation, branch on the delivered snapshot first.

The `PythonRepl` tool remains a raw escape hatch only; it is no longer the
orchestration surface.

## Human-in-the-loop

The parent session is the user's session and stays responsive: the user can steer
the parent while a stage runs, and the parent asks the user directly when it needs
a decision. A full subagent that needs the human ends with
`outcome = human_needed`, which appears in its handoff `final_message.md`
and the wakeup observation; the parent relays it. To intervene in a running stage the user
cancels it or steers an active subagent. No blocked-run table, no signal artifact.

## Migration off the Python REPL

This plan **moves subagent invocation off the Python REPL onto provider-visible
delegation tool calls.** Today `PI.md` teaches orchestration via the
`PythonRepl` tool's `subagents.*` host functions, which busy-wait in a
long-lived Python process. The stage model replaces that with ordinary tool
calls (`delegate_writing_task`, `delegate_readonly_tasks`,
`inspect_delegation`, `cancel_delegation`) plus daemon-driven wakeup observations.

| Surface | Today | Steady state |
| --- | --- | --- |
| delegation tools | none yet | the way to run staged and parallel-RO work |
| workflow skills (`SKILL.md` + `LoadSkill`) | roles exist as skills | named, possibly-cyclic stage playbooks |
| Python REPL `subagents.*` (busy-wait) | current primary path in `PI.md` | raw escape hatch only; removed from `PI.md` |
| `subagent.spawn` / `subagent.list` RPCs | low-level, REPL-backed | reused by / folded into the stage engine |

Sequence: ship delegation tools + stages + handoff/wakeup observation (Phases 0–3); rewrite
the `PI.md` "Subagent delegation" section to teach delegation tools, workflow
skills, the handoff dir, fresh-context prompts, and park-but-stay-responsive,
deleting the `subagents.spawn/wait/...` guidance. Fully retiring the REPL
`subagents.*` host functions is a later cleanup.

## Implementation guide

This is the build brief: what exists, what is new, and the exact seams. Verified
against the tree at the time of writing.

### Prerequisite (satisfied)

The former hard gate — parent-visible child lifecycle events
(`subagent.{spawned,running,idle}`, stale recovery) from PR #150 — **has landed on
`main`** (commit `00f14f9`, "Improve subagent orchestration lifecycle (#150)").
The stage runner's barrier subscribes to those events. There is no remaining
blocking dependency; implementation can start immediately.

### Code seams to build on (verified to exist)

| Need | Use | Location |
| --- | --- | --- |
| Deliver the completion observation to the parent | enqueue a typed daemon observation at `InputPriority::Steer` | `agent-daemon/src/delegation_runner.rs` / `agent-store/src/postgres/delegations.rs` |
| Create a child subagent session | `spawn_subagent` / `subagent_spawn_from_active_parent` | `agent-daemon/src/subagents.rs` |
| Load a role's `SKILL.md` for a subagent | `resolve_skill_role` | `agent-daemon/src/subagents.rs` |
| Fork workspace dirs (for RO snapshots) | `WorkspaceManager::fork_session_from_parent` (btrfs/reflink/copy under the hood in `instantiate.rs`) | `agent-daemon/src/workspaces/mod.rs` (~150) |
| Tear down a session's workspace dirs | `WorkspaceManager::remove_session_dir` (exists, `remove_dir_all`) — extend for RO GC (see net-new #2) | `agent-daemon/src/workspaces/mod.rs` (~260) |
| Per-stage single-flight + recovery | `SessionDriver::acquire`, the `attempt_id`/CAS fence pattern used for `actions` | `agent-daemon/src/runtime/mod.rs`; `agent-store` |
| Render handoff files from a child transcript | `active_branch` / transcript reads (UI body mode: messages + tool calls/results) | `agent-store`; see `repl.rs::parent_context_block` for a render example |
| Subagent steer/interrupt + lifecycle events | existing subagent control RPCs + PR #150 events | `agent-daemon/src/subagents.rs`, `runtime/`, `agent-store::session_links` |

### Net-new pieces to write

1. **Skip the fork for full subagents.** Today every subagent forks
   (`fork_session_from_parent` in `spawn_subagent`). For `subagent_type = full`,
   skip the fork and run the child against the parent's `outer_cwd`/`workspaces`
   directly (in place). For `read_only`, keep the fork (it is the disposable
   snapshot).
2. **Destroy an RO snapshot on return.** A session-dir teardown exists
   (`remove_session_dir`, `remove_dir_all`) but is only invoked on session delete,
   and `remove_dir_all` does **not** reclaim the btrfs *subvolumes* that
   `instantiate.rs` creates — those need `btrfs subvolume delete` (with a
   reflink/copy `rm -rf` fallback). Add a workspace-destroy path that handles
   subvolumes and call it when an RO subagent reaches a terminal lifecycle state.
3. **`stages` table + repo methods** (create/lock/finish, list by parent, sweep
   `running`).
4. **Delegation tools** (Appendix A) + homogeneity/single-stage guards.
5. **The handoff writer** (per-subagent `final_message.md` /
   `transcript.md`) under `<outer_cwd>/.pi-handoff/<stage_id>/`.
6. **The barrier→wakeup-observation hook** in the subagent-lifecycle path (the runner above).
7. **Workflow skills** as `SKILL.md` files (data, no daemon change) and the
   rewritten `PI.md` block.

### Decided defaults (do not reopen)

- Concurrent parent + full-subagent writes → **soft prompt rule**, not hard
  enforcement.
- Handoff directory cleanup → **none** (durable history; cascade-on-delete is a
  possible later add).
- Handoff format → **per-subagent `final_message.md` + `transcript.md`**, with
  `inspect_delegation` as the structured snapshot/control-flow artifact.
- Subagent context → **fresh** (no `fork_context`).
- Multi-dir snapshots → fork **all** of a subagent's workspace subdirs together
  (the existing fork already handles multiple workspaces).
- Handoff dir visibility → it is a **sibling of the workspace dirs** under
  `outer_cwd`, so it is outside every workspace git repo and every RO snapshot by
  construction; the parent's file tools read it by absolute path. Add it to any
  defensive ignore set so a full subagent never stages it.
- `transcript.md` fidelity → render **messages + tool calls + tool results**
  (UI body mode), greppable markdown.
- Fan-out failures → **block until all terminal; one notification; failures
  included** with their full transcripts; stage status `done_with_failures`.
- Retry → **continue/repair the existing session**, never a fresh restart.

### Rejected options — do not re-derive

No daemon-executed workflow DSL/graph; no snapshot adoption/swap; no `git merge`,
source refs, or multi-parent lineage; no parallel writers / `parallel_race`; no
rollback in v1; no `artifacts` table or variable store; no `fork_context`; no
busy-wait/poll loop.

## Implementation phases

### Phase 0 — docs alignment

- PR #150 (lifecycle events) is already on `main`; no code prerequisite remains.
- Apply the staged doc edits in `phase-0-doc-edits.md` (architecture.md goals +
  subagent-delegation bullet + not-implemented list; agent-daemon.md runtime
  summary) **in the same change that lands delegation tools**, so the docs never
  describe unbuilt behavior.

### Phase 1 — typed subagents

- Add `subagent_type` (full | read_only) to sessions.
- Full subagents run in the parent's workspace dirs in place (skip the fork).
- RO subagents keep the forked snapshot; add `destroy_session_workspaces` and
  destroy on RO return.

### Phase 2 — stages

- Add the `stages` table + repo methods + `stage_id` on sessions.
- Add `delegate_writing_task`, `delegate_readonly_tasks`, `inspect_delegation`,
  `cancel_delegation`; enforce homogeneity, a single full subagent, and
  one-stage-at-a-time per parent.

### Phase 3 — handoff + barrier + wakeup observation

- Handoff writer (per-subagent files from the durable transcript).
- Stage runner: barrier, single-flight completion, one typed daemon wakeup
  observation to the parent, attempt fencing, crash-recovery sweep.
- `PI.md` park-but-stay-responsive rules; deterministic idempotency tests.

### Phase 4 — workflow skills

- Install the drafted skills in `workflow-skills/` (see its `README.md`): add a
  `load_global_skills_from_dir(&prompt_root.join("workflows"))` call next to the
  existing `subagent-roles` scan in
  `agent-daemon/src/provider_runtime/skills.rs`, and copy each
  `workflow-skills/<name>/SKILL.md` to `workflows/<name>/SKILL.md` at the prompt
  root. Ships: `workflow-explore`, `workflow-implement-review`,
  `workflow-implement-review-test`, `workflow-kubernetes-e2e`.
- Replace the `PI.md` "Subagent delegation" section with Appendix B.

### Phase 5 — UI

- Agents outline: a flat list of task/role labels led by shape-distinct,
  accessibly named status icons. Do not render group headings, status/outcome
  phrases, progress/counts, or handoff links in ambient rows; open Handoffs or
  Debug Inspector explicitly for detail. Keep scoped cancellation. Delegated
  work has no restart control.

## Testing

Reuse the dev harness that resolves model actions deterministically
(`harness.model.complete` / `harness.model.fail`).

- **Barrier + handoff + wakeup observation (real Postgres):** drive subagents to terminal
  states; assert one typed wakeup observation only after all are terminal; assert
  `final_message.md`/`transcript.md` exist for every subagent
  including failed ones; assert re-delivered events and restart sweeps do not
  double-publish; assert a stale `attempt_id` cannot re-fire.
- **High priority, not follow-up:** delivered through the steer-priority queue
  lane as a daemon observation, seen promptly (mid-turn after a tool batch if
  the parent is running).
- **In-place full writes:** a full subagent's edits are visible in the parent's
  workspace after the stage; full subagents do not fork.
- **RO isolation + GC:** an RO subagent's writes never reach the durable
  workspace; its snapshot is destroyed after return; its handoff files survive.
- **Homogeneity / single-flight:** the start-delegation tools reject mixed
  stages, more than one full subagent, and a second concurrent stage.
- **Continue-where-left-off:** a crashed/interrupted subagent resumes its session
  from the recovered turn boundary; no git, no rollback.
- **Typed outcomes:** an `outcome` outside a workflow skill's set is recorded
  but does not crash the parent's branching.

## Design rules

1. One durable workspace, single writer in time (parent or the one full
   subagent). RO never touches it.
2. The full subagent writes in place. No adoption, no merge, no rollback in v1.
3. RO subagents run in disposable snapshots, may build/test in them, and are GC'd
   on return; they return no files.
4. A stage is homogeneous: one full, or many RO. Never parallel writers.
5. The parent parks (no spin) but stays responsive; completion arrives as a
   typed daemon wakeup observation containing the structured snapshot.
6. Subagents cannot spawn subagents; subagent context is fresh.
7. Results propagate through the delivered snapshot plus the handoff directory
   (final message + transcript) and, for full stages, the durable workspace. No
   root `index.json`, no artifact store, no variable store, no context dump.
8. Recovery is "continue where we left off" — no git recovery, no rollback in v1.
9. Workflows are skills (`SKILL.md` + `LoadSkill`) describing a parent-interpreted,
   possibly-cyclic stage state machine. No DSL, no daemon graph, no `workflow.*`
   tools.
10. The daemon owns mechanism; the model owns policy (which fresh stage next or
    stop). There is no dedicated delegation rerun API.

## Appendix A: delegation tool schemas

Provider-visible function tools intercepted by the daemon runtime — not REPL
host functions. The separate web/inspector client RPCs keep their `stage.*`
method names.

```jsonc
// Launch the single full (writing) subagent. End your turn after calling.
{ "name": "delegate_writing_task",
  "input": { "role": "implementer", "prompt": "Implement X in place. ...",
             "workflow": "implement_review_test", "label": "implement X" },
  "result": { "stage_id": "stage_8", "subagent_session_id": "session_..." } }

// Launch N RO subagents in parallel, each in its own disposable snapshot.
// End your turn after calling.
{ "name": "delegate_readonly_tasks",
  "input": { "tasks": [ { "role": "reviewer", "prompt": "Review for correctness. ..." },
                         { "role": "reviewer", "prompt": "Review for security. ..." } ],
             "workflow": "implement_review_test", "label": "review fan-out" },
  "result": { "stage_id": "stage_9", "subagent_session_ids": ["session_...", "session_..."] } }

// Inspect a stage/delegation. This is the canonical structured snapshot.
{ "name": "inspect_delegation", "input": { "stage_id": "stage_9" },
  "result": { "stage_id": "stage_9", "kind": "readonly_fanout", "status": "running",
              "subagents": [ { "id": "...", "status": "running" } ],
              "handoff_dir": "<cwd>/.pi-handoff/stage_9" } }

// Cancel an in-flight stage (all its subagents).
{ "name": "cancel_delegation", "input": { "stage_id": "stage_9" }, "result": { "cancelled": true } }
```

Daemon-enforced errors: starting a second stage while one is running; mixing full
and RO in one stage; more than one full subagent; steering a terminal or idle
subagent.
Completion is **not** a tool result — it arrives later as a daemon-authored
wakeup observation containing an `inspect_delegation`-equivalent bounded snapshot
with handoff artifact paths. The
`workflow` field is an optional grouping label only.

## Appendix B: draft `PI.md` "Subagent delegation" block

Apply in Phase 4 (it references tools that do not exist yet, so it is not applied
to the live `PI.md` now).

```markdown
## Subagent delegation

Delegate work to subagents through stage tool calls. Do not use the Python REPL
to orchestrate subagents.

Two kinds of subagent:

- **read-only (RO)** — for investigation, review, analysis, and running
  builds/tests to gather information. RO subagents run in a private throwaway copy
  of the workspace; nothing they write reaches your workspace. Use
  `delegate_readonly_tasks` to run several in parallel.
- **full** — for making changes. A full subagent edits your workspace in place.
  Use `delegate_writing_task`. There is exactly one full subagent at a time.

Rules:

- Launch at most one stage per turn, then end your turn. Do not poll or loop —
  you will be notified.
- When a stage finishes you receive a short message containing a delegation
  snapshot equivalent to `inspect_delegation`. Branch on the delivered snapshot;
  call `inspect_delegation` only to refresh/recover state or inspect
  later/running. Read `final_message.md`/`transcript.md` artifacts only if you
  need detail.
- Give each subagent a self-contained task: it starts with fresh context and only
  knows what you put in its prompt (and any handoff/workspace paths you cite).
- While a full subagent is running, supervise and read — do not edit the workspace
  yourself until it returns.
- Never mix RO and full work in one stage.
- To run a known pattern (e.g. implement → review → test), `LoadSkill` the matching
  workflow skill and follow its stage state machine, branching on the typed
  outcomes in `inspect_delegation`, with your own judgment (skip, launch fresh
  work, escalate, stop).
- The `PythonRepl` tool remains only for ad hoc scripting, not for orchestrating
  subagents.
```
