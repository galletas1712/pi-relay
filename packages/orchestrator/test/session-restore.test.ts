import { writeFileSync } from "node:fs";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Orchestrator } from "../src/orchestrator.js";
import { cleanupTempDir, createTempDir, FakeSession } from "./test-helpers.js";

function createInterruptedTranscript() {
	return [
		{
			role: "assistant" as const,
			content: [
				{
					type: "toolCall" as const,
					id: "bg-1",
					name: "bash",
					arguments: { command: "npm test", __background: true },
				},
				{
					type: "toolCall" as const,
					id: "fg-1",
					name: "read",
					arguments: { filePath: "src/index.ts" },
				},
			],
			stopReason: "toolUse" as const,
			timestamp: Date.now(),
		},
		{
			role: "toolResult" as const,
			toolCallId: "bg-1",
			toolName: "bash",
			content: [{ type: "text" as const, text: "[PENDING] bash is still running." }],
			isError: false,
			details: { pending: true, argsPreview: "{\"command\":\"npm test\"}" },
			timestamp: Date.now(),
		},
	];
}

describe("session restore", () => {
	const tempDirs: string[] = [];

	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	it("rebuilds child sessions, marks interrupted tools, and reactivates idle children", async () => {
		const sessionDir = createTempDir("pi-relay-restore-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", { sessionDir });
		const seedOrchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await seedOrchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect restore paths",
		});

		const restoredRoot = new FakeSession("root-session", {
			sessionDir,
			messages: [
				{
					role: "assistant",
					content: [
						{
							type: "toolCall",
							id: "root-bg-1",
							name: "bash",
							arguments: { command: "sleep 60", __background: true },
						},
					],
					stopReason: "toolUse",
					timestamp: Date.now(),
				},
				{
					role: "toolResult",
					toolCallId: "root-bg-1",
					toolName: "bash",
					content: [{ type: "text", text: "[PENDING] bash is still running." }],
					isError: false,
					details: { pending: true, argsPreview: "{\"command\":\"sleep 60\"}" },
					timestamp: Date.now(),
				},
			],
		});
		const restoredChild = new FakeSession("child-session", {
			sessionDir,
			sessionFile: child.sessionFile,
			messages: createInterruptedTranscript(),
		});
		const restoreFactory = vi.fn(async () => ({ session: restoredChild }));
		const restoredOrchestrator = new Orchestrator({
			rootSession: restoredRoot,
			sessionFactory: restoreFactory,
		});

		const didRestore = await restoredOrchestrator.restore();
		expect(didRestore).toBe(true);
		expect(restoreFactory).toHaveBeenCalledWith(
			expect.objectContaining({
				mode: "restore",
				agentId: childId,
				sessionFile: child.sessionFile,
			}),
		);

		const restoredRecord = restoredOrchestrator.getRecord(childId);
		expect(restoredRecord.status).toBe("idle");
		expect(restoredRecord.orphanedPendingToolCallIds).toEqual(["bg-1"]);
		expect(restoredChild.appendedSessionMessages).toHaveLength(1);
		expect((restoredChild.appendedSessionMessages[0] as { toolCallId: string }).toolCallId).toBe("fg-1");
		expect(restoredRoot.sentMessages).toHaveLength(1);
		expect(restoredRoot.sentMessages[0]?.message.customType).toBe("agent_idle");
		expect(restoredRoot.sentMessages[0]?.options).toBeUndefined();
		expect(String(restoredRoot.sentMessages[0]?.message.content)).toContain("Note: Session restored from interrupted state.");
		expect(restoredOrchestrator.consumePendingRootResume("root-session")).toBe(true);

		const transformed = await restoredRecord.session.agent.transformContext?.(restoredChild.agent.state.messages);
		expect(transformed).toBeDefined();
		const pending = transformed?.find(
			(message) => message.role === "toolResult" && "toolCallId" in message && message.toolCallId === "bg-1",
		);
		expect(pending).toBeDefined();
		if (!pending || pending.role !== "toolResult") {
			throw new Error("Pending tool result missing after restore");
		}
		expect(pending.content[0]?.type).toBe("text");
		expect((pending.content[0] as { text: string }).text).toContain("[TERMINATED]");

		const transformedRoot = await restoredRoot.agent.transformContext?.(restoredRoot.agent.state.messages);
		expect(transformedRoot).toBeDefined();
		const rootPending = transformedRoot?.find(
			(message) => message.role === "toolResult" && "toolCallId" in message && message.toolCallId === "root-bg-1",
		);
		expect(rootPending).toBeDefined();
		if (!rootPending || rootPending.role !== "toolResult") {
			throw new Error("Root pending tool result missing after restore");
		}
		expect((rootPending.content[0] as { text: string }).text).toContain("[TERMINATED]");

		await restoredOrchestrator.routeMessage("root", childId, "continue from the interrupted state");
		expect(restoredChild.sentMessages).toHaveLength(1);
		expect(restoredChild.sentMessages[0]?.options).toEqual({ triggerTurn: true });
	});

	it("skips empty session files and preserves completed background tools", async () => {
		const sessionDir = createTempDir("pi-relay-restore-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const firstChild = new FakeSession("first-child-session", { sessionDir });
		const emptyChild = new FakeSession("empty-child-session", { sessionDir });
		writeFileSync(emptyChild.sessionFile!, "", "utf-8");
		const seedOrchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn()
				.mockResolvedValueOnce({ session: firstChild })
				.mockResolvedValueOnce({ session: emptyChild }),
		});

		const firstChildId = await seedOrchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "first child",
		});
		const emptyChildId = await seedOrchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "empty child",
		});

		const restoredRoot = new FakeSession("root-session", { sessionDir });
		const restoredFirstChild = new FakeSession("first-child-session", {
			sessionDir,
			sessionFile: firstChild.sessionFile,
			messages: [
				{
					role: "assistant",
					content: [
						{
							type: "toolCall",
							id: "bg-complete",
							name: "bash",
							arguments: { command: "npm test", __background: true },
						},
					],
					stopReason: "toolUse",
					timestamp: Date.now(),
				},
				{
					role: "toolResult",
					toolCallId: "bg-complete",
					toolName: "bash",
					content: [{ type: "text", text: "[PENDING] bash is still running." }],
					isError: false,
					details: { pending: true, argsPreview: "{\"command\":\"npm test\"}" },
					timestamp: Date.now(),
				},
				{
					role: "custom",
					customType: "bg_tool_completion",
					content: "[Background tool completed] bash (bg-complete)",
					display: true,
					details: {
						toolCallId: "bg-complete",
						toolName: "bash",
						isError: false,
					},
					timestamp: Date.now(),
				},
			],
		});
		const restoreFactory = vi.fn(async () => ({ session: restoredFirstChild }));
		const restoredOrchestrator = new Orchestrator({
			rootSession: restoredRoot,
			sessionFactory: restoreFactory,
		});

		const didRestore = await restoredOrchestrator.restore();
		expect(didRestore).toBe(true);
		expect(restoreFactory).toHaveBeenCalledTimes(1);
		expect(restoredOrchestrator.getChildrenOf("root").map((record) => record.id)).toEqual([firstChildId]);

		const restoredRecord = restoredOrchestrator.getRecord(firstChildId);
		expect(restoredRecord.orphanedPendingToolCallIds).toEqual([]);
		const transformed = await restoredRecord.session.agent.transformContext?.(restoredFirstChild.agent.state.messages);
		const pending = transformed?.find(
			(message) => message.role === "toolResult" && "toolCallId" in message && message.toolCallId === "bg-complete",
		);
		if (!pending || pending.role !== "toolResult") {
			throw new Error("Completed background result missing");
		}
		expect((pending.content[0] as { text: string }).text).toContain("[PENDING]");

		const tree = JSON.parse(await readFile(join(sessionDir, "root-session", "tree.json"), "utf-8")) as {
			agents: Record<string, { status: string }>;
		};
		expect(tree.agents[emptyChildId]?.status).toBe("disposed");
	});
});
