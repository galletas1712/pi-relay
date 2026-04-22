import { describe, expect, it } from "vitest";
import type {
	OrchestratorBoundaryCommand,
	OrchestratorBoundaryEffect,
	OrchestratorTreeSnapshot,
} from "../src/index.js";

interface SpawnConfigLike {
	role: string;
	prompt: string;
}

describe("agent protocol orchestrator boundary types", () => {
	it("supports snapshot, command, and effect-shaped protocol payloads", () => {
		const snapshot: OrchestratorTreeSnapshot<SpawnConfigLike> = {
			sessionId: "root-session",
			agents: {
				root: {
					id: "root",
					parentId: null,
					childIds: ["child-a"],
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
			},
		};
		const command: OrchestratorBoundaryCommand<SpawnConfigLike> = {
			type: "register_pending_spawn",
			parentId: "root",
			draft: { id: "child-a", role: "planner", prompt: "inspect" },
		};
		const effect: OrchestratorBoundaryEffect<SpawnConfigLike> = {
			type: "persist_tree",
			snapshot,
		};

		expect(snapshot.agents.root?.childIds).toEqual(["child-a"]);
		expect(command.type).toBe("register_pending_spawn");
		expect(effect.type).toBe("persist_tree");
	});
});
