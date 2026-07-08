import type {
	Delegation,
	DelegationKind,
	DelegationProgress,
	DelegationStatus,
	DelegationSubagent,
} from "./types.ts";
import type { StartFullDelegationParams, StartReadonlyDelegationFanoutParams } from "./agentApi.ts";

export type DelegationSectionId = "needs-attention" | "active" | "recent";

export interface DelegationSection {
	id: DelegationSectionId;
	label: string;
	delegations: Delegation[];
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

const DELEGATION_KIND_LABELS: Record<DelegationKind, string> = {
	full: "Writing task",
	readonly_fanout: "Parallel research",
};

export function delegationKindLabel(kind: DelegationKind): string {
	return DELEGATION_KIND_LABELS[kind] ?? "Agent task";
}

export function humanizeDelegationValue(value: string): string {
	const normalized = value.trim().replaceAll("_", " ").replace(/\s+/g, " ");
	return normalized ? normalized[0].toUpperCase() + normalized.slice(1) : normalized;
}

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

/** Split a server-ordered page into stable task-centered sections. Filtering
 * preserves the input order inside every section, so polling cannot reshuffle
 * peers that have not changed classification. */
export function groupDelegations(delegations: readonly Delegation[]): DelegationSection[] {
	const groups: Record<DelegationSectionId, Delegation[]> = {
		"needs-attention": [],
		active: [],
		recent: [],
	};
	for (const delegation of delegations) {
		if (delegationNeedsAttention(delegation)) groups["needs-attention"].push(delegation);
		else if (delegation.status === "running") groups.active.push(delegation);
		else groups.recent.push(delegation);
	}
	return [
		{ id: "needs-attention", label: "Needs attention", delegations: groups["needs-attention"] },
		{ id: "active", label: "Active", delegations: groups.active },
		{ id: "recent", label: "Recent", delegations: groups.recent },
	];
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

export function formatDelegationProgress(delegation: Delegation): string {
	const progress = delegationProgressSummary(delegation);
	const prefix =
		progress.expected === null
			? `${progress.spawned} agent${progress.spawned === 1 ? "" : "s"} shown`
			: `${progress.expected} expected · ${progress.spawned} spawned`;
	return `${prefix} · ${progress.terminal} terminal · ${progress.running} running · ${progress.failed} failed`;
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

export function delegationOutcomeText(delegation: Delegation): string | null {
	if (delegation.status === "running") return null;
	const outcomes = Array.from(
		new Set(
			delegation.subagents.flatMap((subagent) =>
				typeof subagent.outcome === "string" && subagent.outcome.trim()
					? [humanizeDelegationValue(subagent.outcome)]
					: [],
			),
		),
	);
	const label = delegation.status === "failed" ? "Failure" : outcomes.length === 1 ? "Outcome" : "Outcomes";
	if (outcomes.length > 0) return `${label}: ${outcomes.join(", ")}`;
	switch (delegation.status) {
		case "done":
			return "Completed · Outcome details are not available in this handoff";
		case "done_with_failures":
			return "Completed with failures · Outcome details are not available in this handoff";
		case "cancelled":
			return "Cancelled";
		case "failed":
			return "Failed";
		default:
			return null;
	}
}

export function subagentOutcomeText(subagent: DelegationSubagent): string | null {
	if (typeof subagent.outcome === "string" && subagent.outcome.trim()) {
		return `${subagent.status === "failed" ? "Failure" : "Outcome"}: ${humanizeDelegationValue(subagent.outcome)}`;
	}
	return null;
}

/** A delegation is in flight (and therefore cancellable / its subagents pollable)
 * exactly while its status is `running`. Every other status is terminal. */
export function isDelegationRunning(delegation: Delegation): boolean {
	return delegation.status === "running";
}

export function subagentHasNonEmptyPromptFile(subagent: DelegationSubagent): boolean {
	return typeof subagent.task_prompt_file === "string" && !!subagent.task_prompt_file.trim();
}

/** The daemon writes normal per-subagent handoff files from the delegation
 * barrier, which completes delegations as `done` or `done_with_failures`.
 * Other terminal states are real, but only completed delegations expose the
 * normal final_message/transcript artifact links in the board. */
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

/** Map a delegation or subagent status to a CSS-safe icon modifier. Icons always
 * sit beside visible status copy; color is supplementary, never authoritative. */
export function statusRailClass(status: string): string {
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

/** Whether a delegation can be re-run from the board. Keep this predicate in lockstep
 * with the actual reconstruction path so the UI never offers a re-run that the
 * click handler will reject. */
export function canReRunDelegation(delegation: Delegation): boolean {
	return reRunTaskPlan(delegation, new Map(), { allowPromptFiles: true }) !== null;
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

function subagentTask(subagent: DelegationSubagent, resolvedPrompts: ReadonlyMap<string, string>): { role: string; prompt: string } | null {
	const prompt = resolvedPrompts.get(subagent.id);
	const role = subagent.role;
	if (typeof prompt !== "string" || !prompt.trim()) return null;
	if (typeof role !== "string" || !role.trim()) return null;
	return { role: role.trim(), prompt };
}

type ReRunTask = { role: string; prompt: string };
type ReRunTaskPlan =
	| { kind: "full"; task: ReRunTask }
	| { kind: "readonly_fanout"; tasks: ReRunTask[] };

function reRunTaskPlan(
	delegation: Delegation,
	resolvedPrompts: ReadonlyMap<string, string>,
	options: { allowPromptFiles: boolean } = { allowPromptFiles: false },
): ReRunTaskPlan | null {
	if (!isReRunnableDelegationStatus(delegation.status)) return null;
	if (delegation.subagents.length === 0) return null;
	const roles = delegation.subagents.map((subagent) => subagent.role);
	if (roles.some((role) => typeof role !== "string" || !role.trim())) return null;
	if (options.allowPromptFiles) {
		if (delegation.subagents.some((subagent) => !subagentHasNonEmptyPromptFile(subagent))) return null;
		const resolved = delegation.subagents.map((subagent) => ({
			role: subagent.role!.trim(),
			prompt: resolvedPrompts.get(subagent.id) ?? "",
		}));
		return planForResolvedTasks(delegation, resolved);
	} else {
		const tasks = delegation.subagents.map((subagent) => subagentTask(subagent, resolvedPrompts));
		if (tasks.some((task) => task === null)) return null;
		const resolved = tasks as ReRunTask[];
		return planForResolvedTasks(delegation, resolved);
	}
}

function planForResolvedTasks(delegation: Delegation, resolved: ReRunTask[]): ReRunTaskPlan | null {
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

/** Reconstruct the `delegation.start_*` params to re-run a finished delegation after
 * prompt text has been explicitly loaded from `task_prompt.md`. The normal
 * delegation snapshot intentionally carries only file refs, not raw task prompt
 * text. Returns the kind-tagged params, or null when any prompt/role is missing. */
export function reRunParamsForDelegation(
	delegation: Delegation,
	parentSessionId: string,
	resolvedPrompts: ReadonlyMap<string, string> = new Map(),
):
	| { kind: "full"; params: StartFullDelegationParams }
	| { kind: "readonly_fanout"; params: StartReadonlyDelegationFanoutParams }
	| null {
	const plan = reRunTaskPlan(delegation, resolvedPrompts);
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
