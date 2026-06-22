import type { Stage, StageStatus, StageSubagent } from "./types.ts";
import type { StartFullStageParams, StartReadonlyFanoutParams } from "./agentApi.ts";

/** A stage is in flight (and therefore cancellable / its subagents pollable)
 * exactly while its status is `running`. Every other status is terminal. */
export function isStageRunning(stage: Stage): boolean {
	return stage.status === "running";
}

/** The daemon only writes handoff files from the stage barrier, which completes
 * stages as `done` or `done_with_failures`. Other terminal states are real, but
 * do not have an index.json or per-subagent handoff files behind them. */
export function stageHasHandoff(stage: Stage): boolean {
	return stage.status === "done" || stage.status === "done_with_failures";
}

const STAGE_STATUS_LABELS: Record<StageStatus, string> = {
	running: "running",
	done: "done",
	done_with_failures: "done with failures",
	cancelled: "cancelled",
	failed: "failed",
};

export function stageStatusLabel(status: StageStatus): string {
	return STAGE_STATUS_LABELS[status] ?? status;
}

/** Only a full stage's single full subagent can be steered; read-only fan-out
 * members are fire-and-forget and the daemon rejects steering them. Returns the
 * steerable subagent id, or null when nothing in the stage is steerable. */
export function steerableSubagentId(stage: Stage): string | null {
	if (!isStageRunning(stage)) return null;
	const full = stage.subagents.find((subagent) => subagent.subagent_type === "full");
	return full ? full.id : null;
}

/** Whether a stage can be re-run from the board. Keep this predicate in lockstep
 * with the actual reconstruction path so the UI never offers a re-run that the
 * click handler will reject. */
export function canReRunStage(stage: Stage): boolean {
	return reRunTaskPlan(stage) !== null;
}

function isReRunnableStageStatus(status: StageStatus): boolean {
	switch (status as string) {
		case "done":
		case "done_with_failures":
		case "cancelled":
		case "failed":
			return true;
		default:
			return false;
	}
}

function subagentTask(subagent: StageSubagent): { role: string; prompt: string } | null {
	const prompt = subagent.task;
	const role = subagent.role;
	if (typeof prompt !== "string" || !prompt.trim()) return null;
	if (typeof role !== "string" || !role.trim()) return null;
	return { role, prompt };
}

type ReRunTask = { role: string; prompt: string };
type ReRunTaskPlan =
	| { kind: "full"; task: ReRunTask }
	| { kind: "readonly_fanout"; tasks: ReRunTask[] };

function reRunTaskPlan(stage: Stage): ReRunTaskPlan | null {
	if (!isReRunnableStageStatus(stage.status)) return null;
	if (stage.subagents.length === 0) return null;
	const tasks = stage.subagents.map((subagent) => subagentTask(subagent));
	if (tasks.some((task) => task === null)) return null;
	const resolved = tasks as ReRunTask[];
	const kind = stage.kind as string;
	if (kind === "full") {
		const only = resolved[0];
		if (!only || resolved.length !== 1) return null;
		return { kind: "full", task: only };
	}
	if (kind === "readonly_fanout") {
		return { kind: "readonly_fanout", tasks: resolved };
	}
	return null;
}

/** Reconstruct the `stage.start_*` params to re-run a finished stage. The board
 * receives the original prompts as `StageSubagent.task`. Returns the kind-tagged
 * params, or null when any prompt/role is missing (re-run is then disabled). */
export function reRunParamsForStage(
	stage: Stage,
	parentSessionId: string,
):
	| { kind: "full"; params: StartFullStageParams }
	| { kind: "readonly_fanout"; params: StartReadonlyFanoutParams }
	| null {
	const plan = reRunTaskPlan(stage);
	if (!plan) return null;
	if (plan.kind === "full") {
		return {
			kind: "full",
			params: {
				parentSessionId,
				role: plan.task.role,
				prompt: plan.task.prompt,
				workflow: stage.workflow ?? undefined,
				label: stage.label ?? undefined,
			},
		};
	}
	return {
		kind: "readonly_fanout",
		params: {
			parentSessionId,
			tasks: plan.tasks,
			workflow: stage.workflow ?? undefined,
			label: stage.label ?? undefined,
		},
	};
}
