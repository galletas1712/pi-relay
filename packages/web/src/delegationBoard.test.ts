import { describe, expect, it } from "vitest";
import {
	canReRunDelegation,
	isDelegationRunning,
	reRunParamsForDelegation,
	delegationHasHandoff,
	delegationStatusLabel,
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
			},
			{
				id: "child-b",
				status: "idle",
				role: "explore",
				subagent_type: "read_only",
				task_prompt_file: "child-b/task_prompt.md",
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
