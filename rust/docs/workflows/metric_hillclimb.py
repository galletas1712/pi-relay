"""Editable autoresearch-style metric optimization loop.

Use when success has a scalar metric and target, e.g. CIFAR-10 validation accuracy >= 0.95.
The loop records hypotheses, implementation summaries, verification, evaluation metrics, and
accept/reject decisions in workflow variables.
"""

from __future__ import annotations

from workflow_sdk import WorkflowClient


def run_metric_hillclimb(
    wf: WorkflowClient,
    *,
    metric_name: str,
    target: float,
    max_iterations: int,
    higher_is_better: bool = True,
) -> None:
    best = None
    wf.write_var(
        "workflow_state",
        {
            "status": "running",
            "iteration": 0,
            "metric_name": metric_name,
            "target": target,
            "best": best,
            "max_iterations": max_iterations,
        },
    )

    for iteration in range(1, max_iterations + 1):
        state = {
            "status": "planning",
            "iteration": iteration,
            "metric_name": metric_name,
            "target": target,
            "best": best,
            "max_iterations": max_iterations,
        }
        wf.write_var("workflow_state", state)
        proposal_var = f"experiment_proposal_{iteration}"
        implementation_var = f"implementation_summary_{iteration}"
        review_var = f"review_report_{iteration}"
        metric_var = f"metric_report_{iteration}"
        decision_var = f"decision_{iteration}"

        wf.spawn_subagent(
            role="worker",
            child_session_id=f"{wf.workflow_id}_proposal_{iteration}",
            result_var=proposal_var,
            context_vars=["workflow_brief", "workflow_state", "artifact_manifest"],
            task=(
                "Propose exactly one next experiment likely to improve the metric. Include hypothesis, "
                "expected effect, editable surface, training/evaluation budget, and risk."
            ),
        )
        wf.await_vars([proposal_var])

        wf.spawn_subagent(
            role="worker",
            child_session_id=f"{wf.workflow_id}_worker_{iteration}",
            result_var=implementation_var,
            context_vars=["workflow_brief", "workflow_state", "artifact_manifest", proposal_var],
            task=(
                "Implement the proposed experiment with a small editable surface. Return artifacts or "
                "patch/content, exact commands, and assumptions. Do not claim metric success."
            ),
        )
        wf.await_vars([implementation_var])

        wf.spawn_subagent(
            role="reviewer",
            child_session_id=f"{wf.workflow_id}_reviewer_{iteration}",
            result_var=review_var,
            context_vars=["workflow_brief", "workflow_state", proposal_var, implementation_var],
            task="Review the implementation and run any fast static checks. Return JSON with pass, blockers, commands, and evidence.",
        )
        wf.await_vars([review_var])
        review_report = wf.read_var(review_var) or {}
        if review_report.get("pass") is False:
            decision = {"accepted": False, "reason": "review failed", "metric": None}
            wf.write_var(decision_var, decision)
            continue

        wf.spawn_subagent(
            role="tester",
            child_session_id=f"{wf.workflow_id}_tester_{iteration}",
            result_var=metric_var,
            context_vars=["workflow_brief", "workflow_state", proposal_var, implementation_var, review_var],
            task=(
                f"Run the agreed evaluation and report real {metric_name}. Return JSON with metric, "
                "target_reached, command, wall_time, hardware, and evidence. If only a smoke/proxy run "
                "was possible, set target_reached=false and explain why."
            ),
        )
        wf.await_vars([metric_var], timeout_ms=120_000)
        metric_report = wf.read_var(metric_var) or {}
        metric = metric_report.get("metric")
        improved = metric is not None and (
            best is None or (metric > best["metric"] if higher_is_better else metric < best["metric"])
        )
        target_reached = metric is not None and (metric >= target if higher_is_better else metric <= target)
        if improved:
            best = {"metric": metric, "iteration": iteration, "metric_var": metric_var, "implementation_var": implementation_var}

        decision = {
            "accepted": bool(improved),
            "target_reached": bool(target_reached),
            "metric": metric,
            "best": best,
            "metric_var": metric_var,
            "implementation_var": implementation_var,
        }
        wf.write_var(decision_var, decision)
        wf.write_var(
            "workflow_state",
            {
                "status": "target_reached" if target_reached else "running",
                "iteration": iteration,
                "metric_name": metric_name,
                "target": target,
                "best": best,
                "max_iterations": max_iterations,
            },
        )
        if target_reached:
            return

    wf.write_var(
        "workflow_state",
        {
            "status": "target_not_reached",
            "iteration": max_iterations,
            "metric_name": metric_name,
            "target": target,
            "best": best,
            "max_iterations": max_iterations,
        },
    )
