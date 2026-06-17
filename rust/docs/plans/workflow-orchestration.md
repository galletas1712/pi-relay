# Minimal workflow orchestration plan

Status: proposed. Last reviewed 2026-06-16 (refinement pass: removed the merger
primitive and cross-task git source refs, made the runner single-flight with a
pure `advance`, typed plugin outcomes, added human-resumption + testing +
subagent-migration sections, and flagged the architecture-doc contradiction).

## Summary

Keep the architecture small.

The workflow system only needs four durable concepts:

1. **Run** — the overall goal and current status.
2. **Task** — one unit of agent or deterministic work.
3. **Artifact** — the durable handoff/evidence/change record between tasks.
   Artifacts are the **only** channel that moves context (including code
   changes, as diffs) between tasks.
4. **Workspace source** — how a task sees files. Every task has **at most one**
   workspace ancestor: `none`, `read_parent`, `fork_parent`, or `from_task`.
   There is no multi-source workspace mode.

Everything else should be derived from those concepts or left inside workflow
plugin code.

Do **not** start with:

- a generic graph DSL;
- a budget abstraction;
- workflow variables;
- a separate transition proposal table;
- a general policy envelope object;
- a workspace lease table;
- custom graph editing UI;
- a daemon-side cross-task merge mechanism (git source refs, synthetic merge
  commits, or a `sources` workspace mode). See "Combining parallel work" below.

Workflows should be discoverable plugins, not hardcoded branches in the daemon.
Ship a small set of bundled plugins first:

- `explore`;
- `hill_climb`;
- `implement_review`;
- `implement_review_test`;
- `kubernetes_e2e`;
- `parallel_race`.

Each plugin is ordinary Rust code that creates tasks, waits for task outputs,
reads artifacts, and schedules the next tasks. A top-level agent discovers and
invokes workflows through normal tool calls, not through Python REPL code.

## Relationship to the stated architecture

This plan deliberately reverses a documented non-goal. `architecture.md` lists
"Do not include subagent orchestration" (Goal 6) and "Hierarchical subagent
orchestration" under "Not implemented by design", and "Removed Pieces" still
names the deleted `agent-orchestrator` crate. That guidance was written when the
runtime had no durable multi-session work model. It is now stale: subagent
sessions, parent links, the Python orchestration REPL, and source-ref merging
all shipped. Before Phase 1, `architecture.md` must be updated in the same
change so there is no silent contradiction at the top of the design. The
durable runtime stays small; what changes is that orchestration is now an
explicit, persisted concept instead of a forbidden one.

### This is the third orchestration attempt; do not repeat the first two

The runtime has already tried and discarded two orchestration designs. The new
plan must avoid re-growing what sank them:

1. **`agent-orchestrator` crate + `SessionRegistry` (removed).** A process-local
   orchestrator owned control flow in RAM. It died because durable state, not a
   live object graph, has to be the source of truth. *Lesson: the runner must be
   stateless and reconstruct everything from Postgres on every step.*
2. **`workflow_variables` + `work.*` RPCs + Python workflow SDK (deleted in
   "Simplify subagent handles on the wire").** Control flow polled named
   variables (`work.await` on `vars`), and orchestration logic lived in editable
   Python templates. It died because the variable store became a second,
   untyped state machine and the templates were ungoverned. *Lesson: no
   general-purpose variable store, and no model-authored control-flow scripts as
   the normal path.*

Two concrete guardrails follow directly from that history:

- **`run.state` is not a variable store.** It holds only the small,
  plugin-private bookkeeping a plugin needs to compute its next step (e.g. an
  iteration counter, the current best artifact id). It must never become a
  general key/value channel between tasks — that is what artifacts are for. If a
  plugin wants to stash arbitrary cross-task data in `run.state`, that is the
  `workflow_variables` failure returning under a new name; reject it in review.
- **Control flow lives in compiled plugin code, never in model-authored
  scripts.** The Python REPL stays an escape hatch for ad hoc delegation, not a
  workflow runtime.

## What changed from the larger plan

The prior plan was directionally right but too abstract. This version collapses
several nouns:

| Larger-plan concept | Minimal replacement |
| --- | --- |
| `WorkflowRun` | `Run` |
| `WorkflowTask` | `Task` |
| `WorkflowArtifact` | `Artifact` |
| `TransitionProposal` | `Artifact { kind: "decision" }` or task result field |
| `WorkspaceLease` | single-parent workspace lineage (one writer by construction) |
| `WorkspaceInstance` table | workspace manager snapshot record |
| cross-task git source refs / `sources` mode | `changes` artifacts (diffs) + an `integrate` task |
| `merger` role | `select` / `integrate` / `reduce` patterns (see below) |
| budget/policy envelope | plugin-specific stopping conditions and allowed outputs |
| controller agent primitive | a normal task with role `controller` |
| workflow variables | artifacts (+ tiny plugin-private `run.state`) |
| custom graph DSL | workflow plugins |

The core invariant remains:

> The daemon deterministically records state and starts/stops tasks. Agents
> perform work and return structured outputs. Workflow plugin code decides the
> next task from those outputs.

## Combining parallel work

There is no `merger` primitive and no daemon-side cross-task merge. "Merge" was
the wrong noun: it bundled three different operations that have different right
answers. Parallel tasks produce one of three output shapes, and the workflow
plugin picks the matching pattern explicitly:

| Pattern | When parallel tasks are… | What happens | Workspace |
| --- | --- | --- | --- |
| **select** | redundant candidates for the *same* goal | an evaluator reads each candidate's `handoff`/`changes` artifact, picks one winner, the run continues `from_task(winner)`; losers are discarded | winner's fork is reused via `from_task` |
| **integrate** | complementary, each owning a *different* slice | one integrator task starts `from_task(base)` and is handed the others' `changes` (diff) artifacts to apply with judgment in a single workspace | one writable fork |
| **reduce** | producing *findings*, not code | a reducer writes one synthesis artifact; no workspace combination occurs | `none` or `read_parent` |

Why this is better than a generic merger:

- **It keeps one context-transfer channel.** Code crosses task boundaries as a
  `changes` artifact (a diff, as text), exactly like every other handoff. There
  is no second transport (git source refs / synthetic commits) that only works
  for git workspaces and silently skips local-folder workspaces.
- **One writer per workspace is true by construction.** Workspace lineage is a
  tree of single-parent edges (`fork_parent` / `from_task`), so no two tasks
  ever write the same directory and no lease table is needed.
- **It is honest about cost.** Most parallel agent work wants *select* (throw
  away all but one), sometimes *integrate* (re-apply disjoint diffs with
  judgment), and only rarely a true 3-way merge — which an agent should perform
  deliberately, not the daemon implicitly.

Escape hatch for the rare genuine merge: an `integrate` task may be granted
read-only access to a sibling task's sealed workspace snapshot when a diff is
too large to pass as an artifact. That is an explicit, logged exception a plugin
requests — not a standing primitive, and never a writable cross-task workspace.

This removes the git source-ref machinery
(`snapshot_worktree_commit` → `refs/pi-relay/sources/*` → in-child `git merge`),
the `sources` workspace mode, and the `merger` role. The
`subagent-source-ref-merge-plan.md` document is superseded by this section.

## Product model

Humans should see:

```text
Run
  goal, kind, status, current phase

Task cards
  role, status, latest summary, transcript link, controls

Artifacts
  handoffs, evidence, logs, diffs, snapshots, human notes

Decision
  latest next-step recommendation or human approval request
```

Humans should not have to think about graphs, transitions, leases, or workflow
state machines.

## Agent model

A normal task agent gets a scoped task contract, not the whole workflow graph.

Task prompt shape:

```text
You are task <task_id> in run <run_id>.

Role:
  <role name and SKILL.md content>

Run goal:
  <overall goal>

Task goal:
  <specific objective for this task>

Context:
  <selected handoff/evidence/changes artifacts>

Workspace:
  <path and one of: none | read_parent | fork_parent | from_task>

Allowed outputs:
  - complete with handoff
  - fail with reason/evidence
  - request human
  - suggest next step, from this task's declared outcome set,
    if this task is allowed to do that

Stopping condition:
  <task-specific stop condition>
```

The task agent's control flow is:

1. read the scoped prompt;
2. use normal tools;
3. write evidence/artifacts if needed;
4. finish with one structured task result.

The agent does not directly mutate workflow state.

## Subagent model

With workflows in place, subagents should no longer be the primary orchestration
API. They are just an execution mode for tasks, plus an interactive delegation
escape hatch.

Use two modes:

### 1. Workflow task agents

These are the normal agents spawned by workflow plugins.

Default behavior:

- one task prompt;
- one child session;
- no recursive subagent spawning;
- no Python REPL orchestration;
- scoped task tools only;
- child writes artifacts/results;
- parent workflow runner observes completion through lifecycle events.

They are "one-shot" in the sense that the workflow expects a terminal task
result. They do not decide arbitrary control flow. They may still run for a long
time and use normal tools, especially for Kubernetes/e2e.

They should usually be interruptible/cancellable by the human and workflow
runner. Making them literally uninterruptible would make long Kubernetes tests,
stuck commands, or wrong assumptions painful. The simpler rule is:

> workflow task agents are non-recursive and scoped, but still async,
> observable, and interruptible.

Read-only one-shot calls are still useful as an optimization for some tasks:

```text
task.execution = "inline_call"
task.workspace.mode = "none" or "read_parent"
```

Use that for small exploration, summarization, review, or classification tasks.
Do not make it the only subagent mode.

### 2. Direct delegation agents

These are user/top-level-agent spawned helpers outside a workflow run.

Keep them more flexible:

- async;
- inspectable;
- steerable;
- interruptible;
- able to have isolated workspaces;
- able to be used for ad hoc exploration or implementation.

Direct delegation remains useful even with workflows because a top-level agent may
want to split a task into smaller chunks without creating a formal run.

However, direct subagent spawning should use regular tools, not Python REPL:

```text
agent.spawn
agent.status
agent.read
agent.cancel
```

Do not expose recursive subagent spawning to workflow task agents by default. If
a workflow needs decomposition, the workflow plugin should create additional
tasks.

### Coexistence and migration

Three orchestration surfaces will exist during the transition. They must not
remain three forever. The intended steady state and sequencing:

| Surface | Today | Steady-state intent |
| --- | --- | --- |
| `workflow.*` plugins | new | the default way to run any multi-step/parallel work that has a reusable shape |
| `agent.*` direct delegation tools | new | the only ad hoc, no-run delegation path; replaces REPL `subagents.*` |
| Python REPL `subagents.*` | current primary API | demoted to an escape hatch; deprecated for orchestration once `agent.*` lands |

Migration sequence (so we never run two ad hoc delegation APIs as equals):

1. Ship `workflow.*` and `agent.*` (Phases 1–6).
2. Re-point the `PI.md` "Subagent delegation" guidance at `agent.*` for ad hoc
   work and at `workflow.*` for shaped work. The current eager-delegation
   guidance that teaches `subagents.spawn/spawn_bulk/wait/...` moves to `agent.*`
   verbs.
3. Keep the Python REPL available as a raw escape hatch, but stop documenting
   `subagents.*` as the normal orchestration path. Do not delete it in this
   plan; deleting it is a separate follow-up once `agent.*` and `workflow.*`
   have absorbed the real use cases.

The litmus test for "is this ad hoc or a workflow": if the control flow is
reusable and worth naming, it is a workflow plugin; if it is a one-off split the
top-level agent is doing in the moment, it is `agent.*`. Neither should be the
Python REPL.

## Minimal durable schema

### `runs`

```text
runs
  id text primary key
  kind text not null
    -- explore | hill_climb | implement_review | implement_review_test |
    -- kubernetes_e2e | parallel_race
  root_session_id text null references sessions(id)
  status text not null
    -- running | blocked | done | failed | cancelled
  goal text not null
  params jsonb not null default '{}'
  state jsonb not null default '{}'
  revision bigint not null default 0
    -- bumped on every run transition; CAS guard for status changes
  created_at timestamptz not null default now()
  updated_at timestamptz not null default now()
```

`params` contains plugin-specific input. Do not standardize more than needed.

`state` is small, plugin-private bookkeeping only (e.g. iteration counter,
current best artifact id). It is **not** a cross-task variable store: tasks never
read or write `run.state`, and anything a task needs to hand to a later task is
an artifact. Keeping arbitrary data here is the deleted `workflow_variables`
failure returning under a new name.

Examples:

```json
{
  "kind": "implement_review_test",
  "goal": "Add durable workflow runs",
  "params": {
    "review_until_approved": true,
    "test_kind": "kubernetes_e2e",
    "stop_when": "review approved and tests pass"
  }
}
```

```json
{
  "kind": "kubernetes_e2e",
  "goal": "Run end-to-end tests for the operator",
  "params": {
    "context": "dynamo-nscale-dev",
    "namespace": "schwinns",
    "stop_when": "tests pass, human approval is needed, or a code issue is found"
  }
}
```

### `tasks`

```text
tasks
  id text primary key
  run_id text not null references runs(id) on delete cascade
  task_key text not null
    -- plugin-chosen deterministic key; unique per run for idempotent upsert
  session_id text null references sessions(id)
  attempt_id text null
    -- fences stale child completions, like the action attempt_id in agent-store
  role text not null
    -- explore | implementer | reviewer | tester | kubernetes-tester |
    -- reducer | evaluator | integrator | controller | ...
  status text not null
    -- pending | running | blocked | done | failed | cancelled
  prompt text not null
  input_artifact_ids jsonb not null default '[]'
  output_artifact_id text null
  workspace jsonb not null default '{}'
  result jsonb null
  created_at timestamptz not null default now()
  updated_at timestamptz not null default now()
  unique (run_id, task_key)
```

`task_key` is what makes `advance` idempotent: the runner upserts desired tasks
by `(run_id, task_key)`, so re-evaluating a plan never double-creates a task.
`attempt_id` fences completion the same way `agent-store` fences action rows — a
child-session completion only writes back if its attempt still matches.

`workspace` is intentionally just a small instruction blob with a single
ancestor:

```json
{
  "mode": "fork_parent"
}
```

```json
{
  "mode": "from_task",
  "task_id": "task_impl_2"
}
```

```json
{
  "mode": "none"
}
```

Supported workspace modes:

- `none` — task does not need files.
- `read_parent` — task reads parent/current workspace only.
- `fork_parent` — task gets a writable Btrfs/copy snapshot of the parent.
- `from_task` — task starts from exactly one prior task's output snapshot.

There is intentionally no multi-source mode. To combine the output of several
tasks, pick the `select` / `integrate` / `reduce` pattern from "Combining
parallel work": the integrator starts `from_task(base)` and receives the other
tasks' work as `changes` (diff) artifacts.

One-writer safety is enforced by construction:

- workspace lineage is a tree of single-parent edges;
- tasks that write get their own `fork_parent` or `from_task` fork;
- sequential tasks chain with `from_task`;
- no two tasks ever name the same writable directory, so no two parallel tasks
  can write the same files.

The runner rejects a task whose `workspace.mode`/`task_id` would create a second
writer for an already-claimed workspace lineage. No separate lease table is
needed.

### `artifacts`

```text
artifacts
  id text primary key
  run_id text not null references runs(id) on delete cascade
  task_id text null references tasks(id)
  kind text not null
    -- context | handoff | evidence | changes | snapshot |
    -- decision | human_request | human_note
  content_text text null
  content_json jsonb null
  created_at timestamptz not null default now()
```

Artifacts are the only context-transfer primitive.

Use artifacts for:

- handoff notes;
- test evidence;
- Kubernetes logs/events/manifests;
- code changes as diffs (`kind: changes`) — the only way code crosses a task
  boundary;
- snapshots;
- human approval requests and human notes;
- reducer/evaluator summaries;
- hill-climb best-so-far records;
- controller/agent next-step suggestions.

There is no `source_refs` artifact kind. Cross-task code is a `changes` diff,
not a git ref (see "Combining parallel work").

## Task result shape

Every task ends with a small structured result:

```json
{
  "status": "done",
  "summary": "Implemented the workflow run table and repository methods.",
  "artifact_id": "artifact_impl_handoff",
  "suggested_next": "review"
}
```

or:

```json
{
  "status": "failed",
  "summary": "Kubernetes auth is expired.",
  "artifact_id": "artifact_tsh_output",
  "suggested_next": "request_human"
}
```

or:

```json
{
  "status": "done",
  "summary": "Tests failed because the deployed image is stale.",
  "artifact_id": "artifact_k8s_evidence",
  "suggested_next": "rebuild_image"
}
```

`suggested_next` is not a state mutation. It is input to the workflow plugin.

It is also **not free text.** Each plugin declares a small typed `Outcome` enum
(its accepted `suggested_next` values), and the task contract advertises exactly
that set under "Allowed outputs". The runner validates the returned value
against the plugin's enum before calling `advance`; an unrecognized value is
recorded as a task error, not silently matched. This keeps control-flow
vocabulary typed at the boundary, the same way the rest of the runtime treats
wire enums, instead of coupling a plugin's `match` arms to a task agent's prose.

Example: `kubernetes_e2e` accepts exactly
`{ pass, product_failure, environment_retry, human_needed }`.

## Workflow runner

The runner is the one piece that must be as careful about concurrency as the
rest of the runtime. It is stateless: it owns no in-memory run graph and
reconstructs everything from Postgres on each step. It reuses the existing
discipline — a per-row lock, an `attempt_id` fence, and revision counters —
applied to runs instead of sessions.

### `advance` is pure; the runner reconciles

`advance` must be a **pure function** of `(run, tasks, artifacts) -> Plan`. It
performs no I/O and creates nothing itself. It returns a declarative *desired
next state*:

```rust
enum Plan {
    /// The complete set of tasks that should exist for this run, keyed by a
    /// plugin-chosen deterministic task key. The runner diffs this against the
    /// tasks already in the database.
    Tasks(Vec<DesiredTask>),
    Block { reason: String },           // e.g. waiting on a human request
    Complete { summary_artifact: ArtifactRef },
    Fail { reason: String },
}

struct DesiredTask {
    key: String,        // deterministic, stable across re-evaluation
    role: String,
    prompt_inputs: PromptInputs,
    workspace: WorkspaceMode,
    input_artifact_ids: Vec<String>,
}
```

The runner turns a `Plan` into reality idempotently:

- For each `DesiredTask`, **upsert by `(run_id, key)`**. A task whose key already
  exists is left alone; only genuinely new keys create rows. This is what makes
  a double-fired `advance` harmless: the second evaluation produces the same
  keys and inserts nothing.
- `Block` / `Complete` / `Fail` are compare-and-set transitions on `run.status`,
  guarded by the run revision.

Because `advance` is pure and the runner reconciles to a desired set, a crash
mid-step is trivially recoverable: re-running `advance` from the persisted rows
reproduces the same plan.

### Single-flight per run

Exactly one `advance` evaluation runs per run at a time, mirroring the
per-session row lock in `agent-store`:

1. Take the run row lock (`select ... for update`) — different runs proceed
   concurrently; the same run is strictly serialized.
2. Load tasks and artifacts in the same transaction (consistent snapshot).
3. Reconcile any finished child sessions into task `result`/`output_artifact_id`
   (fenced by the task's `attempt_id`, so a stale child completion matches zero
   rows).
4. Call `advance(run, tasks, artifacts)`.
5. Apply the `Plan` (idempotent task upserts and/or a CAS run-status change),
   bump `run.revision`, emit events, commit.

### What triggers a step

`advance` is evaluated on:

- run creation (`workflow.start`);
- any child task lifecycle event (`idle`/`done`/`failed`/`cancelled`) for a task
  belonging to the run — reusing the parent-visible lifecycle events from PR
  #150;
- an explicit `workflow.signal` (e.g. a human response that unblocks a run);
- crash recovery sweep at daemon startup, for runs left `running`.

There is no polling loop and no timer in v1. Steps are event-driven,
single-flight, and idempotent, so re-delivery of an event is safe.

Pseudo-code:

```rust
async fn step(run_id: &str) -> Result<()> {
    let mut tx = store.begin().await?;
    let run = store.lock_run(&mut tx, run_id).await?;        // for update
    let tasks = store.tasks_for_run(&mut tx, run_id).await?;
    let artifacts = store.artifacts_for_run(&mut tx, run_id).await?;
    store.reconcile_finished_tasks(&mut tx, &tasks).await?;  // attempt_id-fenced

    let plugin = workflow_registry.get(&run.kind)?;
    let plan = plugin.advance(&run, &tasks, &artifacts);     // pure, no I/O

    store.apply_plan(&mut tx, &run, plan).await?;            // idempotent upsert + CAS
    store.bump_run_revision(&mut tx, run_id).await?;
    tx.commit().await
}
```

The plugin owns control flow. The generic runner only persists and dispatches.

## Workflow plugins

### Plugin shape

A workflow plugin is the smallest unit of reusable orchestration.

It provides:

```text
id
  stable workflow kind, e.g. kubernetes_e2e

title
  human-readable name

description
  concise explanation for humans and agents

input_schema
  JSON schema for workflow.start params

default_roles
  role names the plugin may use

outcomes
  the typed set of suggested_next values this plugin accepts

start(params) -> initial run state + initial DesiredTask(s)

advance(run, tasks, artifacts) -> Plan        // PURE: no I/O, returns desired state
```

`start` and `advance` are pure and return declarative desired state; the runner
performs all persistence and dispatch. This is enough for discovery, invocation,
validation, and UI rendering.

### Plugin manifests

Each plugin should have a small manifest for discovery. For bundled Rust plugins,
the manifest can be compiled into the daemon. Later, workspace/user plugins can
provide the same manifest from disk.

Example manifest:

```json
{
  "id": "kubernetes_e2e",
  "title": "Kubernetes e2e test",
  "description": "Run adaptive end-to-end tests against a Kubernetes cluster.",
  "input_schema": {
    "type": "object",
    "required": ["context", "namespace", "test_goal"],
    "properties": {
      "context": { "type": "string" },
      "namespace": { "type": "string" },
      "test_goal": { "type": "string" },
      "stop_when": { "type": "string" }
    }
  },
  "default_roles": ["kubernetes-tester"],
  "outcomes": ["pass", "product_failure", "environment_retry", "human_needed"]
}
```

### Plugin discovery

Top-level agents should be able to discover workflows through tool calls:

```text
workflow.list
workflow.describe
```

`workflow.list` returns compact plugin summaries:

```json
{
  "workflows": [
    {
      "id": "explore",
      "title": "Parallel exploration",
      "description": "Spawn explorers and reduce findings."
    },
    {
      "id": "kubernetes_e2e",
      "title": "Kubernetes e2e test",
      "description": "Run adaptive e2e tests in a Kubernetes cluster."
    }
  ]
}
```

`workflow.describe({ "id": "kubernetes_e2e" })` returns the full manifest and
input schema.

### Plugin locations

Start with bundled Rust plugins.

Later support:

```text
repo/.agents/workflows/<workflow-id>/WORKFLOW.md
repo/.agents/workflows/<workflow-id>/workflow.wasm or workflow binary
~/.agents/workflows/<workflow-id>/WORKFLOW.md
```

Do not design the disk plugin ABI in v1. The important v1 decision is that
workflow discovery/invocation goes through a registry, not a hardcoded prompt or
Python helper.

## Bundled workflow plugins

### `explore`

Use for RLM-style parallel exploration.

Human UX:

- user asks a broad question;
- UI shows explorer task cards;
- each explorer returns findings/evidence;
- reducer summarizes;
- human can ask for another round.

Agent UX:

- explorer gets a focused research question;
- explorer returns an evidence artifact;
- reducer gets explorer artifacts, not full transcripts by default.

Control flow:

```text
start
  -> spawn N explore tasks
  -> when all done, spawn reducer
  -> reducer writes synthesis
  -> done
```

Minimal params:

```json
{
  "question": "...",
  "num_explorers": 4,
  "context_artifact_ids": []
}
```

### `hill_climb`

Use for candidate search.

Human UX:

- see current best candidate;
- see candidate/evaluator cards by iteration;
- stop or promote a candidate whenever desired.

Agent UX:

- proposer sees best-so-far artifact and prior evaluator notes;
- evaluator sees candidate and metric/criteria;
- evaluator writes score/evidence.

Control flow:

```text
start
  -> propose candidate
  -> evaluate candidate
  -> if better, update best artifact
  -> continue until stop_when is satisfied or human stops
```

Minimal params:

```json
{
  "objective": "...",
  "metric": "plain text or JSON scoring contract",
  "stop_when": "good enough or human stops"
}
```

No generic budget object is needed. If a caller wants a max iteration count, put
`"max_iterations": 5` in this plugin's params.

### `implement_review`

Use when testing is not part of the workflow.

Human UX:

- see implementation task;
- see reviewer verdict;
- loop continues until reviewer approves, fails, or asks human.

Agent UX:

- implementer sees goal plus latest reviewer feedback;
- reviewer sees the implementer's handoff and `changes` (diff) artifact plus
  acceptance criteria;
- reviewer writes verdict artifact.

Control flow:

```text
start
  -> implement
  -> review
  -> if approved: done
  -> if changes requested: implement again from latest task
  -> if blocked/human needed: blocked
```

Minimal params:

```json
{
  "goal": "...",
  "acceptance": "...",
  "stop_when": "reviewer approves"
}
```

### `implement_review_test`

Use when code should be reviewed before testing.

Human UX:

- see implementation/review subloop;
- after approval, see test task;
- if tests fail, see whether failure returns to implement/review or asks for
  human input.

Agent UX:

- implementer and reviewer behave as above;
- tester sees approved implementation artifact and test instructions;
- tester writes evidence and suggested next step.

Control flow:

```text
start
  -> implement_review subloop
  -> test
  -> if tests pass: done
  -> if tester says code_issue: implement_review again
  -> if tester says environment_retry: test again
  -> if tester says human_needed: blocked
  -> otherwise: failed
```

Minimal params:

```json
{
  "goal": "...",
  "acceptance": "...",
  "test": "cargo test, kubernetes_e2e, or plain-text test instruction",
  "stop_when": "reviewer approves and tests pass"
}
```

### `kubernetes_e2e`

Use when the agent should run adaptive end-to-end Kubernetes tests without an
implementer/reviewer loop.

Human UX:

- see cluster/context/namespace;
- see current operation and evidence;
- see explicit human requests for auth or unsafe operations;
- final output classifies the result.

Agent UX:

- Kubernetes tester gets the run goal, context, namespace, and safety rules;
- it adapts to cluster state;
- it writes evidence artifacts;
- it returns a suggested next step.

Control flow:

```text
start
  -> kubernetes-tester
  -> if pass: done
  -> if environment_retry: kubernetes-tester again
  -> if product_failure: failed with evidence
  -> if human_needed: blocked
```

Minimal params:

```json
{
  "context": "dynamo-nscale-dev",
  "namespace": "schwinns",
  "test_goal": "...",
  "stop_when": "tests pass, code issue found, or human needed"
}
```

The `kubernetes-tester` role `SKILL.md` should encode Kubernetes safety rules:

- always pass explicit `--context` and namespace;
- never rely on current kube context;
- avoid unsafe cluster-scoped destructive operations;
- request human approval for dangerous shared-cluster operations;
- handle Teleport auth expiration by asking the user;
- collect logs/events/manifests as evidence.

### `parallel_race`

Use when multiple implementers/candidates should race at the *same* goal. This
is the **select** pattern: the run keeps one winner and discards the rest. It
does not merge candidates.

Human UX:

- see candidate task cards in parallel;
- see the evaluator card;
- choose/promote the winning result.

Agent UX:

- each candidate agent gets the same goal and its own `fork_parent`;
- the evaluator reads each candidate's `handoff` + `changes` (diff) artifacts —
  not their workspaces — and picks one.

Control flow:

```text
start
  -> spawn N candidate tasks with fork_parent
  -> when all done, spawn one evaluator task (workspace: none)
       inputs: each candidate's handoff + changes artifacts
  -> evaluator writes a decision artifact naming the winner
  -> continue the run from_task(winner); discard the losing forks
  -> done, or blocked for explicit human choice
```

If the goal was actually decomposable into disjoint slices rather than redundant
candidates, use the **integrate** pattern instead (a single integrator task
seeded `from_task(base)` that applies the others' `changes` diffs). `parallel_race`
is specifically for redundant candidates.

Minimal params:

```json
{
  "goal": "...",
  "num_candidates": 3,
  "selection": "best reviewed candidate or human choice"
}
```

## Workspace propagation

Keep this simple.

Every task gets exactly one workspace instruction with a single ancestor:

```json
{ "mode": "none" }
```

```json
{ "mode": "fork_parent" }
```

```json
{ "mode": "from_task", "task_id": "task_impl_1" }
```

The workspace manager decides how to implement that instruction:

- Btrfs snapshot when available;
- reflink/copy fallback otherwise.

Do not expose Btrfs as a workflow concept. Btrfs is an implementation detail of
fast fork/seal/copy.

There is no multi-source workspace mode and no cross-task git ref import. Code
moves between tasks as a `changes` (diff) artifact, applied by an `integrate`
task in its own single workspace (see "Combining parallel work").

## Context transfer

Only artifacts transfer context between tasks.

Avoid:

- transcript scraping as the default;
- workflow variables;
- hidden global memory;
- cross-task git refs.

A task prompt should include:

- run goal;
- task goal;
- role instructions;
- selected prior artifact summaries (handoff / evidence / changes);
- stopping condition;
- required output schema (including its allowed `suggested_next` values).

If an agent needs more detail, it can open linked artifacts or transcripts, but
the default prompt should be compact.

## Tools

Minimal workflow tools:

```text
workflow.list
workflow.describe
workflow.start
workflow.status
workflow.cancel
workflow.signal
workflow.read_artifact
```

Top-level agents use `workflow.list` / `workflow.describe` to discover available
workflow plugins, then call `workflow.start` with plugin-specific params.

Minimal task tools:

```text
task.write_artifact
task.complete
task.fail
task.request_human
```

`task.complete` may include `suggested_next`.

Do not add `workflow.propose_transition` or `task.propose_transition` in v1.
They are just `task.complete({ suggested_next: ... })`.

Keep Python REPL as an escape hatch, not the normal workflow runtime.

Minimal direct delegation tools:

```text
agent.spawn
agent.status
agent.read
agent.cancel
```

These replace the Python REPL as the normal way for a top-level agent to ask for
ad hoc helper work. Workflow task agents should not receive these tools by
default.

## Human requests and resumption

`blocked` is a first-class run state, and "a human must do something" is a
normal, durable outcome — not an error. The mechanism is fully expressed with
the existing primitives:

1. A task calls `task.request_human(message)`. The runner writes an artifact
   `{ kind: "human_request" }` (carrying the question/required action) and the
   task completes with `suggested_next = human_needed`.
2. `advance` sees the `human_request` artifact has no answering `human_note` and
   returns `Plan::Block { reason }`. The run becomes `blocked`. No task is
   spawned; nothing polls.
3. The human answers in the run board ("answer human request"), which calls
   `workflow.signal({ run_id, request_artifact_id, response })`. The runner
   writes an artifact `{ kind: "human_note" }` referencing the request, then
   triggers one `advance` step (the same single-flight path as a task event).
4. `advance` now sees the request is answered and proceeds — typically creating
   the next task seeded with the `human_note` as an input artifact.

So resumption is just: `human_request` artifact → `blocked` → `human_note`
artifact (via `workflow.signal`) → `advance` re-runs. No special resume state,
no separate human-task table; a blocked run re-enters the ordinary runner step
when its signal arrives. Auth-expiry (the Teleport case) and "approve this
destructive operation" both use exactly this path.

## Testing

This subsystem must be testable without real model calls, the same way the
runtime already resolves model actions deterministically with the dev harness
(`harness.model.complete` / `harness.model.fail`).

- **Pure `advance` unit tests.** Because `advance` is a pure
  `(run, tasks, artifacts) -> Plan`, every plugin's control flow is unit-tested
  by constructing synthetic task results/artifacts and asserting the returned
  `Plan`. No Postgres, no sessions, no models. This is where the bulk of plugin
  correctness is proven.
- **Runner reconciliation tests (real Postgres).** Assert idempotency directly:
  running `step` twice for the same run state creates tasks once; a re-delivered
  lifecycle event does not double-spawn; a stale task completion (wrong
  `attempt_id`) matches zero rows. Inspect both the run/task/artifact rows and
  the emitted events, mirroring the existing "check transcript order *and* DB
  state" discipline.
- **A task harness** that resolves a task's child session to a chosen
  `{ status, summary, artifact, suggested_next }` without a provider, so
  end-to-end run progression (e.g. `implement_review_test` looping on
  `code_issue`) is exercised deterministically.
- **Outcome validation tests.** A `suggested_next` outside a plugin's declared
  outcome enum is recorded as a task error, not silently matched.

## PR notes

PR #150, `fix/subagent-orchestration-api-notifications`, is still important
because this minimal architecture needs parent-visible task lifecycle events:

- child spawned;
- child running;
- child idle/done;
- stale child recovery;
- event-driven UI refresh.

As of this revision PR #150's tip ("Improve subagent orchestration lifecycle")
is **not yet on `origin/main`** — it is the parent of this plan's own branch.
Phase 0 is therefore a real prerequisite, not a formality: land it on `main`
(or port its lifecycle-event work) before Phase 2 relies on those events to
drive `advance`.

PR #149 is unrelated UI polish.

Older workflow PR branches are useful design history only. Do not revive the old
`session_relationships` table, the `workflow_variables` store, the `work.*`
RPCs, or the editable Python workflow SDK. See "This is the third orchestration
attempt" for why each of those is a known failure mode, not a starting point.

## Implementation phases

### Phase 0: lifecycle foundation

- Merge or port PR #150 (its tip is not yet on `origin/main`).
- Keep using `sessions.parent_session_id`.
- Ensure parent-visible child lifecycle events work reliably.
- Update `architecture.md`: retire the "no subagent orchestration" non-goal and
  the stale "Removed Pieces" framing so the docs and this plan agree. (See
  "Relationship to the stated architecture".)

### Phase 1: minimal run/task/artifact tables

- Add `runs`.
- Add `tasks`.
- Add `artifacts`.
- Add repository methods.
- Add basic UI/RPC views.
- Add workflow plugin registry interfaces.
- Add bundled plugin manifests.

### Phase 2: task execution and the runner

- Start an agent task as a child session.
- Assemble scoped task prompt from run/task/artifacts.
- Record task result and output artifact when child session finishes.
- Support `task.write_artifact`, `task.complete`, `task.fail`,
  `task.request_human`.
- Ensure workflow task agents are non-recursive by default.
- Implement the single-flight runner: per-run row lock, `attempt_id`-fenced
  task-completion reconciliation, pure `advance`, idempotent
  upsert-by-`(run_id, key)`, CAS run-status transitions, run revision counter.
- Add the deterministic task harness and runner reconciliation tests (idempotent
  re-step, no double-spawn on re-delivered events, stale completion rejected).

### Phase 3: workflow plugin tools

- Add `workflow.list`.
- Add `workflow.describe`.
- Add `workflow.start` by plugin id.
- Add `workflow.status`, `workflow.cancel`, `workflow.signal`,
  `workflow.read_artifact`.
- Implement the human-request → `blocked` → `workflow.signal` → `advance`
  resumption path.

### Phase 4: bundled plugins

Implement bundled plugins in this order. Each lands with its pure-`advance`
unit tests and a declared outcome enum:

1. `explore` (reduce pattern);
2. `implement_review`;
3. `implement_review_test`;
4. `kubernetes_e2e`;
5. `parallel_race` (select pattern; add the `integrate` pattern only if a real
   disjoint-fan-out workflow needs it);
6. `hill_climb`.

### Phase 5: workspace lineage

- Implement task `workspace.mode` for `none` / `read_parent` / `fork_parent` /
  `from_task` only.
- Use existing Btrfs snapshot/fork support where available, reflink/copy
  otherwise.
- Enforce single-parent lineage in the runner: reject a task that would create a
  second writer for an already-claimed workspace.
- Implement `changes` (diff) artifact capture so `select`/`integrate` work
  without cross-task git refs.
- Do not implement a `sources` mode or git source-ref import.

### Phase 6: direct delegation tools

- Add regular tool-call replacements for Python REPL subagent orchestration:
  - `agent.spawn`;
  - `agent.status`;
  - `agent.read`;
  - `agent.cancel`.
- Keep direct delegation agents async, observable, steerable/cancellable where
  useful.

### Phase 7: UI

- Show runs.
- Show task cards.
- Show artifacts.
- Show pending human requests.
- Support cancel, retry, steer child session, and signal human response.

## Design rules

1. If a concept can be an artifact, make it an artifact.
2. If a transition can live in plugin code, do not make it a generic graph edge.
3. If a task can suggest the next step in its result, do not add a proposal
   table — but type that suggestion as a per-plugin outcome enum, not free text.
4. If workspace safety can be achieved by single-parent forking, do not add
   leases and do not add a multi-source mode.
5. If a workflow can be a plugin, do not create a DSL yet.
6. Keep the human UX to runs, task cards, artifacts, and human requests.
7. Keep the agent UX to scoped task prompts and structured task results.
8. Workflow task agents are scoped and non-recursive by default.
9. Direct delegation agents may remain async/interruptible because they are still
   useful outside formal workflows.
10. Workflow discovery and invocation should be regular tool calls, not Python
    REPL code.
11. Code crosses task boundaries only as a `changes` (diff) artifact. There is
    no daemon-side merge and no cross-task git ref. Combine parallel work with
    `select` / `integrate` / `reduce`, never a generic `merger`.
12. `advance` is a pure function and the runner reconciles to its desired state
    idempotently under a per-run lock. Control flow is never model-authored and
    never lives in `run.state`.
