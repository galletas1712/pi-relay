"""Editable parallel candidate race template.

Use when several approaches should compete under the same reviewer/evaluator. The root compares
result variables and accepts the best candidate; it can then feed the winner into a review loop or
metric hillclimb.
"""

from __future__ import annotations

from workflow_sdk import WorkflowClient


def run_parallel_race(wf: WorkflowClient, *, candidate_count: int = 3) -> None:
    wf.write_var("workflow_state", {"status": "racing", "candidate_count": candidate_count})

    proposal_vars = []
    for idx in range(1, candidate_count + 1):
        var = f"candidate_{idx}"
        proposal_vars.append(var)
        wf.spawn_subagent(
            role="worker",
            child_session_id=f"{wf.workflow_id}_candidate_{idx}",
            result_var=var,
            context_vars=["workflow_brief", "workflow_state", "artifact_manifest"],
            task=(
                f"Create candidate approach {idx}. Optimize for the workflow brief, keep scope small, "
                "and write a structured result with artifacts, expected strengths, and risks."
            ),
        )

    wf.await_vars(proposal_vars)
    compare_var = "race_comparison"
    wf.spawn_subagent(
        role="reviewer",
        child_session_id=f"{wf.workflow_id}_race_reviewer",
        result_var=compare_var,
        context_vars=["workflow_brief", "workflow_state", *proposal_vars],
        task=(
            "Compare all candidates under the same criteria. Return JSON with winner, ranking, "
            "blocking issues, and recommended next workflow step."
        ),
    )
    wf.await_vars([compare_var])
    comparison = wf.read_var(compare_var)
    wf.write_var("workflow_state", {"status": "race_complete", "comparison": comparison})
