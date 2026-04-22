import { describe, expect, it } from "vitest";
import type { OrchestratorTreeSnapshot } from "@pi-relay/agent-protocol";
import {
	aggregateUsageTotals,
	buildAgentSummaries,
	buildSiblingBatchEntries,
	createOrchestratorCoreState,
	evaluateSpawnAllowance,
	reduceOrchestratorState,
	toTreeSnapshot,
} from "../src/index.js";

interface SpawnConfigLike {
	role: string;
	prompt: string;
}

function createSnapshot(): OrchestratorTreeSnapshot<SpawnConfigLike> {
	return {
		sessionId: "root-session",
		agents: {
			root: {
				id: "root",
				parentId: null,
				childIds: ["child-a", "child-b"],
				role: "root",
				status: "idle",
				spawnConfig: { role: "root", prompt: "" },
				sessionFile: "root.jsonl",
				worklogFile: "root.worklog.md",
				createdAt: 1,
				lastStatusChange: 1,
				lastWorklogTurn: 0,
				lastWorklogMessageCount: 0,
				turnCount: 0,
			},
			"child-a": {
				id: "child-a",
				parentId: "root",
				childIds: [],
				role: "planner",
				status: "running",
				spawnConfig: { role: "planner", prompt: "inspect A" },
				sessionFile: "child-a.jsonl",
				worklogFile: "child-a.worklog.md",
				createdAt: 2,
				lastStatusChange: 2,
				lastWorklogTurn: 0,
				lastWorklogMessageCount: 0,
				turnCount: 0,
			},
			"child-b": {
				id: "child-b",
				parentId: "root",
				childIds: [],
				role: "researcher",
				status: "idle",
				spawnConfig: { role: "researcher", prompt: "inspect B" },
				sessionFile: "child-b.jsonl",
				worklogFile: "child-b.worklog.md",
				createdAt: 3,
				lastStatusChange: 3,
				lastWorklogTurn: 0,
				lastWorklogMessageCount: 0,
				turnCount: 0,
			},
		},
	};
}

describe("orchestrator-core selectors", () => {
	it("evaluates spawn allowance, sibling batches, and summaries from pure state", () => {
		const state = createOrchestratorCoreState(createSnapshot(), {
			root: [{ id: "pending-child", role: "auditor", prompt: "inspect pending" }],
		});

		expect(evaluateSpawnAllowance(state, "root", {
			maxChildren: 4,
			maxDepth: 4,
			maxActiveAgents: 4,
		}).allowed).toBe(true);
		expect(evaluateSpawnAllowance(state, "root", {
			maxChildren: 1,
			maxDepth: 4,
			maxActiveAgents: 4,
		}).reason).toBe("max-children");

		const summaries = buildAgentSummaries(state, "root");
		expect(summaries.map((summary) => summary.id)).toEqual(["root", "child-a", "child-b"]);
		expect(summaries[1]).toMatchObject({ depth: 1, role: "planner", status: "running" });

		const siblingEntries = buildSiblingBatchEntries(state, "root", "child-a");
		expect(siblingEntries).toEqual([
			{ id: "child-b", role: "researcher", prompt: "inspect B", status: "idle" },
			{ id: "pending-child", role: "auditor", prompt: "inspect pending", status: "spawning" },
		]);
	});

	it("aggregates usage totals and emits persist-tree effects through the reducer", () => {
		const snapshot = createSnapshot();
		const state = createOrchestratorCoreState(snapshot);
		const reduced = reduceOrchestratorState(state, {
			type: "register_pending_spawn",
			parentId: "root",
			draft: { id: "pending-child", role: "auditor", prompt: "inspect pending" },
		});

		expect(reduced.events[0]?.type).toBe("pending_spawn_registered");
		expect(reduced.effects.some((effect) => effect.type === "persist_tree")).toBe(true);
		expect(toTreeSnapshot(reduced.state).sessionId).toBe("root-session");

		const aggregated = aggregateUsageTotals(reduced.state, "root", {
			root: {
				sessionFile: "root.jsonl",
				sessionId: "root-session",
				userMessages: 1,
				assistantMessages: 2,
				toolCalls: 0,
				toolResults: 0,
				totalMessages: 3,
				tokens: { input: 10, output: 20, cacheRead: 0, cacheWrite: 0, total: 30 },
				cost: 0.1,
			},
			"child-a": {
				sessionFile: "child-a.jsonl",
				sessionId: "child-a-session",
				userMessages: 2,
				assistantMessages: 1,
				toolCalls: 1,
				toolResults: 1,
				totalMessages: 5,
				tokens: { input: 5, output: 6, cacheRead: 1, cacheWrite: 0, total: 12 },
				cost: 0.05,
			},
			"child-b": {
				sessionFile: "child-b.jsonl",
				sessionId: "child-b-session",
				userMessages: 1,
				assistantMessages: 1,
				toolCalls: 0,
				toolResults: 0,
				totalMessages: 2,
				tokens: { input: 4, output: 3, cacheRead: 0, cacheWrite: 0, total: 7 },
				cost: 0.02,
			},
		});

		expect(aggregated).toMatchObject({
			agentId: "root",
			descendantCount: 2,
			totals: {
				userMessages: 4,
				assistantMessages: 4,
				toolCalls: 1,
				toolResults: 1,
				totalMessages: 10,
				input: 19,
				output: 29,
				cacheRead: 1,
				cacheWrite: 0,
				totalTokens: 49,
				cost: 0.17,
			},
		});
	});
});
