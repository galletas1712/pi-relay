---
name: workflow-explore
description: Parallel read-only exploration of a question, then synthesis. Use when you want several independent angles investigated at once before deciding anything.
---

# Workflow: explore

Investigate a question from several angles in parallel (read-only), then
synthesize the findings yourself. Nothing is changed in the workspace.

## Delegations
- explorer — read-only subagent(s), run in parallel, one per angle.

## Outcomes (suggested_next, in inspect_delegation)
- explorer: done | inconclusive

## Control flow
1. Decide the angles (e.g. "current behavior", "prior art in the repo",
   "failure modes", "simplest option"). One explorer per angle.
2. Run a single read-only fan-out with all explorers.
3. When the handoff steer arrives, call `inspect_delegation` and read each
   explorer's final_message.md if you need more detail. Synthesize the answer
   yourself (you are the reducer — there is no reducer subagent).
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
- Explorers return only their final message (their snapshot is discarded), so
  instruct each to summarize findings and quote key evidence inline, and to end
  with a line `suggested_next: done` or `suggested_next: inconclusive`.
- This workflow never edits the workspace; do not use a full delegation here.
