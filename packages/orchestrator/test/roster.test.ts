import { describe, expect, it, vi } from "vitest";
import { buildAgentSelectorOptions, buildAgentWidgetLines, buildSubagentRoster } from "../src/roster.js";
import { Orchestrator } from "../src/orchestrator.js";
import { FakeSession } from "./test-helpers.js";

describe("buildSubagentRoster", () => {
	it("returns an empty string when an agent has no children", () => {
		const orchestrator = new Orchestrator({
			rootSession: new FakeSession("root-session"),
			sessionFactory: vi.fn(),
		});

		expect(buildSubagentRoster(orchestrator, "root")).toBe("");
	});

	it("renders live child status, activity, and child counts", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.lastAssistantText = "Scanning packages/orchestrator and comparing session restore paths.";
		const grandchild = new FakeSession("grandchild-session");
		const sessions = [child, grandchild];
		const factory = vi.fn(async () => ({ session: sessions.shift()! }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect",
		});
		await orchestrator.spawnAgent(childId, {
			role: "explorer",
			prompt: "inspect nested",
		});

		const roster = buildSubagentRoster(orchestrator, "root");
		expect(roster).toContain("## Active Subagents");
		expect(roster).toContain(`${childId} (running, 1 children): planner`);
		expect(roster).toContain("Scanning packages/orchestrator");
	});

	it("builds selector and widget views for the attached agent tree", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.lastAssistantText = "Still indexing code paths.";
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect",
		});

		const options = buildAgentSelectorOptions(orchestrator, childId);
		expect(options.map((option) => option.agentId)).toEqual(["root", childId]);
		expect(options[1]?.label).toContain(`${childId} [running] planner`);

		const widget = buildAgentWidgetLines(orchestrator, childId);
		expect(widget[0]).toBe("Relay Agents");
		expect(widget[1]).toContain(`Attached: ${childId} (planner, running)`);
		expect(widget.at(-1)).toBe("Use /agents to switch");
	});
});
