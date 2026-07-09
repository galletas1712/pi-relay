import { describe, expect, it } from "vitest";
import {
	agentStatusIconKey,
	delegationNeedsAttention,
	delegationProgressSummary,
	isDelegationRunning,
	orderDelegations,
	remainingDelegationWorkCount,
	statusIconClass,
} from "./delegationBoard.ts";
import type { Delegation } from "./types.ts";

function delegation(overrides: Partial<Delegation> = {}): Delegation {
	return {
		delegation_id: "delegation-1",
		kind: "full",
		status: "done",
		workflow: null,
		label: "ship it",
		subagents: [{
			id: "child-1",
			status: "done",
			activity: "idle",
			role: "implementer",
			subagent_type: "full",
			task_prompt_file: "child-1/task_prompt.md",
		}],
		...overrides,
	};
}

describe("delegation triage model", () => {
	it("groups attention, active, and recent work while preserving order within each group", () => {
		const delegations = [
			delegation({ delegation_id: "recent-new", status: "done" }),
			delegation({ delegation_id: "attention-new", status: "failed" }),
			delegation({ delegation_id: "active-new", status: "running" }),
			delegation({
				delegation_id: "partial-failure",
				status: "running",
				progress: { expected: 3, spawned: 2, terminal: 1, running: 1, failed: 1 },
			}),
			delegation({ delegation_id: "attention-old", status: "done_with_failures" }),
			delegation({ delegation_id: "active-old", status: "running" }),
			delegation({ delegation_id: "recent-old", status: "cancelled" }),
		];

		expect(
			orderDelegations(delegations).map((item) => item.delegation_id),
		).toEqual([
			"attention-new",
			"partial-failure",
			"attention-old",
			"active-new",
			"active-old",
			"recent-new",
			"recent-old",
		]);
	});

	it("uses server and child failure state for ordering without treating cancellation as attention", () => {
		expect(delegationNeedsAttention(delegation({ status: "failed" }))).toBe(true);
		expect(delegationNeedsAttention(delegation({
			status: "running",
			progress: { expected: 2, spawned: 2, terminal: 1, running: 1, failed: 1 },
		}))).toBe(true);
		expect(delegationNeedsAttention(delegation({ status: "cancelled" }))).toBe(false);
	});
});

describe("delegation control helpers", () => {
	it("treats only running work as cancellable", () => {
		expect(isDelegationRunning(delegation({ status: "running" }))).toBe(true);
		for (const status of ["done", "done_with_failures", "cancelled", "failed"] as const) {
			expect(isDelegationRunning(delegation({ status }))).toBe(false);
		}
	});

	it("retains progress underneath the UI for cancellation impact", () => {
		const item = delegation({
			status: "running",
			progress: { expected: 4, spawned: 3, terminal: 1, running: 2, failed: 1 },
		});
		expect(delegationProgressSummary(item)).toEqual({
			expected: 4,
			spawned: 3,
			terminal: 1,
			running: 2,
			failed: 1,
			source: "server",
		});
		expect(remainingDelegationWorkCount(item)).toEqual({ count: 3, unit: "agents/slots" });
	});

	it("maps statuses to stable icon modifiers", () => {
		expect(statusIconClass("running")).toBe("running");
		expect(statusIconClass("done")).toBe("done");
		expect(statusIconClass("done_with_failures")).toBe("warn");
		expect(statusIconClass("failed")).toBe("failed");
		expect(statusIconClass("cancelled")).toBe("cancelled");
		expect(statusIconClass("queued")).toBe("pending");
	});

	it("maps every delegation and subagent status plus fallback to a distinct icon key", () => {
		const cases = {
			running: "running",
			done: "done",
			done_with_failures: "done-with-failures",
			failed: "failed",
			cancelled: "cancelled",
			queued: "queued",
			idle: "idle",
			future_status: "unknown",
		} as const;

		for (const [status, icon] of Object.entries(cases)) {
			expect(agentStatusIconKey(status), status).toBe(icon);
		}
		expect(new Set(Object.values(cases)).size).toBe(Object.keys(cases).length);
	});
});
