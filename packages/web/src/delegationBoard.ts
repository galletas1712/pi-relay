import type {
	Activity,
	Delegation,
	DelegationProgress,
	DelegationStatus,
	DelegationSubagent,
} from "./types.ts";

export type AgentStatus = Activity | DelegationStatus;
export type AgentStatusIconKey =
	| "running"
	| "done"
	| "done-with-failures"
	| "failed"
	| "cancelled"
	| "queued"
	| "idle"
	| "unknown";

const AGENT_STATUS_ICON_KEYS: Record<AgentStatus, AgentStatusIconKey> = {
	running: "running",
	done: "done",
	done_with_failures: "done-with-failures",
	failed: "failed",
	cancelled: "cancelled",
	queued: "queued",
	idle: "idle",
};

/** Keep meaningful delegation states shape-distinct. Unknown future statuses use
 * their own fallback shape rather than borrowing a known state's semantics. */
export function agentStatusIconKey(status: string): AgentStatusIconKey {
	return AGENT_STATUS_ICON_KEYS[status as AgentStatus] ?? "unknown";
}

export interface DelegationProgressSummary {
	expected: number | null;
	spawned: number;
	terminal: number;
	running: number;
	failed: number;
	source: "server" | "children";
}

const TERMINAL_SUBAGENT_STATUSES = new Set([
	"done",
	"done_with_failures",
	"cancelled",
	"failed",
]);

export function delegationNeedsAttention(delegation: Delegation): boolean {
	if (delegation.status === "failed" || delegation.status === "done_with_failures") return true;
	if (delegation.status === "cancelled") return false;
	if (delegation.progress && delegation.progress.failed > 0) return true;
	if (
		delegation.subagents.some(
			(subagent) => subagent.status === "failed" || subagent.status === "done_with_failures",
		)
	) {
		return true;
	}
	return false;
}

/** Preserve server order within Needs attention, Active, then Recent. */
export function orderDelegations(delegations: readonly Delegation[]): Delegation[] {
	const attention: Delegation[] = [];
	const active: Delegation[] = [];
	const recent: Delegation[] = [];
	for (const delegation of delegations) {
		if (delegationNeedsAttention(delegation)) attention.push(delegation);
		else if (delegation.status === "running") active.push(delegation);
		else recent.push(delegation);
	}
	return [...attention, ...active, ...recent];
}

function directChildProgress(subagents: readonly DelegationSubagent[]): DelegationProgressSummary {
	let terminal = 0;
	let running = 0;
	let failed = 0;
	for (const subagent of subagents) {
		const status = typeof subagent.status === "string" ? subagent.status : "idle";
		if (TERMINAL_SUBAGENT_STATUSES.has(status)) {
			terminal += 1;
			if (status === "failed" || status === "done_with_failures") failed += 1;
			continue;
		}
		if (status === "running" || subagent.activity === "running") running += 1;
	}
	return {
		expected: null,
		spawned: subagents.length,
		terminal,
		running,
		failed,
		source: "children",
	};
}

export function delegationProgressSummary(delegation: Delegation): DelegationProgressSummary {
	const progress: DelegationProgress | null | undefined = delegation.progress;
	if (!progress) return directChildProgress(delegation.subagents);
	return {
		expected: progress.expected,
		spawned: progress.spawned,
		terminal: progress.terminal,
		running: progress.running,
		failed: progress.failed,
		source: "server",
	};
}

export function remainingDelegationWorkCount(delegation: Delegation): {
	count: number;
	unit: "agents" | "agents/slots";
} {
	const progress = delegationProgressSummary(delegation);
	if (progress.expected !== null) {
		return {
			count: Math.max(0, progress.expected - progress.terminal),
			unit: "agents/slots",
		};
	}
	return {
		count: Math.max(0, progress.spawned - progress.terminal),
		unit: "agents",
	};
}

/** A delegation is in flight (and therefore cancellable / its subagents pollable)
 * exactly while its status is `running`. Every other status is terminal. */
export function isDelegationRunning(delegation: Delegation): boolean {
	return delegation.status === "running";
}

/** Map a delegation or subagent status to a CSS-safe icon modifier. */
export function statusIconClass(status: string): string {
	switch (status) {
		case "running":
			return "running";
		case "done":
			return "done";
		case "done_with_failures":
			return "warn";
		case "failed":
			return "failed";
		case "cancelled":
			return "cancelled";
		default:
			return "pending";
	}
}
