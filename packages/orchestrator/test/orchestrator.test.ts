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
});
