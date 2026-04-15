import { describe, expect, it, vi } from "vitest";
import { buildAgentSelectorOptions, buildAgentWidgetLines, buildDirectChildRoster } from "../src/roster.js";
import { Orchestrator } from "../src/orchestrator.js";
import { FakeSession, waitForMicrotasks } from "./test-helpers.js";

function stripAnsi(text: string | undefined): string {
	return (text ?? "").replace(/\u001b\[[0-9;]*m/g, "");
}

describe("buildDirectChildRoster", () => {
	it("returns a no-children message when an agent has no direct children", () => {
		const orchestrator = new Orchestrator({
			rootSession: new FakeSession("root-session"),
			sessionFactory: vi.fn(),
		});

		expect(buildDirectChildRoster(orchestrator, "root")).toBe("You have no direct children.");
		expect(buildAgentWidgetLines(orchestrator, "root")).toBeUndefined();
	});

	it("renders current direct child ids, statuses, roles, and child counts", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
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

		const roster = buildDirectChildRoster(orchestrator, "root");
		expect(roster).toContain("## Active Children");
		expect(roster).toContain(`${childId} (waiting, 1 child): planner`);
		expect(roster).not.toContain("Scanning packages/orchestrator");
	});

	it("includes idle direct children in the on-demand child lookup", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const sibling = new FakeSession("sibling-session");
		const sessions = [child, sibling];
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: sessions.shift()! })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect",
		});
		const siblingId = await orchestrator.spawnAgent("root", {
			role: "explorer",
			prompt: "inspect more",
		});
		sibling.emit({ type: "agent_end" });
		await waitForMicrotasks();

		const roster = buildDirectChildRoster(orchestrator, "root");
		expect(roster).toContain("## Active Children");
		expect(roster).toContain(childId);
		expect(roster).toContain("## Idle Children");
		expect(roster).toContain(`${siblingId} (idle): explorer`);
	});

	it("builds selector and widget views for the attached agent tree", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.isStreaming = true;
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
		const childLabel = stripAnsi(options[1]?.label);
		expect(options.map((option) => option.agentId)).toEqual(["root", childId]);
		expect(childLabel).toContain(`● ${childId} · planner`);
		expect(childLabel).not.toContain("[running]");

		const widget = buildAgentWidgetLines(orchestrator, childId);
		const attachedLine = stripAnsi(widget[1]);
		expect(widget[0]).toBe("Relay Agents");
		expect(attachedLine).toContain(`● ${childId} (planner)`);
		expect(widget.at(-1)).toBe("Use /agents to switch");
	});

	it("shows waiting for quiet coordinators with active children", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const grandchild = new FakeSession("grandchild-session");
		grandchild.isStreaming = true;
		const sessions = [child, grandchild];
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: sessions.shift()! })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect",
		});
		await orchestrator.spawnAgent(childId, {
			role: "explorer",
			prompt: "inspect nested",
		});

		const options = buildAgentSelectorOptions(orchestrator, childId);
		expect(stripAnsi(options[1]?.label)).toContain(`● ${childId} (waiting) · planner`);

		const widget = buildAgentWidgetLines(orchestrator, "root");
		expect(widget?.some((line) => stripAnsi(line).includes(`● ${childId} (waiting) · planner · waiting · 1 child`))).toBe(
			true,
		);
	});

	it("hides idle agents by default in the widget", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect",
		});
		child.emit({ type: "agent_end" });
		await waitForMicrotasks();

		expect(buildAgentWidgetLines(orchestrator, "root")).toBeUndefined();
	});

	it("keeps the attached idle child visible while hiding other idle agents", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const sibling = new FakeSession("sibling-session");
		const sessions = [child, sibling];
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: sessions.shift()! })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect",
		});
		await orchestrator.spawnAgent("root", {
			role: "explorer",
			prompt: "inspect more",
		});
		child.emit({ type: "agent_end" });
		sibling.emit({ type: "agent_end" });
		await waitForMicrotasks();

		const widget = buildAgentWidgetLines(orchestrator, childId);
		expect(widget?.[0]).toBe("Relay Agents");
		expect(widget?.some((line) => stripAnsi(line).includes(`  ${childId} · planner`))).toBe(true);
		expect(widget?.some((line) => line.includes("idle agent"))).toBe(true);
		expect(widget?.at(-1)).toBe("Use /agents to switch");
	});
});
