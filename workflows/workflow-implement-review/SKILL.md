---
name: workflow-implement-review
description: Implement a change, then loop implementer<->reviewer until a reviewer approves. Use when a change should land cleanly but does not need a separate test delegation.
---

# Workflow: implement -> review

Implement a change, then loop with a reviewer until the reviewer approves. You
drive the loop; branch on the typed outcomes in the delivered delegation
snapshot.

## Delegations
- implementer — full subagent (writes the workspace in place).
- reviewer    — read-only subagent(s) (review only; never write).

## Outcomes (outcome, in the delegation snapshot)
- reviewer: approved | changes_requested

## Control flow
1. implement
2. review
   - approved          -> DONE
   - changes_requested -> implement again (pass the reviewer notes) -> 2
3. Termination: if review has not converged after ~3 rounds, stop and ask the
   human rather than looping indefinitely.

## Running each delegation (one delegation per turn, then end your turn)
- implement: delegate_writing_task({ role: "implementer",
    prompt: "<goal + latest reviewer notes>", workflow: "implement_review" })
- review:    delegate_readonly_tasks({ tasks: [ { role: "reviewer",
    prompt: "<what to review + acceptance criteria>" } ], workflow: "implement_review" })

Notes:
- After launching a delegation, end your turn; you will receive a completion observation when it
  completes with an `inspect_delegation`-equivalent snapshot.
- Subagents start fresh — carry prior control-flow facts from the delivered
  snapshot into the next delegation's prompt. Read transcript/final-message
  files only when more detail is needed.
- In each reviewer's prompt, REQUIRE it to end its final message with a line
  `outcome: approved` or `outcome: changes_requested` — that line
  is what the delivered snapshot records and you branch on.
- While the implementer (full) runs, supervise and read; do not edit yourself.
- A single reviewer is usually enough; fan out multiple reviewers only when you
  want distinct lenses (e.g. correctness vs security).
