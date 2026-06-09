"""Editable worker/reviewer/tester handoff loop.

Copy this file and workflow_sdk.py into a session cwd, bind `rpc`, edit the task-specific
strings, then run/rerun. This template is for non-metric tasks where pass/fail review and tests are
more important than optimizing one scalar.
"""

from __future__ import annotations

from workflow_sdk import WorkflowClient


def run_review_loop(wf: WorkflowClient, *, max_rounds: int = 3) -> None:
    wf.write_var("workflow_state", {"round": 0, "status": "running", "max_rounds": max_rounds})

    for round_id in range(1, max_rounds + 1):
        wf.write_var("workflow_state", {"round": round_id, "status": "implementing", "max_rounds": max_rounds})
        impl_var = f"implementation_round_{round_id}"
        review_var = f"review_round_{round_id}"
        test_var = f"test_round_{round_id}"

        wf.spawn_subagent(
            role="worker",
            child_session_id=f"{wf.workflow_id}_worker_{round_id}",
            result_var=impl_var,
            context_vars=["workflow_brief", "workflow_state", "artifact_manifest"],
            task=(
                "Implement the next revision for the workflow objective. Keep the editable surface "
                "small, explain changed artifacts, and write the result variable with summary, "
                "artifacts, and any patch/content needed by the parent."
            ),
        )
        wf.await_vars([impl_var])

        wf.spawn_subagent(
            role="reviewer",
            child_session_id=f"{wf.workflow_id}_reviewer_{round_id}",
            result_var=review_var,
            context_vars=["workflow_brief", "workflow_state", "artifact_manifest", impl_var],
            task=(
                "Review the implementation against the workflow brief. Return JSON with pass: bool, "
                "blocking_issues, nonblocking_issues, and recommended_next_step."
            ),
        )
        wf.await_vars([review_var])
        review = wf.read_var(review_var) or {}

        wf.spawn_subagent(
            role="tester",
            child_session_id=f"{wf.workflow_id}_tester_{round_id}",
            result_var=test_var,
            context_vars=["workflow_brief", "workflow_state", "artifact_manifest", impl_var, review_var],
            task="Run or design the appropriate validation. Return JSON with pass, commands, evidence, and failures.",
        )
        wf.await_vars([test_var])
        test = wf.read_var(test_var) or {}

        passed = bool(review.get("pass")) and bool(test.get("pass"))
        if passed:
            wf.write_var("workflow_state", {"round": round_id, "status": "passed", "max_rounds": max_rounds})
            return

    wf.write_var("workflow_state", {"round": max_rounds, "status": "needs_user_or_more_rounds", "max_rounds": max_rounds})
