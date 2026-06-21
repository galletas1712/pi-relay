import { describe, expect, it } from "vitest";
import {
	canReRunStage,
	isStageRunning,
	reRunParamsForStage,
	stageStatusLabel,
	steerableSubagentId,
} from "./runBoard.ts";
import type { Stage } from "./types.ts";

function fullStage(overrides: Partial<Stage> = {}): Stage {
	return {
		stage_id: "stage-1",
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
				task: "implement the feature",
			},
		],
		...overrides,
	};
}

function fanoutStage(overrides: Partial<Stage> = {}): Stage {
	return {
		stage_id: "stage-2",
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
				task: "explore module a",
			},
			{
				id: "child-b",
				status: "idle",
				role: "explore",
				subagent_type: "read_only",
				task: "explore module b",
			},
		],
		...overrides,
	};
}

describe("isStageRunning", () => {
	it("is true only for the running status", () => {
		expect(isStageRunning(fullStage({ status: "running" }))).toBe(true);
		for (const status of ["done", "done_with_failures", "cancelled", "failed"] as const) {
			expect(isStageRunning(fullStage({ status }))).toBe(false);
		}
	});
});

describe("stageStatusLabel", () => {
	it("humanizes the underscore statuses", () => {
		expect(stageStatusLabel("done_with_failures")).toBe("done with failures");
		expect(stageStatusLabel("running")).toBe("running");
	});
});

describe("steerableSubagentId", () => {
	it("returns the full subagent only while the stage is running", () => {
		expect(steerableSubagentId(fullStage({ status: "running" }))).toBe("child-1");
		expect(steerableSubagentId(fullStage({ status: "done" }))).toBeNull();
	});

	it("never returns a read-only fan-out member", () => {
		expect(steerableSubagentId(fanoutStage({ status: "running" }))).toBeNull();
	});
});

describe("canReRunStage", () => {
	it("allows re-run of a finished stage when every prompt and role is present", () => {
		expect(canReRunStage(fullStage({ status: "done" }))).toBe(true);
	});

	it("forbids re-run while the stage is running", () => {
		expect(canReRunStage(fullStage({ status: "running" }))).toBe(false);
	});

	it("forbids re-run when any prompt is missing", () => {
		const stage = fanoutStage({
			subagents: [
				{ id: "child-a", status: "idle", role: "explore", subagent_type: "read_only", task: "look here" },
				{ id: "child-b", status: "idle", role: "explore", subagent_type: "read_only", task: null },
			],
		});
		expect(canReRunStage(stage)).toBe(false);
	});

	it("forbids re-run when any role is missing", () => {
		const stage = fullStage({
			subagents: [
				{
					id: "child-1",
					status: "idle",
					role: null,
					subagent_type: "full",
					task: "implement the feature",
				},
			],
		});
		expect(canReRunStage(stage)).toBe(false);
	});
});

describe("reRunParamsForStage", () => {
	it("reconstructs a full stage start with the subagent task", () => {
		const result = reRunParamsForStage(fullStage({ status: "done" }), "parent");
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
		const result = reRunParamsForStage(fanoutStage(), "parent");
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
		const stage = fullStage({
			subagents: [{ id: "child-1", status: "idle", role: "implement", subagent_type: "full", task: null }],
		});
		expect(reRunParamsForStage(stage, "parent")).toBeNull();
	});

	it("returns null when a role cannot be recovered", () => {
		const stage = fullStage({
			subagents: [
				{
					id: "child-1",
					status: "idle",
					role: undefined,
					subagent_type: "full",
					task: "implement the feature",
				},
			],
		});
		expect(reRunParamsForStage(stage, "parent")).toBeNull();
	});

	it("returns null while running or when a stage has no subagents", () => {
		expect(reRunParamsForStage(fullStage({ status: "running" }), "parent")).toBeNull();
		expect(reRunParamsForStage(fullStage({ subagents: [] }), "parent")).toBeNull();
	});
});
