import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Orchestrator } from "../src/orchestrator.js";
import { cleanupTempDir, createTempDir, FakeSession } from "./test-helpers.js";

describe("tree metadata", () => {
	const tempDirs: string[] = [];

	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	it("writes tree.json on spawn and updates status transitions", async () => {
		const sessionDir = createTempDir("pi-relay-tree-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", { sessionDir });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect tree persistence",
		});
		const treeFile = join(sessionDir, "root-session", "tree.json");
		await vi.waitFor(async () => {
			const tree = JSON.parse(await readFile(treeFile, "utf-8")) as {
				agents: Record<string, { status: string; childIds: string[] }>;
			};
			expect(tree.agents.root.childIds).toEqual([childId]);
			expect(tree.agents[childId]?.status).toBe("running");
		});

		child.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(async () => {
			const tree = JSON.parse(await readFile(treeFile, "utf-8")) as {
				agents: Record<string, { status: string; childIds: string[] }>;
			};
			expect(tree.agents[childId]?.status).toBe("idle");
		});

		await orchestrator.dispose();
		await vi.waitFor(async () => {
			const tree = JSON.parse(await readFile(treeFile, "utf-8")) as {
				agents: Record<string, { status: string; childIds: string[] }>;
			};
			expect(tree.agents.root.status).toBe("disposed");
			expect(tree.agents[childId]?.status).toBe("disposed");
		});
	});
});
