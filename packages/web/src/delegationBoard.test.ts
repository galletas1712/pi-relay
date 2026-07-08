import { describe, expect, it } from "vitest";
import {
	canReRunDelegation,
	delegationKindLabel,
	delegationNeedsAttention,
	delegationOutcomeText,
	delegationProgressSummary,
	formatDelegationProgress,
	groupDelegations,
	isDelegationRunning,
	reRunParamsForDelegation,
	delegationHasHandoff,
	delegationStatusLabel,
	remainingDelegationWorkCount,
} from "./delegationBoard.ts";
import type { Delegation } from "./types.ts";

function fullDelegation(overrides: Partial<Delegation> = {}): Delegation {
	return {
		delegation_id: "delegation-1",
		kind: "full",
		status: "done",
		workflow: "workflow-implement-review",
		label: "ship it",
		subagents: [
			{
				id: "child-1",
				status: "idle",
				role: "implement",
				subagent_type: "full",
				task_prompt_file: "child-1/task_prompt.md",
				final_message_file: null,
				transcript_file: null,
				outcome: null,
			},
		],
		...overrides,
	};
}

function fanoutDelegation(overrides: Partial<Delegation> = {}): Delegation {
	return {
		delegation_id: "delegation-2",
		kind: "readonly_fanout",
		status: "done_with_failures",
		workflow: null,
		label: null,
		subagents: [
			{
				id: "child-a",
				status: "idle",
				role: "explore",
				subagent_type: "read_only",
				task_prompt_file: "child-a/task_prompt.md",
				final_message_file: null,
				transcript_file: null,
				outcome: null,
			},
			{
				id: "child-b",
				status: "idle",
				role: "explore",
				subagent_type: "read_only",
				task_prompt_file: "child-b/task_prompt.md",
				final_message_file: null,
				transcript_file: null,
				outcome: null,
			},
		],
		...overrides,
	};
}

describe("isDelegationRunning", () => {
	it("is true only for the running status", () => {
		expect(isDelegationRunning(fullDelegation({ status: "running" }))).toBe(true);
		for (const status of ["done", "done_with_failures", "cancelled", "failed"] as const) {
			expect(isDelegationRunning(fullDelegation({ status }))).toBe(false);
		}
	});

describe("delegation triage model", () => {
	it("groups attention, active, and recent work while preserving server order inside each section", () => {
		const delegations = [
			fullDelegation({ delegation_id: "recent-new", status: "done" }),
			fullDelegation({ delegation_id: "attention-new", status: "failed" }),
			fullDelegation({ delegation_id: "active-new", status: "running" }),
			fullDelegation({
				delegation_id: "partial-failure",
				status: "running",
				progress: { expected: 3, spawned: 2, terminal: 1, running: 1, failed: 1 },
			}),
			fullDelegation({ delegation_id: "attention-old", status: "done_with_failures" }),
			fullDelegation({ delegation_id: "active-old", status: "running" }),
			fullDelegation({ delegation_id: "recent-old", status: "cancelled" }),
		];

		expect(
			groupDelegations(delegations).map((section) => ({
				label: section.label,
				ids: section.delegations.map((delegation) => delegation.delegation_id),
			})),
		).toEqual([
			{ label: "Needs attention", ids: ["attention-new", "partial-failure", "attention-old"] },
			{ label: "Active", ids: ["active-new", "active-old"] },
			{ label: "Recent", ids: ["recent-new", "recent-old"] },
		]);
	});

	it("uses available technical failure status/progress, including partial running failures", () => {
		expect(delegationNeedsAttention(fullDelegation({ status: "failed" }))).toBe(true);
		expect(
			delegationNeedsAttention(
				fullDelegation({
					status: "running",
					progress: { expected: 3, spawned: 2, terminal: 1, running: 1, failed: 1 },
				}),
			),
		).toBe(true);
		expect(
			delegationNeedsAttention(
				fullDelegation({
					subagents: [{
						id: "review",
						status: "done",
						role: "reviewer",
						task_prompt_file: "review/task_prompt.md",
						outcome: "changes_requested",
					}],
				}),
			),
		).toBe(false);
		expect(
			delegationNeedsAttention(
				fullDelegation({
					subagents: [{
						id: "review",
						status: "done",
						role: "reviewer",
						task_prompt_file: "review/task_prompt.md",
						outcome: "approved",
					}],
				}),
			),
		).toBe(false);
		expect(
			delegationNeedsAttention(
				fullDelegation({
					status: "cancelled",
					progress: { expected: 1, spawned: 1, terminal: 1, running: 0, failed: 1 },
				}),
			),
		).toBe(false);
	});

	it("uses task-centered labels and never infers a product outcome from the list contract's null", () => {
		expect(delegationKindLabel("full")).toBe("Writing task");
		expect(delegationKindLabel("readonly_fanout")).toBe("Parallel research");
		const listContractDone = fullDelegation();
		expect(delegationOutcomeText(listContractDone)).toBe(
			"Completed · Outcome details are not available in this handoff",
		);
		expect(delegationOutcomeText(listContractDone)).not.toContain("success");
		expect(delegationOutcomeText(fullDelegation({ status: "failed", subagents: [] }))).toBe(
			"Failed",
		);
	});

	it("renders a known outcome only when a caller actually supplies one", () => {
		expect(
			delegationOutcomeText(
				fullDelegation({
					subagents: [{
						id: "review",
						status: "done",
						role: "reviewer",
						task_prompt_file: "review/task_prompt.md",
						outcome: "changes_requested",
					}],
				}),
			),
		).toBe("Outcome: Changes requested");
	});
});

describe("delegation progress", () => {
	it("formats every server progress field", () => {
		const delegation = fanoutDelegation({
			status: "running",
			progress: { expected: 4, spawned: 3, terminal: 1, running: 2, failed: 1 },
		});
		expect(delegationProgressSummary(delegation)).toEqual({
			expected: 4,
			spawned: 3,
			terminal: 1,
			running: 2,
			failed: 1,
			source: "server",
		});
		expect(formatDelegationProgress(delegation)).toBe(
			"4 expected · 3 spawned · 1 terminal · 2 running · 1 failed",
		);
		expect(remainingDelegationWorkCount(delegation)).toEqual({
			count: 3,
			unit: "agents/slots",
		});
	});

	it("derives only observed child counts when server progress is absent", () => {
		const delegation = fanoutDelegation({
			status: "running",
			progress: null,
			subagents: [
				{ id: "done", status: "done", activity: "running", role: "explore" },
				{ id: "running", status: "running", activity: "running", role: "explore" },
				{ id: "failed", status: "failed", activity: "idle", role: "explore" },
				{ id: "queued", status: "queued", activity: "queued", role: "explore" },
			],
		});
		expect(delegationProgressSummary(delegation)).toEqual({
			expected: null,
			spawned: 4,
			terminal: 2,
			running: 1,
			failed: 1,
			source: "children",
		});
		expect(formatDelegationProgress(delegation)).toBe(
			"4 agents shown · 2 terminal · 1 running · 1 failed",
		);
		expect(formatDelegationProgress(delegation)).not.toContain("expected");
	});
});
});

describe("delegationStatusLabel", () => {
	it("humanizes the underscore statuses", () => {
		expect(delegationStatusLabel("done_with_failures")).toBe("done with failures");
		expect(delegationStatusLabel("running")).toBe("running");
	});
});

describe("delegationHasHandoff", () => {
	it("is true only for barrier-completed delegations", () => {
		expect(delegationHasHandoff(fullDelegation({ status: "done" }))).toBe(true);
		expect(delegationHasHandoff(fullDelegation({ status: "done_with_failures" }))).toBe(true);
		expect(delegationHasHandoff(fullDelegation({ status: "running" }))).toBe(false);
		expect(delegationHasHandoff(fullDelegation({ status: "cancelled" }))).toBe(false);
		expect(delegationHasHandoff(fullDelegation({ status: "failed" }))).toBe(false);
	});
});

describe("canReRunDelegation", () => {
	it("allows re-run of a finished delegation when every prompt file ref and role is present", () => {
		expect(canReRunDelegation(fullDelegation({ status: "done" }))).toBe(true);
	});

	it("forbids re-run while the delegation is running", () => {
		expect(canReRunDelegation(fullDelegation({ status: "running" }))).toBe(false);
	});

	it("forbids re-run when any prompt file ref is missing", () => {
		const delegation = fanoutDelegation({
			subagents: [
				{
					id: "child-a",
					status: "idle",
					role: "explore",
					subagent_type: "read_only",
					task_prompt_file: "child-a/task_prompt.md",
				},
				{ id: "child-b", status: "idle", role: "explore", subagent_type: "read_only", task_prompt_file: null },
			],
		});
		expect(canReRunDelegation(delegation)).toBe(false);
	});

	it("allows re-run from task prompt handoff file references before prompt text is loaded", () => {
		const delegation = fanoutDelegation({
			subagents: [
				{
					id: "child-a",
					status: "idle",
					role: "explore",
					subagent_type: "read_only",
					task_prompt_file: "child-a/task_prompt.md",
				},
				{
					id: "child-b",
					status: "idle",
					role: "explore",
					subagent_type: "read_only",
					task_prompt_file: "child-b/task_prompt.md",
				},
			],
		});
		expect(canReRunDelegation(delegation)).toBe(true);
		expect(reRunParamsForDelegation(delegation, "parent")).toBeNull();
	});

	it("forbids re-run when task prompt file references are missing or empty", () => {
		const delegation = fanoutDelegation({
			subagents: [
				{
					id: "child-a",
					status: "idle",
					role: "explore",
					subagent_type: "read_only",
					task_prompt_file: "",
				},
				{
					id: "child-b",
					status: "idle",
					role: "explore",
					subagent_type: "read_only",
					task_prompt_file: null,
				},
			],
		});
		expect(canReRunDelegation(delegation)).toBe(false);
	});

	it("forbids re-run when any role is missing", () => {
		const delegation = fullDelegation({
			subagents: [
				{
					id: "child-1",
					status: "idle",
					role: null,
					subagent_type: "full",
					task_prompt_file: "child-1/task_prompt.md",
				},
			],
		});
		expect(canReRunDelegation(delegation)).toBe(false);
	});

	it("forbids re-run for unknown delegation kinds", () => {
		const delegation = fanoutDelegation({ kind: "bogus" as Delegation["kind"] });
		expect(canReRunDelegation(delegation)).toBe(false);
	});

	it("forbids re-run for unknown delegation statuses", () => {
		const delegation = fullDelegation({ status: "stale" as Delegation["status"] });
		expect(canReRunDelegation(delegation)).toBe(false);
	});
});

describe("reRunParamsForDelegation", () => {
	it("reconstructs a full delegation start with the subagent task", () => {
		const result = reRunParamsForDelegation(
			fullDelegation({ status: "done" }),
			"parent",
			new Map([["child-1", "implement the feature"]]),
		);
		expect(result).toEqual({
			kind: "full",
			params: {
				parentSessionId: "parent",
				role: "implement",
				prompt: "implement the feature",
				workflow: "workflow-implement-review",
				label: "ship it",
			},
		});
	});

	it("reconstructs a fan-out with one task per subagent", () => {
		const result = reRunParamsForDelegation(
			fanoutDelegation(),
			"parent",
			new Map([
				["child-a", "explore module a"],
				["child-b", "explore module b"],
			]),
		);
		expect(result).toEqual({
			kind: "readonly_fanout",
			params: {
				parentSessionId: "parent",
				tasks: [
					{ role: "explore", prompt: "explore module a" },
					{ role: "explore", prompt: "explore module b" },
				],
				workflow: undefined,
				label: undefined,
			},
		});
	});

	it("returns null when a prompt cannot be recovered", () => {
		const delegation = fullDelegation();
		expect(reRunParamsForDelegation(delegation, "parent")).toBeNull();
	});

	it("returns null when a role cannot be recovered", () => {
		const delegation = fullDelegation({
			subagents: [
				{
					id: "child-1",
					status: "idle",
					role: undefined,
					subagent_type: "full",
					task_prompt_file: "child-1/task_prompt.md",
				},
			],
		});
		expect(reRunParamsForDelegation(delegation, "parent", new Map([["child-1", "implement the feature"]]))).toBeNull();
	});

	it("returns null while running or when a delegation has no subagents", () => {
		expect(reRunParamsForDelegation(fullDelegation({ status: "running" }), "parent")).toBeNull();
		expect(reRunParamsForDelegation(fullDelegation({ subagents: [] }), "parent")).toBeNull();
	});

	it("returns null instead of treating an unknown kind as fan-out", () => {
		const delegation = fanoutDelegation({ kind: "bogus" as Delegation["kind"] });
		expect(reRunParamsForDelegation(delegation, "parent")).toBeNull();
	});

	it("returns null for an unknown status", () => {
		const delegation = fullDelegation({ status: "stale" as Delegation["status"] });
		expect(reRunParamsForDelegation(delegation, "parent")).toBeNull();
	});
});
