import type { Delegation, DelegationStatus, DelegationSubagent } from "./types.ts";
import type { StartFullDelegationParams, StartReadonlyDelegationFanoutParams } from "./agentApi.ts";

/** A delegation is in flight (and therefore cancellable / its subagents pollable)
 * exactly while its status is `running`. Every other status is terminal. */
export function isDelegationRunning(delegation: Delegation): boolean {
	return delegation.status === "running";
}

/** The daemon only writes handoff files from the delegation barrier, which
 * completes delegations as `done` or `done_with_failures`. Other terminal
 * states are real, but do not have an index.json or per-subagent handoff files
 * behind them. */
export function delegationHasHandoff(delegation: Delegation): boolean {
	return delegation.status === "done" || delegation.status === "done_with_failures";
}

const DELEGATION_STATUS_LABELS: Record<DelegationStatus, string> = {
	running: "running",
	done: "done",
	done_with_failures: "done with failures",
	cancelled: "cancelled",
	failed: "failed",
};

export function delegationStatusLabel(status: DelegationStatus): string {
	return DELEGATION_STATUS_LABELS[status] ?? status;
}

/** Only a full delegation's single full subagent can be steered; read-only fan-out
 * members are fire-and-forget and the daemon rejects steering them. Returns the
 * steerable subagent id, or null when nothing in the delegation is steerable. */
export function steerableSubagentId(delegation: Delegation): string | null {
	if (!isDelegationRunning(delegation)) return null;
	const full = delegation.subagents.find((subagent) => subagent.subagent_type === "full");
	return full ? full.id : null;
}

/** Whether a delegation can be re-run from the board. Keep this predicate in lockstep
 * with the actual reconstruction path so the UI never offers a re-run that the
 * click handler will reject. */
export function canReRunDelegation(delegation: Delegation): boolean {
	return reRunTaskPlan(delegation) !== null;
}

function isReRunnableDelegationStatus(status: DelegationStatus): boolean {
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

function subagentTask(subagent: DelegationSubagent): { role: string; prompt: string } | null {
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

function reRunTaskPlan(delegation: Delegation): ReRunTaskPlan | null {
	if (!isReRunnableDelegationStatus(delegation.status)) return null;
	if (delegation.subagents.length === 0) return null;
	const tasks = delegation.subagents.map((subagent) => subagentTask(subagent));
	if (tasks.some((task) => task === null)) return null;
	const resolved = tasks as ReRunTask[];
	const kind = delegation.kind as string;
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

/** Reconstruct the `delegation.start_*` params to re-run a finished delegation. The board
 * receives the original prompts as `DelegationSubagent.task`. Returns the kind-tagged
 * params, or null when any prompt/role is missing (re-run is then disabled). */
export function reRunParamsForDelegation(
	delegation: Delegation,
	parentSessionId: string,
):
	| { kind: "full"; params: StartFullDelegationParams }
	| { kind: "readonly_fanout"; params: StartReadonlyDelegationFanoutParams }
	| null {
	const plan = reRunTaskPlan(delegation);
	if (!plan) return null;
	if (plan.kind === "full") {
		return {
			kind: "full",
			params: {
				parentSessionId,
				role: plan.task.role,
				prompt: plan.task.prompt,
				workflow: delegation.workflow ?? undefined,
				label: delegation.label ?? undefined,
			},
		};
	}
	return {
		kind: "readonly_fanout",
		params: {
			parentSessionId,
			tasks: plan.tasks,
			workflow: delegation.workflow ?? undefined,
			label: delegation.label ?? undefined,
		},
	};
}
