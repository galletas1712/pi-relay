import type { SessionSummary, Stage, StageStatus, StageSubagent } from "./types.ts";
import type { StartFullStageParams, StartReadonlyFanoutParams } from "./agentApi.ts";

/** A stage is in flight (and therefore cancellable / its subagents pollable)
 * exactly while its status is `running`. Every other status is terminal. */
export function isStageRunning(stage: Stage): boolean {
	return stage.status === "running";
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

/** Whether a stage can be re-run from the board: its prompts must be
 * recoverable for every member (see `reRunParamsForStage`). A still-running
 * stage is never re-runnable (cancel it first). */
export function canReRunStage(stage: Stage, taskBySessionId: Map<string, string | null>): boolean {
	if (isStageRunning(stage)) return false;
	if (stage.subagents.length === 0) return false;
	return stage.subagents.every((subagent) => {
		const task = taskBySessionId.get(subagent.id);
		return typeof task === "string" && task.trim().length > 0;
	});
}

function subagentTask(
	subagent: StageSubagent,
	taskBySessionId: Map<string, string | null>,
): { role: string; prompt: string } | null {
	const prompt = taskBySessionId.get(subagent.id);
	const role = subagent.role;
	if (typeof prompt !== "string" || !prompt.trim()) return null;
	if (typeof role !== "string" || !role.trim()) return null;
	return { role, prompt };
}

/** Reconstruct the `stage.start_*` params to re-run a finished stage. The board
 * does not carry the original prompts, so they are recovered from each
 * subagent's session metadata (`metadata.task`). Returns the kind-tagged params,
 * or null when any prompt/role is missing (re-run is then disabled). */
export function reRunParamsForStage(
	stage: Stage,
	parentSessionId: string,
	taskBySessionId: Map<string, string | null>,
):
	| { kind: "full"; params: StartFullStageParams }
	| { kind: "readonly_fanout"; params: StartReadonlyFanoutParams }
	| null {
	const tasks = stage.subagents.map((subagent) => subagentTask(subagent, taskBySessionId));
	if (tasks.some((task) => task === null)) return null;
	const resolved = tasks as { role: string; prompt: string }[];
	if (stage.kind === "full") {
		const only = resolved[0];
		if (!only || resolved.length !== 1) return null;
		return {
			kind: "full",
			params: {
				parentSessionId,
				role: only.role,
				prompt: only.prompt,
				workflow: stage.workflow ?? undefined,
				label: stage.label ?? undefined,
			},
		};
	}
	return {
		kind: "readonly_fanout",
		params: {
			parentSessionId,
			tasks: resolved,
			workflow: stage.workflow ?? undefined,
			label: stage.label ?? undefined,
		},
	};
}

/** The subagent task prompt persisted on each subagent session
 * (`metadata.task`), keyed by session id, for re-run reconstruction. */
export function taskBySessionId(summaries: SessionSummary[]): Map<string, string | null> {
	const map = new Map<string, string | null>();
	for (const summary of summaries) {
		const task = summary.metadata?.task;
		map.set(summary.session_id, typeof task === "string" ? task : null);
	}
	return map;
}
