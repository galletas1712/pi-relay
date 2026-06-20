import { describe, expect, it } from "vitest";
import {
	canReRunStage,
	isStageRunning,
	reRunParamsForStage,
	stageStatusLabel,
	steerableSubagentId,
	taskBySessionId,
} from "./runBoard.ts";
import type { SessionSummary, Stage } from "./types.ts";

function fullStage(overrides: Partial<Stage> = {}): Stage {
	return {
		stage_id: "stage-1",
		kind: "full",
		status: "done",
		workflow: "workflow-implement-review",
		label: "ship it",
		subagents: [{ id: "child-1", status: "idle", role: "implement", subagent_type: "full" }],
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
			{ id: "child-a", status: "idle", role: "explore", subagent_type: "read_only" },
			{ id: "child-b", status: "idle", role: "explore", subagent_type: "read_only" },
		],
		...overrides,
	};
}

function summary(sessionId: string, task: unknown): SessionSummary {
	return {
		session_id: sessionId,
		project_id: null,
		outer_cwd: "/cwd",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "claude", model: "claude-opus-4-8" },
		metadata: task === undefined ? {} : { task },
		created_at: "",
		updated_at: "",
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

describe("taskBySessionId", () => {
	it("maps the metadata.task prompt and records missing tasks as null", () => {
		const map = taskBySessionId([summary("child-1", "do the work"), summary("child-2", undefined)]);
		expect(map.get("child-1")).toBe("do the work");
		expect(map.get("child-2")).toBeNull();
	});
});

describe("canReRunStage", () => {
	it("allows re-run of a finished stage when every prompt is recoverable", () => {
		const tasks = new Map([["child-1", "implement the feature"]]);
		expect(canReRunStage(fullStage({ status: "done" }), tasks)).toBe(true);
	});

	it("forbids re-run while the stage is running", () => {
		const tasks = new Map([["child-1", "implement the feature"]]);
		expect(canReRunStage(fullStage({ status: "running" }), tasks)).toBe(false);
	});

	it("forbids re-run when any prompt is missing", () => {
		const tasks = new Map<string, string | null>([
			["child-a", "look here"],
			["child-b", null],
		]);
		expect(canReRunStage(fanoutStage(), tasks)).toBe(false);
	});
});

describe("reRunParamsForStage", () => {
	it("reconstructs a full stage start with the recovered prompt", () => {
		const tasks = new Map([["child-1", "implement the feature"]]);
		const result = reRunParamsForStage(fullStage({ status: "done" }), "parent", tasks);
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
		const tasks = new Map([
			["child-a", "explore module a"],
			["child-b", "explore module b"],
		]);
		const result = reRunParamsForStage(fanoutStage(), "parent", tasks);
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
		const tasks = new Map<string, string | null>([["child-1", null]]);
		expect(reRunParamsForStage(fullStage({ status: "done" }), "parent", tasks)).toBeNull();
	});
});
