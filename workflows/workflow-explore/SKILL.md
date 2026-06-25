---
name: workflow-explore
description: Parallel read-only exploration of a question, then synthesis. Use when you want several independent angles investigated at once before deciding anything.
---

# Workflow: explore

Investigate a question from several angles in parallel (read-only), then
synthesize the findings yourself. Nothing is changed in the workspace.

## Delegations
- explorer — read-only subagent(s), run in parallel, one per angle.

## Outcomes (outcome, in the delegation snapshot)
- explorer: done | inconclusive

## Control flow
1. Decide the angles (e.g. "current behavior", "prior art in the repo",
   "failure modes", "simplest option"). One explorer per angle.
2. Run a single read-only fan-out with all explorers.
3. When a wakeup observation arrives, branch on the delivered delegation
   snapshot. If it is still `running`, decide only for that current fan-out:
   steer a running/steerable explorer, cancel the delegation, or end your turn
   and wait; do not start another delegation yet. If it is terminal, carry
   control-flow facts from that snapshot into your synthesis; read an
   explorer's final_message.md or transcript only when you need more detail.
   Call `inspect_delegation` only to refresh/recover state or inspect later.
   Synthesize the answer yourself (you are the reducer — there is no reducer
   subagent).
4. If key angles came back `inconclusive` or revealed new questions, run another
   read-only fan-out with refined prompts. Stop when you can answer confidently
   or the user should weigh in.

## Running the delegation (one delegation per turn, then end your turn)
- explore: delegate_readonly_tasks({
    tasks: [
      { role: "explore", prompt: "<angle 1: question + where to look>" },
      { role: "explore", prompt: "<angle 2: ...>" }
    ],
    workflow: "explore" })

Notes:
- Explorers start with fresh context: put the question and any pointers (files,
  prior handoff paths) in each prompt.
- Explorers write detailed findings in their final message artifact. The
  delivered/refreshed delegation snapshot carries control-flow facts such as
  `outcome`, status/progress, and artifact refs; read `final_message.md`
  or `transcript.md` via those refs when more detail is needed. Instruct each
  explorer to end with a line `outcome: done` or
  `outcome: inconclusive`.
- This workflow never edits the workspace; do not use a full delegation here.
