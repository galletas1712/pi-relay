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
- A **workflow** is a **skill** (`SKILL.md`) documenting a recommended — possibly
  cyclic — state machine of stages. The parent loads it with `LoadSkill` and
  drives it with discretion; there is no workflow tool surface and no
  daemon-executed graph. The parent decides which stage to run, whether to
  re-run one, and when to stop.

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
3. **Workflows are skills the parent interprets, not a daemon DSL.** A workflow
   is a `SKILL.md` describing a (possibly cyclic) state machine of stages; the
   parent follows it with discretion, branching on typed subagent outcomes. The
   daemon supplies mechanism (typed subagents, snapshots, the handoff dir, steer
   notifications, durable stage
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

### Subagents start with fresh context

A subagent's context is **fresh**, not a fork of the parent's transcript. It
receives only its scoped task prompt: role `SKILL.md`, the task/goal the parent
wrote, the workflow stage hint, and the handoff paths of any prior stage it
should read. It is **not** seeded with the parent's conversation history.

The parent is the context router: it decides what each subagent needs to know and
puts that in the task prompt (and points at handoff files / workspace paths for
detail). This keeps subagent context small and on-task, avoids leaking the
parent's unrelated history, and keeps prompt-cache behavior predictable.

This is a deliberate change from the current REPL `subagents.spawn(fork_context=…)`
flag, which optionally dumps up to ~60KB of the parent's active branch into the
child. That flag is a REPL-era convenience and is **not** carried into the stage
model; if a subagent genuinely needs parent detail, the parent passes the
specific facts (or a handoff path) in the task prompt rather than forking the
whole transcript.

## The handoff directory

Subagent results reach the parent through files, not through model context, so
context stays bounded no matter how large a fan-out or a transcript is.

- The cwd root is not itself a workspace; the workspace dirs live under it. The
  daemon owns one more directory under the cwd root — the **handoff directory**
  (e.g. `<cwd>/.pi-handoff/`). It is not a workspace: it is never forked,
  snapshotted, or part of any git repo.
- On stage completion, for **every** subagent in the stage (success or failure),
  the daemon writes a per-subagent final message and full transcript, plus a
  per-stage **`index.json`** for easy navigation:

  ```text
  <cwd>/.pi-handoff/<stage_id>/index.json
  <cwd>/.pi-handoff/<stage_id>/<subagent>/final_message.md
  <cwd>/.pi-handoff/<stage_id>/<subagent>/transcript.md
  ```

  These are rendered from the subagent's durable transcript, so they exist even
  after an RO snapshot is gone and even when the subagent crashed.
- The **`index.json`** is the parent's entry point: a compact, machine-readable
  manifest of the stage so the parent can navigate without scanning the tree.

  ```json
  {
    "stage_id": "stage_7",
    "kind": "readonly_fanout",
    "workflow_id": "implement_review_test",
    "label": "reviewer fan-out",
    "status": "done_with_failures",
    "subagents": [
      { "id": "reviewer-a", "role": "reviewer", "status": "done",
        "suggested_next": "approve",
        "final_message": "reviewer-a/final_message.md",
        "transcript": "reviewer-a/transcript.md" },
      { "id": "reviewer-c", "role": "reviewer", "status": "failed",
        "suggested_next": null,
        "final_message": "reviewer-c/final_message.md",
        "transcript": "reviewer-c/transcript.md" }
    ]
  }
  ```

  Paths in `index.json` are relative to the stage directory. `transcript.md` is
  the human-readable render; `index.json` carries the structured per-subagent
  status/outcome so the parent can branch without parsing prose.
- The daemon then delivers the notification by enqueuing a **short steer** to the
  parent (which appears as a user message in the parent's transcript). It names
  the stage, says how many subagents succeeded/failed, and points at
  `index.json` — it does **not** inline the messages. Example:

  ```text
  Stage stage_7 (reviewer fan-out) finished: 3 ok, 1 failed.
  Read <cwd>/.pi-handoff/stage_7/index.json for the manifest, then the
  per-subagent final_message.md files. Failed: reviewer-c.
  ```

- The parent reads `index.json` first, then `final_message.md` files
  (summaries), and opens `transcript.md` only when it needs detail — using its
  normal file tools. For a full stage, the full subagent's actual edits are
  already in the workspace; the handoff files are its summary/transcript.

The handoff directory is **never cleaned up automatically** — its files double as
durable run history that the parent (and the human) can revisit. Deleting them is
out of scope for the daemon in v1; cascade-on-session-delete can be added later if
disk use ever matters.

There is no `artifacts` table and no structured artifact API in v1; the handoff
directory (with `index.json`) plus the durable workspace are the entire handoff
surface. A richer structured view can be layered on later if needed.

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

## Workflows are skills, not a DSL

Real workflows are **cyclic**, not linear lists. The motivating example —
implementer and reviewer loop until the reviewer is satisfied, then a tester
runs; bugs send it back to the implementer and the loop restarts — is a state
machine with gates and back-edges. The question is **who executes that control
flow.**

We choose **the parent, guided by a skill** — not a daemon-interpreted DSL.

### Why not a DSL

A DSL means the daemon interprets a graph, tracks the current node, and decides
transitions from subagent outcomes. That is exactly the `advance()` state machine
plus the `workflow_variables` store this project **built and deleted twice**. It
re-grows the daemon, needs a typed-outcome interpreter, is brittle against messy
real conditions ("reviewer is mostly happy with one nit — proceed but note it"),
and contradicts invariant 3 (the parent owns sequencing). We do not reintroduce
it.

### Workflows are skills

A workflow is a **skill** (`SKILL.md`) that documents a recommended state machine
in prose the parent follows with judgment, driving it with `stage.*` tool calls.
This reuses the existing skill system (`SKILL.md`, `LoadSkill`, the skills index —
the subagent roles are already skills) and needs **zero** new daemon machinery.
There is **no `workflow.list`/`workflow.describe` tool surface**: a workflow is
discovered in the skills index and loaded with `LoadSkill` like any other skill.
Stages carry an optional `workflow` label only so the run board can group them.

### Soft control flow, hard signals

The skill is written in a **graph-shaped** form (states, outcomes, transitions)
so it is auditable and unambiguous, but it is **parent-interpreted, not
daemon-executed**. What keeps it from being mushy prose is the **typed
`suggested_next` outcomes** each subagent reports (surfaced in the handoff
`index.json`): the reviewer returns `approved | changes_requested`, the tester
returns `pass | bugs_found | environment_issue`. Those are the **edge labels** the
parent branches on — hard signals, not vibes.

### Example: the `implement_review_test` skill

```markdown
# Workflow skill: implement -> review -> test

Use when a change should be implemented, reviewed until a reviewer is satisfied,
then tested, looping back on failures. You drive this loop; branch on the typed
outcomes each subagent reports in the handoff index.json.

## Stages
- implementer - full subagent (writes the workspace in place)
- reviewer    - read-only subagent(s) (review only; never write)
- tester      - full subagent (runs the suite; reports results)

## Outcomes (suggested_next, in index.json)
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
- implement: stage.start_full({ role:"implementer",
    prompt:<goal + latest review/test notes>, workflow:"implement_review_test" })
- review:    stage.start_readonly_fanout({ tasks:[{role:"reviewer",
    prompt:<what to review + acceptance criteria>}], workflow:"implement_review_test" })
- test:      stage.start_full({ role:"tester",
    prompt:<how to test>, workflow:"implement_review_test" })

When the handoff steer arrives, read index.json then the relevant
final_message.md, and take the branch above. Subagents start fresh, so carry the
prior stage's findings (from the handoff files) into the next stage's prompt.
```

This expresses the full cyclic flow — gates, back-edges, restart, termination —
with no DSL and no daemon interpreter.

### The honest risk and its mitigations

Nothing **enforces** the gates: a parent could loop forever or skip review. We
accept that, mitigated without a DSL by (a) explicit termination/escalation rules
in the skill (the "ask the human after ~3 rounds" line), (b) the human watching
the run board and able to steer/cancel, and (c) typed outcomes making the
branches crisp. If a single critical gate ever needs hard enforcement (e.g.
"cannot finish without a tester `pass`"), add a targeted check for that case —
not a general graph engine.

Bundled workflow skills to ship first:

- `explore` — one RO fan-out; the parent synthesizes from the handoff files.
- `implement_review` — implementer/reviewer loop until `approved`.
- `implement_review_test` — the cyclic example above.
- `kubernetes_e2e` — a single full stage with the `kubernetes-tester` role and
  safety rules.

There is no `parallel_race` skill: it requires parallel writers, which this model
does not support.

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
```

Workflows have **no tools**: they are skills, discovered in the skills index and
loaded with the existing `LoadSkill` tool.

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

This plan **transitions subagent invocation off the Python REPL and onto regular
daemon RPC tool calls.** Today, `PI.md` teaches the model to orchestrate through
the `PythonRepl` tool's `subagents.*` host functions (`spawn`/`spawn_bulk`/
`wait`/`call`/...), which busy-wait inside a long-lived Python process. The stage
model replaces that with ordinary tool calls (`stage.*`) — the same kind of
daemon RPC the existing `subagent.spawn` already is — plus daemon-driven steer
notifications, so there is no REPL and no busy-wait on the orchestration path.
Workflows add no tools; they are skills loaded with `LoadSkill`.

| Surface | Today | Steady state |
| --- | --- | --- |
| `stage.*` (regular tool calls) | none yet | the way to run staged and parallel-RO work |
| workflow skills (`SKILL.md` + `LoadSkill`) | roles exist as skills | named, possibly-cyclic stage playbooks |
| Python REPL `subagents.*` (busy-wait orchestration) | current primary path in `PI.md` | raw escape hatch only; removed from `PI.md` guidance |
| `subagent.spawn` / `subagent.list` RPCs | low-level, REPL-backed | fold into / be replaced by `stage.*` |

Sequence:

1. Ship typed subagents + stages + handoff/steer notifications (Phases 0–3) as
   regular RPC tools.
2. Rewrite the `PI.md` "Subagent delegation" section to teach `stage.*`, workflow
   skills, the handoff directory, fresh-context task prompts, and
   park-but-stay-responsive. **Delete the `subagents.spawn/wait/...` guidance**
   so the model stops reaching for the REPL to orchestrate.
3. Keep the `PythonRepl` tool available as a raw escape hatch (ad hoc scripting),
   but it is no longer the orchestration surface. Fully retiring the REPL
   `subagents.*` host functions is a later cleanup once `stage.*` covers the real
   use cases.

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

- Implement the handoff directory writer (`index.json` + per-subagent
  final_message.md + transcript.md, rendered from the durable transcript).
- Stage runner: barrier over all subagents, single-flight completion, one steer
  to the parent pointing at the handoff dir, attempt fencing, crash-recovery
  sweep.
- System-prompt park-but-stay-responsive instructions; deterministic idempotency
  tests.

### Phase 4: workflow skills

- Author bundled workflow skills (`SKILL.md`) discovered via the skills index and
  loaded with `LoadSkill`: `explore`, `implement_review`,
  `implement_review_test`, `kubernetes_e2e`.
- Each documents its stages, typed outcomes, and (possibly cyclic) control flow
  in the graph-shaped prose form shown in "Workflows are skills". No daemon
  changes — skills are data.

### Phase 5: UI

- Run board: parent session -> stages -> subagents with status and links to their
  handoff files; show the full subagent's in-place changes.
- Controls: cancel stage, steer the full subagent, re-run a stage.

## Resolved decisions

These were open questions in earlier revisions and are now settled:

- **Concurrent parent + full-subagent writes → soft rule.** The parent stays
  responsive during a full stage; it is told (prompt) that the full subagent owns
  the workspace and to supervise/read rather than edit until the subagent
  returns. We do **not** hard-park the parent's write tools. RO stages are
  conflict-free regardless.
- **Handoff directory cleanup → none.** The daemon never deletes handoff files;
  they are durable run history. (A cascade-on-session-delete can be added later if
  disk use ever matters, but it is not part of v1.)
- **Handoff format includes an index.** Each stage gets an `index.json` manifest
  (structured per-subagent status/outcome + relative paths) alongside the
  per-subagent `final_message.md` / `transcript.md`. The parent reads
  `index.json` first.
- **Subagent context is fresh, not forked.** No `fork_context`; the parent routes
  context through the task prompt and handoff paths.

## Open questions

1. **Multi-dir snapshot consistency.** An RO subagent must snapshot all workspace
   subdirectories at one consistent point. Low stakes (disposable) but do it
   atomically.
2. **Handoff dir and tooling visibility.** Confirm the handoff dir is excluded
   from workspace git repos (it is a sibling of the workspace dirs, so naturally
   outside them) and from RO snapshots, and that the parent's file tools can read
   it. (Likely add it to the per-workspace ignore set so a full subagent does not
   accidentally stage it.)
3. **Transcript render fidelity.** `transcript.md` is the human-readable render;
   confirm it captures enough (tool calls/results, not just messages) to debug a
   failed subagent, while staying greppable.

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
9. Workflows are skills (`SKILL.md` + `LoadSkill`) that document a possibly-cyclic
   stage state machine; the parent interprets them and owns sequencing. No DSL,
   no daemon-executed graph, no `workflow.*` tools.
10. The daemon owns mechanism (typed subagents, snapshots, handoff dir, steers,
    durable stages); the model owns policy (which stage next, re-run, stop).

## Appendix A: tool schemas (regular RPC tools)

These are ordinary daemon RPC tools (the same kind as the existing
`subagent.spawn`), surfaced to the model as function tools. They are **not** REPL
host functions.

```jsonc
// stage.start_full — launch the single full (writing) subagent of a stage.
// The parent should end its turn after calling this.
{
  "name": "stage.start_full",
  "input": {
    "role": "implementer",                 // role/SKILL to load
    "prompt": "Implement X in place. ...",  // scoped task; fresh context
    "workflow_id": "implement_review_test", // optional, for grouping/UI
    "label": "implement X"                  // optional human label
  },
  "result": { "stage_id": "stage_8", "subagent_session_id": "session_..." }
}

// stage.start_readonly_fanout — launch N RO subagents in parallel, each in its
// own disposable snapshot. The parent should end its turn after calling this.
{
  "name": "stage.start_readonly_fanout",
  "input": {
    "tasks": [
      { "role": "reviewer", "prompt": "Review the change for correctness. ..." },
      { "role": "reviewer", "prompt": "Review the change for security. ..." }
    ],
    "workflow_id": "implement_review_test",
    "label": "review fan-out"
  },
  "result": { "stage_id": "stage_9", "subagent_session_ids": ["session_...", "session_..."] }
}

// stage.status — inspect a stage and its subagents (also readable via index.json).
{ "name": "stage.status", "input": { "stage_id": "stage_9" },
  "result": { "stage_id": "stage_9", "kind": "readonly_fanout", "status": "running",
              "subagents": [ { "id": "...", "status": "running" } ],
              "handoff_dir": "<cwd>/.pi-handoff/stage_9" } }

// stage.cancel — cancel an in-flight stage (all its subagents).
{ "name": "stage.cancel", "input": { "stage_id": "stage_9" }, "result": { "cancelled": true } }

// Workflows have NO tools. A workflow is a SKILL.md, discovered in the skills
// index and loaded with the existing LoadSkill tool. The optional "workflow"
// field on stage.start_* is just a grouping label for the run board.
```

Errors the daemon enforces (not the model): starting a second stage while one is
running; mixing full and RO in one stage; more than one full subagent; steering an
RO subagent. Completion is **not** a tool result — it arrives later as a steer
pointing at the handoff `index.json`.

## Appendix B: draft `PI.md` "Subagent delegation" block

This replaces the current REPL-oriented section once the tools exist. It is drafted
here, not applied to `PI.md`, because wiring guidance for tools that do not exist
yet would mislead the model today.

```markdown
## Subagent delegation

Delegate work to subagents through stage tool calls. Do not use the Python REPL
to orchestrate subagents.

Two kinds of subagent:

- **read-only (RO)** — for investigation, review, analysis, and running
  builds/tests to gather information. RO subagents run in a private throwaway
  copy of the workspace; nothing they write reaches your workspace. Use
  `stage.start_readonly_fanout` to run several in parallel.
- **full** — for making changes. A full subagent edits your workspace in place.
  Use `stage.start_full`. There is exactly one full subagent at a time.

Rules:

- Launch at most one stage per turn, then end your turn. Do not poll or wait in a
  loop — you will be notified.
- When a stage finishes you receive a short message pointing at a handoff
  directory. Read its `index.json` first, then each subagent's
  `final_message.md`; open `transcript.md` only if you need detail.
- Give each subagent a self-contained task: it starts with fresh context and only
  knows what you put in its prompt (and any handoff/workspace paths you cite).
- While a full subagent is running, supervise and read — do not edit the
  workspace yourself until it returns.
- Never mix RO and full work in one stage.
- To run a known pattern (e.g. implement→review→test), `LoadSkill` the matching
  workflow skill and follow its stage state machine, branching on the typed
  outcomes in `index.json`, with your own judgment (skip, re-run, escalate, or
  stop based on the results).
- The `PythonRepl` tool remains only for ad hoc scripting, not for orchestrating
  subagents.
```

## Appendix C: status of decisions

Settled: shared one durable workspace; full writes in place; RO disposable
snapshots GC'd on return; no merge/adoption/rollback (rollback deferred);
no parallel writers / no `parallel_race`; handoff directory with `index.json`
+ per-subagent `final_message.md`/`transcript.md`; steer (not follow-up)
notifications; single barrier per fan-out (partial results + failures together);
parent parks but stays responsive (soft write rule during full stages); no
handoff cleanup; fresh subagent context (no fork); subagents non-recursive;
retry == continue the session; transition off the Python REPL to `stage.*` RPC
tools; **workflows are skills (`SKILL.md` + `LoadSkill`), not a DSL and not a
`workflow.*` tool surface** — cyclic control flow is parent-interpreted, branching
on typed outcomes.

Still open (operational, non-blocking): multi-dir snapshot atomicity; handoff dir
ignore/visibility; transcript render fidelity.
