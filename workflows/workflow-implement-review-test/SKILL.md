---
name: workflow-implement-review-test
description: Implement, loop implementer<->reviewer until approved, then test; test failures send it back to implement and the loop restarts. Use for changes that must pass review and tests.
---

# Workflow: implement -> review -> test

Implement a change, review it until a reviewer is satisfied, then test. Test
failures send it back to the implementer and the loop restarts. You drive this;
branch on the typed outcomes each subagent reports in the handoff index.json.

## Stages
- implementer — full subagent (writes the workspace in place).
- reviewer    — read-only subagent(s) (review only; never write).
- tester      — full subagent (runs the suite; reports results).

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
                          (re-review before re-testing)
   - environment_issue -> re-run test once; if it recurs, ask the human
4. Termination: if review has not converged after ~3 rounds, stop and ask the
   human.

## Running each stage (one stage per turn, then end your turn)
- implement: delegate_writing_task({ role: "implementer",
    prompt: "<goal + latest review/test notes>", workflow: "implement_review_test" })
- review:    delegate_readonly_tasks({ tasks: [ { role: "reviewer",
    prompt: "<what to review + acceptance criteria>" } ], workflow: "implement_review_test" })
- test:      delegate_writing_task({ role: "tester",
    prompt: "<how to test: command(s), what 'pass' means>", workflow: "implement_review_test" })

Notes:
- The tester is a full stage because building/running tests writes the workspace
  (build outputs); it edits in place like the implementer.
- After launching a stage, end your turn; you will be steered when it completes.
- Subagents start fresh — carry the prior stage's findings (from the handoff
  files) into the next stage's prompt.
- In each subagent's prompt, REQUIRE it to end its final message with a line
  `suggested_next: <one of the outcomes above>` — that line is what the handoff
  index.json records and you branch on; without it the recorded outcome is null.
- Do not mark DONE without a tester `pass`.
