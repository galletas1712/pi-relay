# Minimal workflow orchestration plan

Status: proposed. Last reviewed 2026-06-14.

## Summary

Keep the architecture small.

The workflow system only needs four durable concepts:

1. **Run** — the overall goal and current status.
2. **Task** — one unit of agent or deterministic work.
3. **Artifact** — the durable handoff/evidence/change record between tasks.
4. **Workspace source** — how a task sees files: parent snapshot, prior task
   snapshot/source refs, or no workspace.

Everything else should be derived from those concepts or left inside workflow
plugin code.

Do **not** start with:

- a generic graph DSL;
- a budget abstraction;
- workflow variables;
- a separate transition proposal table;
- a general policy envelope object;
- a workspace lease table;
- custom graph editing UI.

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

## What changed from the larger plan

The prior plan was directionally right but too abstract. This version collapses
several nouns:

| Larger-plan concept | Minimal replacement |
| --- | --- |
| `WorkflowRun` | `Run` |
| `WorkflowTask` | `Task` |
| `WorkflowArtifact` | `Artifact` |
| `TransitionProposal` | `Artifact { kind: "decision" }` or task result field |
| `WorkspaceLease` | one-writer rule enforced by plugin/scheduler |
| `WorkspaceInstance` table | workspace manager snapshot/source-ref record |
| budget/policy envelope | plugin-specific stopping conditions and allowed outputs |
| controller agent primitive | a normal task with role `controller` |
| workflow variables | artifacts |
| custom graph DSL | workflow plugins |

The core invariant remains:

> The daemon deterministically records state and starts/stops tasks. Agents
> perform work and return structured outputs. Workflow plugin code decides the
> next task from those outputs.

## Product model

Humans should see:

```text
Run
  goal, kind, status, current phase

Task cards
  role, status, latest summary, transcript link, controls

Artifacts
  handoffs, evidence, logs, diffs, source refs, snapshots, human notes

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
  <selected handoffs/evidence/source refs/snapshots>

Workspace:
  <path and source/snapshot/source-ref instructions>

Allowed outputs:
  - complete with handoff
  - fail with reason/evidence
  - request human
  - suggest next step, if this task is allowed to do that

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
  created_at timestamptz not null default now()
  updated_at timestamptz not null default now()
```

`params` contains plugin-specific input. Do not standardize more than needed.

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
  session_id text null references sessions(id)
  role text not null
    -- explore | implementer | reviewer | tester | kubernetes-tester |
    -- reducer | controller | merger | ...
  status text not null
    -- pending | running | blocked | done | failed | cancelled
  prompt text not null
  input_artifact_ids jsonb not null default '[]'
  output_artifact_id text null
  workspace jsonb not null default '{}'
  result jsonb null
  created_at timestamptz not null default now()
  updated_at timestamptz not null default now()
```

`workspace` is intentionally just a small instruction blob:

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
  "mode": "sources",
  "task_ids": ["candidate_a", "candidate_b"]
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
- `from_task` — task starts from a prior task's output snapshot/source.
- `sources` — task imports source refs/snapshots from multiple prior tasks.

One-writer safety is enforced by construction:

- tasks that write get their own fork;
- sequential tasks use `from_task`;
- reducers/mergers use `sources`;
- no two parallel tasks write the same directory.

No separate lease table is needed in v1.

### `artifacts`

```text
artifacts
  id text primary key
  run_id text not null references runs(id) on delete cascade
  task_id text null references tasks(id)
  kind text not null
    -- context | handoff | evidence | changes | snapshot | source_refs |
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
- source refs;
- snapshots;
- diffs;
- human approval requests;
- reducer summaries;
- hill-climb best-so-far records;
- controller/agent next-step suggestions.

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

## Workflow runner

The runner is simple:

1. load a run;
2. load tasks and artifacts;
3. reconcile linked child sessions;
4. if a task finished, record its result/artifact;
5. load the workflow plugin for `run.kind`;
6. call the plugin's `advance(run_state, tasks, artifacts)`;
7. create the next task(s), mark the run blocked, or mark the run done/failed.

The plugin owns control flow. The generic runner only persists and dispatches.

Pseudo-code:

```rust
fn advance(run: Run, tasks: Vec<Task>, artifacts: Vec<Artifact>) -> Advance {
    let plugin = workflow_registry.get(&run.kind)?;
    plugin.advance(run, tasks, artifacts)
}
```

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

start(params) -> initial Run state + initial Task(s)

advance(run, tasks, artifacts) -> next action
  create tasks | block | complete | fail
```

This is enough for discovery, invocation, validation, and UI rendering.

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
  "default_roles": ["kubernetes-tester"]
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
- reviewer sees handoff/diff/source refs and acceptance criteria;
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

Use when multiple implementers/candidates should race.

Human UX:

- see candidate task cards in parallel;
- see evaluator/merger card;
- choose/promote result.

Agent UX:

- each candidate agent gets same goal and separate fork;
- evaluator or merger sees candidate artifacts/source refs.

Control flow:

```text
start
  -> spawn N candidate tasks with fork_parent
  -> when done, spawn evaluator/merger with sources
  -> evaluator/merger writes winner or merged artifact
  -> done or blocked for human choice
```

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

Every task gets exactly one workspace instruction:

```json
{ "mode": "none" }
```

```json
{ "mode": "fork_parent" }
```

```json
{ "mode": "from_task", "task_id": "task_impl_1" }
```

```json
{ "mode": "sources", "task_ids": ["task_a", "task_b"] }
```

The workspace manager decides how to implement that instruction:

- Btrfs snapshot when available;
- reflink/copy fallback otherwise;
- git source refs for git workspaces when importing task outputs.

Do not expose Btrfs as a workflow concept. Btrfs is an implementation detail of
fast fork/seal/copy.

## Context transfer

Only artifacts transfer context between tasks.

Avoid:

- transcript scraping as the default;
- workflow variables;
- hidden global memory.

A task prompt should include:

- run goal;
- task goal;
- role instructions;
- selected prior artifact summaries;
- relevant source refs/snapshots;
- stopping condition;
- required output schema.

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

## PR notes

PR #150, `fix/subagent-orchestration-api-notifications`, is still important
because this minimal architecture needs parent-visible task lifecycle events:

- child spawned;
- child running;
- child idle/done;
- stale child recovery;
- event-driven UI refresh.

PR #149 is unrelated UI polish.

Older workflow PR branches are useful design history only. Do not revive the old
`session_relationships` table or workflow-variable design.

## Implementation phases

### Phase 0: lifecycle foundation

- Merge or port PR #150.
- Keep using `sessions.parent_session_id`.
- Ensure parent-visible child lifecycle events work reliably.

### Phase 1: minimal run/task/artifact tables

- Add `runs`.
- Add `tasks`.
- Add `artifacts`.
- Add repository methods.
- Add basic UI/RPC views.
- Add workflow plugin registry interfaces.
- Add bundled plugin manifests.

### Phase 2: task execution

- Start an agent task as a child session.
- Assemble scoped task prompt from run/task/artifacts.
- Record task result and output artifact when child session finishes.
- Support `task.write_artifact`, `task.complete`, `task.fail`,
  `task.request_human`.
- Ensure workflow task agents are non-recursive by default.

### Phase 3: workflow plugin tools

- Add `workflow.list`.
- Add `workflow.describe`.
- Add `workflow.start` by plugin id.
- Add `workflow.status`, `workflow.cancel`, `workflow.signal`,
  `workflow.read_artifact`.

### Phase 4: bundled plugins

Implement bundled plugins in this order:

1. `explore`;
2. `implement_review`;
3. `implement_review_test`;
4. `kubernetes_e2e`;
5. `parallel_race`;
6. `hill_climb`.

### Phase 5: workspace sources

- Implement task `workspace.mode`.
- Use existing Btrfs snapshot/fork support where available.
- Use git source refs for `sources`.
- Keep one-writer safety by construction.

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
   table.
4. If workspace safety can be achieved by forking, do not add leases yet.
5. If a workflow can be a plugin, do not create a DSL yet.
6. Keep the human UX to runs, task cards, artifacts, and human requests.
7. Keep the agent UX to scoped task prompts and structured task results.
8. Workflow task agents are scoped and non-recursive by default.
9. Direct delegation agents may remain async/interruptible because they are still
   useful outside formal workflows.
10. Workflow discovery and invocation should be regular tool calls, not Python
    REPL code.
