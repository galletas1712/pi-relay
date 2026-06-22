# Phase 0 doc edits (apply when delegation tools land)

> **Historical/superseded.** These staged edits were for the delegation-tool
> landing and have been applied/polished in the live docs. Do not apply this
> file as instructions; use `rust/docs/architecture.md`,
> `rust/docs/modules/agent-daemon.md`, and `rust/docs/modules/agent-tools.md`
> for current wording.

These are the exact documentation edits for Phase 0 of
`../workflow-orchestration.md`. They are staged here (not applied to the live
docs) because they describe the delegation-tool surface and the
one-durable-workspace model, which do not exist yet. Apply them in the same
change that lands the delegation tools so the docs never describe unbuilt
behavior.

The current docs were updated by #151 and now describe subagent delegation as
spawn/control through the Python REPL `subagents` module, with optional
forked-context snapshots and git source-refs. This design replaces that surface.

---

## Edit 1 — `rust/docs/architecture.md`, Goal 6

Replace:

> 6. Support bounded parent/child subagent delegation (spawn/list/wait/steer/
>    interrupt over forked sessions) without a generic injected-message routing
>    layer or event bus between arbitrary sessions.

with:

> 6. Support bounded parent/child subagent delegation as **stages**: the parent
>    runs one full (writing) subagent or a parallel fan-out of read-only
>    subagents, parks, and is steered with a completion notification. No generic
>    injected-message routing layer or event bus between arbitrary sessions.

## Edit 2 — `rust/docs/architecture.md`, "Subagent delegation" feature bullet

Replace the bullet that currently reads (paraphrased): "a parent spawns child
sessions by role/skill (optionally with a forked context snapshot and git
source-refs), then lists, waits on, reads, steers, and interrupts them …
spawn and control flow through the in-process Python REPL `subagents` module"

with:

> - Subagent delegation runs as **stages** through provider-visible delegation
>   tools (`delegate_writing_task`, `delegate_readonly_tasks`,
>   `inspect_delegation`, `cancel_delegation`).
>   A stage is one **full** subagent (writes the parent's workspace in place) or a
>   parallel fan-out of **read-only** subagents (each in a disposable btrfs
>   snapshot, destroyed on return). The parent parks after launching a stage and
>   is delivered a parent-scoped completion **steer** pointing at a handoff
>   directory (`index.json` + per-subagent final message and transcript).
>   `subagent.{spawned,running,idle}` lifecycle events drive the stage barrier.
>   Reusable patterns are **workflow skills** (`SKILL.md` + `LoadSkill`), not a
>   DSL. The Python REPL `subagents` module remains only as a raw escape hatch.
>   See [agent-daemon](modules/agent-daemon.md).

## Edit 3 — `rust/docs/architecture.md`, "Not implemented by design"

Add:

> - Cross-subagent workspace merging. There is one durable workspace with a single
>   writer in time; read-only subagents are isolated in throwaway snapshots and
>   never merged back. (The legacy `subagents.spawn(sources=…)` git-source-ref
>   merge path is removed by this design; see
>   `plans/subagent-source-ref-merge-plan.md`, retained as history.)
> - Daemon-executed workflow graphs/DSLs and a workflow variable store. Workflow
>   control flow lives in parent-interpreted skills.

## Edit 4 — `rust/docs/modules/agent-daemon.md`

In the subagent/runtime section, replace the description of REPL-driven
spawn/wait/steer with a short summary of the stage runtime:

> Subagent work runs as **stages** (`delegate_writing_task` /
> `delegate_readonly_tasks` / `inspect_delegation` / `cancel_delegation`). Full
> subagents reuse the parent's workspace dirs in place; read-only subagents get a
> forked snapshot destroyed on return. A stage runner watches
> `subagent.{spawned,running,idle}` events, applies a single-flight,
> `attempt_id`-fenced barrier when all subagents of a stage are terminal, writes
> the handoff directory, and enqueues one `InputPriority::Steer` notification to
> the parent. The runner never decides the next stage — the parent does, guided
> by workflow skills.

## Edit 5 — remove the source-ref doc

When the `subagents.spawn(sources=…)` source-ref path is removed from
`repl.rs`/`subagents.rs`, delete `rust/docs/subagent-source-ref-merge-plan.md`
(it documents that now-removed feature) and drop its reference from this plan.
Until then it stays, because it documents shipping code.
