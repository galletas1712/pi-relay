import { describe, expect, it, vi } from "vitest";
import { Orchestrator } from "../src/orchestrator.js";
import type { AgentSessionFactoryOptions } from "../src/types.js";
import { FakeSession, waitForMicrotasks } from "./test-helpers.js";

describe("Orchestrator", () => {
	it("spawns child sessions through the injected factory", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const factory = vi.fn(async (_options: AgentSessionFactoryOptions) => ({ session: child }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect the repo",
		});

		expect(factory).toHaveBeenCalledTimes(1);
		expect(orchestrator.getRecord(childId).parentId).toBe("root");
		expect(child.prompts).toEqual(["inspect the repo"]);
	});

	it("does not count idle children against spawn limits", async () => {
		const root = new FakeSession("root-session");
		const firstChild = new FakeSession("first-child-session");
		const secondChild = new FakeSession("second-child-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn()
				.mockResolvedValueOnce({ session: firstChild })
				.mockResolvedValueOnce({ session: secondChild }),
			config: {
				maxChildren: 1,
				maxActiveAgents: 3,
			},
		});

		await orchestrator.spawnAgent("root", {
			role: "explore-a",
			prompt: "inspect first area",
		});
		firstChild.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(orchestrator.getRecord("root").childIds).toHaveLength(1);
			expect(orchestrator.getChildrenOf("root")[0]?.status).toBe("idle");
		});

		await expect(
			orchestrator.spawnAgent("root", {
				role: "explore-b",
				prompt: "inspect second area",
			}),
		).resolves.toMatch(/explore-b-/);
	});

	it("counts pending child spawns against the direct-child limit", async () => {
		const root = new FakeSession("root-session");
		let releaseFirstSpawn = () => {};
		let nextChildIndex = 0;
		const firstSpawnGate = new Promise<void>((resolve) => {
			releaseFirstSpawn = resolve;
		});
		const factory = vi
			.fn()
			.mockImplementationOnce(async () => {
				await firstSpawnGate;
				return { session: new FakeSession("first-child-session") };
			})
			.mockImplementation(async () => ({ session: new FakeSession(`child-${++nextChildIndex}`) }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
			config: {
				maxChildren: 1,
				maxActiveAgents: 4,
			},
		});

		const firstSpawn = orchestrator.spawnAgent("root", {
			role: "explore-a",
			prompt: "inspect first area",
		});
		await waitForMicrotasks();

		await expect(
			orchestrator.spawnAgent("root", {
				role: "explore-b",
				prompt: "inspect second area",
			}),
		).rejects.toThrow("maximum number of children");

		releaseFirstSpawn();
		await firstSpawn;
	});

	it("counts pending child spawns against the active-agent limit", async () => {
		const root = new FakeSession("root-session");
		let releaseFirstSpawn = () => {};
		let nextChildIndex = 0;
		const firstSpawnGate = new Promise<void>((resolve) => {
			releaseFirstSpawn = resolve;
		});
		const factory = vi
			.fn()
			.mockImplementationOnce(async () => {
				await firstSpawnGate;
				return { session: new FakeSession("first-child-session") };
			})
			.mockImplementation(async () => ({ session: new FakeSession(`child-${++nextChildIndex}`) }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
			config: {
				maxChildren: 8,
				maxActiveAgents: 2,
			},
		});
		root.emit({ type: "agent_start" });

		const firstSpawn = orchestrator.spawnAgent("root", {
			role: "explore-a",
			prompt: "inspect first area",
		});
		await waitForMicrotasks();

		await expect(
			orchestrator.spawnAgent("root", {
				role: "explore-b",
				prompt: "inspect second area",
			}),
		).rejects.toThrow("active agent limit");

		releaseFirstSpawn();
		await firstSpawn;
	});

	it("routes directives only to direct children", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const factory = vi.fn(async () => ({ session: child }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		await orchestrator.routeMessage("root", childId, "check src");
		expect(child.sentMessages).toHaveLength(1);
		expect(child.sentMessages[0]?.options).toEqual({ triggerTurn: true });
		await expect(orchestrator.routeMessage(childId, "root", "bad")).rejects.toThrow("not a direct child");
	});

	it("does not block message delivery on the child's triggered turn", async () => {
		const root = new FakeSession("root-session");
		let resolveChildTurn = () => {};
		const childTurn = new Promise<void>((resolve) => {
			resolveChildTurn = resolve;
		});
		const child = new FakeSession("child-session");
		child.sendCustomMessage = vi.fn(async (message, options) => {
			child.sentMessages.push({ message, options });
			if ((options as { triggerTurn?: boolean } | undefined)?.triggerTurn) {
				await childTurn;
			}
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		const routePromise = orchestrator.routeMessage("root", childId, "check src");
		await waitForMicrotasks();
		await expect(routePromise).resolves.toBeUndefined();
		expect(child.sentMessages).toHaveLength(1);
		expect(child.sentMessages[0]?.options).toEqual({ triggerTurn: true });
		expect(child.sentMessages[0]?.message.customType).toBe("agent_directive");

		resolveChildTurn();
	});

	it("notifies the parent when a child becomes idle", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.lastAssistantText = "finished scanning files";
		const factory = vi.fn(async () => ({ session: child }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(root.sentMessages).toHaveLength(1);
		});
		expect(root.sentMessages[0]?.options).toEqual({ triggerTurn: true });
		expect(String(root.sentMessages[0]?.message.content)).toContain("The child is idle and can be reactivated with `message`.");
		expect(String(root.sentMessages[0]?.message.content)).not.toContain("Last output:");
	});

	it("waits for the session run to become idle before finalizing agent_end", async () => {
		const root = new FakeSession("root-session");
		let resolveIdle = () => {};
		const waitForIdle = new Promise<void>((resolve) => {
			resolveIdle = resolve;
		});
		const child = new FakeSession("child-session", {
			waitForIdle: async () => waitForIdle,
		});
		child.lastAssistantText = "finished scanning files";
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "agent_end", messages: [] });
		await waitForMicrotasks();

		expect(orchestrator.getRecord(childId).status).toBe("running");
		expect(root.sentMessages).toHaveLength(0);

		resolveIdle();
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).status).toBe("idle");
		});
		expect(root.sentMessages[0]?.message.customType).toBe("agent_idle");
	});

	it("recovers agent status when idle reactivation fails", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.sendCustomMessage = vi.fn(async () => {
			throw new Error("reactivation failed");
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});
		child.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).status).toBe("idle");
		});

		await orchestrator.routeMessage("root", childId, "continue");
		expect(orchestrator.getRecord(childId).status).toBe("idle");
		expect(root.sentMessages).toHaveLength(2);
		expect(root.sentMessages[1]?.message.customType).toBe("agent_idle");
	});

	it("delivers child updates as steering while the parent is already running", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.lastAssistantText = "done inspecting";
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		root.isStreaming = true;

		await orchestrator.handleReport(childId, "partial result");
		expect(root.sentMessages[0]?.options).toEqual({ deliverAs: "steer" });
		expect(root.sentMessages[0]?.message.customType).toBe("agent_report");

		child.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(root.sentMessages).toHaveLength(2);
		});
		expect(root.sentMessages[1]?.options).toEqual({ deliverAs: "steer" });
		expect(root.sentMessages[1]?.message.customType).toBe("agent_idle");
	});

	it("does not block report delivery on the parent's triggered turn", async () => {
		const root = new FakeSession("root-session");
		let resolveParentTurn = () => {};
		const parentTurn = new Promise<void>((resolve) => {
			resolveParentTurn = resolve;
		});
		root.sendCustomMessage = vi.fn(async (message, options) => {
			root.sentMessages.push({ message, options });
			if ((options as { triggerTurn?: boolean } | undefined)?.triggerTurn) {
				await parentTurn;
			}
		});

		const child = new FakeSession("child-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		const reportPromise = orchestrator.handleReport(childId, "important finding");
		await waitForMicrotasks();
		await expect(reportPromise).resolves.toBeUndefined();
		expect(root.sentMessages).toHaveLength(1);
		expect(root.sentMessages[0]?.options).toEqual({ triggerTurn: true });
		expect(root.sentMessages[0]?.message.customType).toBe("agent_report");

		resolveParentTurn();
	});

	it("buffers child idle updates until the last running sibling finishes", async () => {
		const root = new FakeSession("root-session");
		const firstChild = new FakeSession("first-child-session");
		const secondChild = new FakeSession("second-child-session");
		firstChild.lastAssistantText = "first child done";
		secondChild.lastAssistantText = "second child done";
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn()
				.mockResolvedValueOnce({ session: firstChild })
				.mockResolvedValueOnce({ session: secondChild }),
		});

		await orchestrator.spawnAgent("root", {
			role: "explore-a",
			prompt: "inspect first area",
		});
		await orchestrator.spawnAgent("root", {
			role: "explore-b",
			prompt: "inspect second area",
		});

		firstChild.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(root.sentMessages).toHaveLength(1);
		});
		expect(root.sentMessages[0]?.options).toBeUndefined();
		expect(root.sentMessages[0]?.message.customType).toBe("agent_idle");

		secondChild.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(root.sentMessages).toHaveLength(2);
		});
		expect(root.sentMessages[1]?.options).toEqual({ triggerTurn: true });
		expect(root.sentMessages[1]?.message.customType).toBe("agent_idle");
	});

	it("does not propagate a child's idle upstream while that child still has a running subtree", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const grandchild = new FakeSession("grandchild-session");
		child.lastAssistantText = "child summary";
		grandchild.lastAssistantText = "grandchild summary";
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn()
				.mockResolvedValueOnce({ session: child })
				.mockResolvedValueOnce({ session: grandchild }),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect the top level",
		});
		await orchestrator.spawnAgent(childId, {
			role: "explorer",
			prompt: "inspect a deeper area",
		});

		child.emit({ type: "agent_end", messages: [] });
		await waitForMicrotasks();
		expect(orchestrator.getRecord(childId).status).toBe("running");
		expect(root.sentMessages).toHaveLength(0);

		grandchild.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(child.sentMessages).toHaveLength(1);
		});
		expect(child.sentMessages[0]?.options).toEqual({ triggerTurn: true });
		expect(root.sentMessages).toHaveLength(0);

		child.emit({ type: "agent_end", messages: [] });
		await vi.waitFor(() => {
			expect(root.sentMessages).toHaveLength(1);
		});
		expect(root.sentMessages[0]?.options).toEqual({ triggerTurn: true });
		expect(root.sentMessages[0]?.message.customType).toBe("agent_idle");
	});

	it("retries idle finalization after compaction ends", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.isCompacting = true;
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "agent_end", messages: [] });
		await waitForMicrotasks();
		expect(orchestrator.getRecord(childId).status).toBe("running");
		expect(root.sentMessages).toHaveLength(0);

		child.isCompacting = false;
		child.emit({
			type: "compaction_end",
			reason: "threshold",
			result: undefined,
			aborted: false,
			willRetry: false,
		});

		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).status).toBe("idle");
		});
		expect(root.sentMessages).toHaveLength(1);
		expect(root.sentMessages[0]?.message.customType).toBe("agent_idle");
	});

	it("does not finalize idle while compaction is about to auto-retry", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.isCompacting = true;
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "agent_end", messages: [] });
		await waitForMicrotasks();

		child.isCompacting = false;
		child.emit({
			type: "compaction_end",
			reason: "overflow",
			result: undefined,
			aborted: false,
			willRetry: true,
		});

		await waitForMicrotasks();
		expect(orchestrator.getRecord(childId).status).toBe("running");
		expect(root.sentMessages).toHaveLength(0);
	});

	it("registers background and multi-agent prompt sources on the root session", () => {
		const root = new FakeSession("root-session");
		new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
			rootRole: "root",
		});

		const names = root.promptSources.map((source) => source.name);
		expect(names).toEqual([
			"orchestrator.background-capabilities",
			"orchestrator.multi-agent",
		]);
	});

	it("registers prompt sources with hasParent=true on spawned children", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		await orchestrator.spawnAgent("root", {
			role: "explorer",
			prompt: "inspect",
		});

		expect(child.promptSources).toHaveLength(2);
		const multiAgent = child.promptSources.find((source) => source.name === "orchestrator.multi-agent");
		const fragments = multiAgent?.contribute({
			sessionId: child.sessionId,
			cwd: "/tmp",
			now: new Date(),
			toolNames: [],
		});
		const text = fragments?.map((fragment) => fragment.content).join("\n");
		expect(text).toContain("Your role in the current agent tree: explorer.");
	});
});

describe("Orchestrator.aggregateSubtreeUsage", () => {
	/**
	 * Builds a SessionStats value on a FakeSession. Only fields the aggregator
	 * reads are meaningfully populated; everything else is kept at sensible
	 * defaults so we can assert on exact sums.
	 */
	const configureStats = (
		session: FakeSession,
		overrides: {
			userMessages?: number;
			assistantMessages?: number;
			toolCalls?: number;
			toolResults?: number;
			input?: number;
			output?: number;
			cacheRead?: number;
			cacheWrite?: number;
			cost?: number;
		},
	): void => {
		const input = overrides.input ?? 0;
		const output = overrides.output ?? 0;
		const cacheRead = overrides.cacheRead ?? 0;
		const cacheWrite = overrides.cacheWrite ?? 0;
		session.sessionStats = {
			sessionFile: session.sessionFile,
			sessionId: session.sessionId,
			userMessages: overrides.userMessages ?? 0,
			assistantMessages: overrides.assistantMessages ?? 0,
			toolCalls: overrides.toolCalls ?? 0,
			toolResults: overrides.toolResults ?? 0,
			totalMessages:
				(overrides.userMessages ?? 0) +
				(overrides.assistantMessages ?? 0) +
				(overrides.toolResults ?? 0),
			tokens: {
				input,
				output,
				cacheRead,
				cacheWrite,
				total: input + output + cacheRead + cacheWrite,
			},
			cost: overrides.cost ?? 0,
		};
	};

	it("returns self == tree for a leaf agent with no descendants", async () => {
		const root = new FakeSession("root-session");
		configureStats(root, {
			userMessages: 2,
			assistantMessages: 2,
			input: 1000,
			output: 200,
			cacheRead: 500,
			cacheWrite: 100,
			cost: 0.015,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
		});

		const result = orchestrator.aggregateSubtreeUsage("root");
		expect(result).toBeDefined();
		expect(result?.hasDescendants).toBe(false);
		expect(result?.agentId).toBe("root");
		expect(result?.self).toEqual(result?.tree);
		expect(result?.tree.tokens.input).toBe(1000);
		expect(result?.tree.cost).toBeCloseTo(0.015);
	});

	it("returns undefined for unknown agent ids", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
		});
		expect(orchestrator.aggregateSubtreeUsage("ghost")).toBeUndefined();
	});

	it("sums stats across a depth-1 tree (root + two children)", async () => {
		const root = new FakeSession("root-session");
		const childA = new FakeSession("child-a-session");
		const childB = new FakeSession("child-b-session");
		configureStats(root, { userMessages: 1, assistantMessages: 1, input: 100, output: 20, cost: 0.002 });
		configureStats(childA, { userMessages: 1, assistantMessages: 2, input: 400, output: 80, cacheRead: 200, cost: 0.008 });
		configureStats(childB, { userMessages: 1, assistantMessages: 3, input: 700, output: 120, cacheRead: 350, cacheWrite: 40, cost: 0.014 });

		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn()
				.mockResolvedValueOnce({ session: childA })
				.mockResolvedValueOnce({ session: childB }),
		});

		const idA = await orchestrator.spawnAgent("root", { role: "a", prompt: "" });
		const idB = await orchestrator.spawnAgent("root", { role: "b", prompt: "" });

		const result = orchestrator.aggregateSubtreeUsage("root");
		expect(result?.hasDescendants).toBe(true);
		// self covers only the root agent
		expect(result?.self.tokens.input).toBe(100);
		expect(result?.self.cost).toBeCloseTo(0.002);
		// tree sums root + both children
		expect(result?.tree.tokens.input).toBe(1200);
		expect(result?.tree.tokens.output).toBe(220);
		expect(result?.tree.tokens.cacheRead).toBe(550);
		expect(result?.tree.tokens.cacheWrite).toBe(40);
		expect(result?.tree.cost).toBeCloseTo(0.024);
		expect(result?.tree.userMessages).toBe(3);
		expect(result?.tree.assistantMessages).toBe(6);

		// Aggregating from a single child shows just that child's self/tree.
		const childAResult = orchestrator.aggregateSubtreeUsage(idA);
		expect(childAResult?.hasDescendants).toBe(false);
		expect(childAResult?.tree.tokens.input).toBe(400);

		// And a different child is summed independently.
		const childBResult = orchestrator.aggregateSubtreeUsage(idB);
		expect(childBResult?.tree.tokens.input).toBe(700);
	});

	it("sums stats across a depth-3 tree", async () => {
		const root = new FakeSession("root-session");
		const level1 = new FakeSession("level1-session");
		const level2 = new FakeSession("level2-session");
		const level3 = new FakeSession("level3-session");
		configureStats(root, { input: 100, cost: 0.001 });
		configureStats(level1, { input: 200, cost: 0.002 });
		configureStats(level2, { input: 400, cost: 0.004 });
		configureStats(level3, { input: 800, cost: 0.008 });

		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn()
				.mockResolvedValueOnce({ session: level1 })
				.mockResolvedValueOnce({ session: level2 })
				.mockResolvedValueOnce({ session: level3 }),
		});

		const l1 = await orchestrator.spawnAgent("root", { role: "l1", prompt: "" });
		const l2 = await orchestrator.spawnAgent(l1, { role: "l2", prompt: "" });
		await orchestrator.spawnAgent(l2, { role: "l3", prompt: "" });

		const rootResult = orchestrator.aggregateSubtreeUsage("root");
		expect(rootResult?.tree.tokens.input).toBe(1500);
		expect(rootResult?.tree.cost).toBeCloseTo(0.015);

		// l1 subtree sums l1 + l2 + l3.
		const l1Result = orchestrator.aggregateSubtreeUsage(l1);
		expect(l1Result?.tree.tokens.input).toBe(1400);
		expect(l1Result?.tree.cost).toBeCloseTo(0.014);

		// l2 subtree sums l2 + l3.
		const l2Result = orchestrator.aggregateSubtreeUsage(l2);
		expect(l2Result?.tree.tokens.input).toBe(1200);
		expect(l2Result?.tree.cost).toBeCloseTo(0.012);
	});

	it("treats zero-stat children as zero-contribution, not as missing", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		configureStats(root, { input: 500, cost: 0.005 });
		// child's sessionStats unset → FakeSession.getSessionStats returns all-zero stats.
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn().mockResolvedValue({ session: child }),
		});

		await orchestrator.spawnAgent("root", { role: "lazy", prompt: "" });
		const result = orchestrator.aggregateSubtreeUsage("root");
		expect(result?.hasDescendants).toBe(true);
		expect(result?.tree.tokens.input).toBe(500);
		expect(result?.tree.cost).toBeCloseTo(0.005);
	});

	it("skips disposed descendants", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		configureStats(root, { input: 100 });
		configureStats(child, { input: 10_000, cost: 1.0 });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn().mockResolvedValue({ session: child }),
		});

		const childId = await orchestrator.spawnAgent("root", { role: "dispose-me", prompt: "" });
		orchestrator.getRecord(childId).status = "disposed";

		const result = orchestrator.aggregateSubtreeUsage("root");
		expect(result?.hasDescendants).toBe(false);
		expect(result?.tree.tokens.input).toBe(100);
	});

	it("is robust to pathological cycles in childIds", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		configureStats(root, { input: 100 });
		configureStats(child, { input: 200 });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn().mockResolvedValue({ session: child }),
		});

		const childId = await orchestrator.spawnAgent("root", { role: "c", prompt: "" });
		// Fabricate a cycle: child lists root as its own child. Visited-set guard
		// in the aggregator must prevent infinite recursion.
		orchestrator.getRecord(childId).childIds.push("root");

		const result = orchestrator.aggregateSubtreeUsage("root");
		expect(result?.tree.tokens.input).toBe(300);
	});
});
