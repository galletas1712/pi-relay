import { readFile } from "node:fs/promises";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Orchestrator } from "../src/orchestrator.js";
import { appendWorklogEntry, buildWorklogPrompt, WORKLOG_COMPACTION_TOOL, WORKLOG_UPDATE_TOOL } from "../src/worklog.js";
import { cleanupTempDir, createTempDir, FakeSession, TEST_MODEL, waitForMicrotasks } from "./test-helpers.js";

function createWorklogAssistant(content: string) {
	return {
		role: "assistant" as const,
		content: [
			{
				type: "toolCall" as const,
				id: "worklog-call",
				name: WORKLOG_UPDATE_TOOL.name,
				arguments: { content },
			},
		],
		stopReason: "toolUse" as const,
		timestamp: Date.now(),
	};
}

function createWorklogCompactionAssistant(summary: string) {
	return {
		role: "assistant" as const,
		content: [
			{
				type: "toolCall" as const,
				id: "worklog-compaction-call",
				name: WORKLOG_COMPACTION_TOOL.name,
				arguments: { summary },
			},
		],
		stopReason: "toolUse" as const,
		timestamp: Date.now(),
	};
}

describe("worklog fork", () => {
	const tempDirs: string[] = [];

	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	it("tells child agents to batch worklog updates instead of sending routine progress", () => {
		const prompt = buildWorklogPrompt("## Entry — previous");
		expect(prompt).toContain('Do not use the worklog for step-by-step progress updates, routine status pings, or "I looked at X" notes.');
		expect(prompt).toContain("Do not log ordinary file browsing, obvious commands, or temporary hypotheses that did not matter.");
		expect(prompt).toContain("Batch related findings into one entry instead of emitting one entry per small observation.");
		expect(prompt).toContain("For short tasks, prefer a single substantial entry near the end.");
	});

	it("runs on turn_end via the transform pipeline and leaves agent context untouched", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const transformContext = vi.fn(async (messages) => [
			...messages,
			{
				role: "custom" as const,
				customType: "test_transform",
				content: "transformed context",
				display: false,
				timestamp: Date.now(),
			},
		]);
		const convertToLlm = vi.fn(async (messages) => [
			{
				role: "user" as const,
				content: [
					{
						type: "text" as const,
						text: `converted:${messages.length}`,
					},
				],
				timestamp: Date.now(),
			},
		]);
		const streamFn = vi.fn(async (_model, context) => {
			expect(context.messages[0]?.role).toBe("user");
			expect(context.messages[0]?.content[0]?.type).toBe("text");
			expect((context.messages[0]?.content[0] as { text: string }).text).toBe("converted:2");
			expect(context.messages[1]?.role).toBe("user");
			expect((context.messages[1]?.content[0] as { text: string }).text).toContain("<last-worklog-entry>");
			return {
				result: async () => createWorklogAssistant("## Findings\n- Restored sessions reopen child trees from tree.json."),
			} as never;
		});
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "Inspect the orchestrator." }],
					timestamp: Date.now(),
				},
			],
			transformContext,
			convertToLlm,
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect the orchestrator",
		});
		const originalMessages = [...child.agent.state.messages];

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});

		const worklogFile = orchestrator.getRecord(childId).worklogFile;
		expect(await readFile(worklogFile, "utf-8")).toContain("## Findings");
		expect(root.sentMessages).toHaveLength(0);
		expect(transformContext).toHaveBeenCalledTimes(1);
		expect(convertToLlm).toHaveBeenCalledTimes(1);
		expect(child.agent.state.messages).toEqual(originalMessages);
		expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
	});

	it("does not wait for pending ancestor worklog forks and prepends completed worklogs plus recent context", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const rootDeferred = Promise.withResolvers<void>();
		const root = new FakeSession("root-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "previous question" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: "previous answer" }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
				{
					role: "user",
					content: [{ type: "text", text: "latest question" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: "latest answer" }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
			streamFn: vi.fn(async () => {
				await rootDeferred.promise;
				return {
					result: async () => createWorklogAssistant("## Decisions\n- Prefer tree.json as the restore source of truth."),
				} as never;
			}),
		});
		const child = new FakeSession("child-session", { sessionDir });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const rootRecord = orchestrator.getRecord("root");
		await appendWorklogEntry(rootRecord.worklogFile, "## Decisions\n- Previous durable summary.", 1);
		rootRecord.lastWorklogTurn = 1;
		rootRecord.lastWorklogMessageCount = 2;

		root.emit({ type: "turn_end", messages: [] });
		const spawnPromise = orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "create a restore plan",
		});

		await vi.waitFor(() => {
			expect(child.prompts).toHaveLength(1);
		});

		await spawnPromise;

		expect(child.prompts[0]).toContain("<ancestor-worklog agent=\"root\" role=\"root\">");
		expect(child.prompts[0]).toContain("Previous durable summary.");
		expect(child.prompts[0]).toContain("<ancestor-recent-context agent=\"root\" role=\"root\">");
		expect(child.prompts[0]).toContain("[User]: latest question");
		expect(child.prompts[0]).toContain("[Assistant]: latest answer");
		expect(child.prompts[0]).toContain("create a restore plan");
		rootDeferred.resolve();
	});

	it("keeps raw worklogs append-only while child spawns inherit the compacted variant", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const compactingModel = {
			...TEST_MODEL,
			contextWindow: 50_000,
		};
		let worklogUpdateCount = 0;
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			model: compactingModel,
			streamFn: vi.fn(async (_model, context) => {
				const toolName = context.tools?.[0]?.name;
				if (toolName === WORKLOG_UPDATE_TOOL.name) {
					worklogUpdateCount += 1;
					const repeatedBlob =
						worklogUpdateCount === 1 ? "ALPHA ".repeat(12_500) : "BETA ".repeat(12_500);
					const marker = worklogUpdateCount === 1 ? "legacy-marker" : "recent-marker";
					return {
						result: async () => createWorklogAssistant(`## Findings\n- ${marker}\n- ${repeatedBlob}`),
					} as never;
				}
				if (toolName === WORKLOG_COMPACTION_TOOL.name) {
					return {
						result: async () =>
							createWorklogCompactionAssistant("## Durable Context\n- legacy-marker remains relevant."),
					} as never;
				}
				throw new Error(`Unexpected worklog tool request: ${toolName}`);
			}),
		});
		const grandchild = new FakeSession("grandchild-session", { sessionDir });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn(async () => ({ session: child }))
				.mockResolvedValueOnce({ session: child })
				.mockResolvedValueOnce({ session: grandchild }),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect the backend flow",
		});

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
		});

		child.emit({ type: "turn_end", messages: [] });
		const rawFile = orchestrator.getRecord(childId).worklogFile;
		const compactedFile = rawFile.replace(".worklog.md", ".compacted.worklog.md");
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(2);
		});
		await vi.waitFor(async () => {
			const compacted = await readFile(compactedFile, "utf-8");
			expect(compacted).toContain("## Summary —");
		});

		const rawWorklog = await readFile(rawFile, "utf-8");
		const compactedWorklog = await readFile(compactedFile, "utf-8");
		expect(rawWorklog).toContain("legacy-marker");
		expect(rawWorklog).toContain("recent-marker");
		expect(rawWorklog).toContain("ALPHA ALPHA ALPHA ALPHA");
		expect(rawWorklog).toContain("BETA BETA BETA BETA");
		expect(compactedWorklog).toContain("legacy-marker remains relevant.");
		expect(compactedWorklog).toContain("recent-marker");
		expect(compactedWorklog).toContain("BETA BETA BETA BETA");
		expect(compactedWorklog).not.toContain("ALPHA ALPHA ALPHA ALPHA");

		await orchestrator.spawnAgent(childId, {
			role: "inspector",
			prompt: "check inherited worklog context",
		});

		expect(grandchild.prompts[0]).toContain("legacy-marker remains relevant.");
		expect(grandchild.prompts[0]).toContain("recent-marker");
		expect(grandchild.prompts[0]).toContain("BETA BETA BETA BETA");
		expect(grandchild.prompts[0]).not.toContain("ALPHA ALPHA ALPHA ALPHA");
	});

	it("compacts oversized recent ancestor context instead of truncating it", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const childModel = {
			...TEST_MODEL,
			contextWindow: 20_000,
		};
		const coveredQuestion = "covered question";
		const coveredAnswer = "covered answer";
		const recentQuestion = `recent-overflow-question ${"RECENT ".repeat(14_000)}`;
		const recentAnswer = "recent-overflow-answer";
		const root = new FakeSession("root-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: coveredQuestion }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: coveredAnswer }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
				{
					role: "user",
					content: [{ type: "text", text: recentQuestion }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: recentAnswer }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
			streamFn: vi.fn(async (_model, context) => {
				expect(context.tools?.[0]?.name).toBe(WORKLOG_COMPACTION_TOOL.name);
				const promptText = (context.messages[0]?.content[0] as { text: string }).text;
				expect(promptText).toContain("seed-marker");
				expect(promptText).toContain("recent-overflow-question");
				return {
					result: async () =>
						createWorklogCompactionAssistant(
							"## Durable Context\n- seed-marker\n- recent-overflow-question",
						),
				} as never;
			}),
		});
		const child = new FakeSession("child-session", { sessionDir, model: childModel });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const rootRecord = orchestrator.getRecord("root");
		await appendWorklogEntry(rootRecord.worklogFile, "## Durable Context\n- seed-marker", 1);
		rootRecord.lastWorklogTurn = 1;
		rootRecord.lastWorklogMessageCount = 2;

		await orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect the overflow path",
		});

		const compactedFile = rootRecord.worklogFile.replace(".worklog.md", ".compacted.worklog.md");
		const compactedWorklog = await readFile(compactedFile, "utf-8");
		expect(compactedWorklog).toContain("recent-overflow-question");
		expect(compactedWorklog).toContain("seed-marker");
		expect(orchestrator.getRecord("root").lastWorklogMessageCount).toBe(root.agent.state.messages.length);
		expect(child.prompts[0]).toContain("recent-overflow-question");
		expect(child.prompts[0]).not.toContain("<ancestor-recent-context");
		expect(child.prompts[0]).not.toContain("[Truncated to the most recent 4000 characters");
		expect(child.prompts[0]).not.toContain(recentQuestion);
	});

	it("includes concurrent sibling spawns in each child's initial prompt", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const gate = Promise.withResolvers<void>();
		const created = new Map<string, FakeSession>();
		const root = new FakeSession("root-session", { sessionDir });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async ({ agentId }) => {
				await gate.promise;
				const session = new FakeSession(`${agentId}-session`, { sessionDir });
				created.set(agentId, session);
				return { session };
			}),
		});

		const planSpawn = orchestrator.spawnAgent("root", {
			role: "planner",
			prompt: "inspect the backend flow",
		});
		const exploreSpawn = orchestrator.spawnAgent("root", {
			role: "explorer",
			prompt: "inspect the frontend flow",
		});
		await waitForMicrotasks();
		gate.resolve();

		const [planId, exploreId] = await Promise.all([planSpawn, exploreSpawn]);
		const planPrompt = created.get(planId)?.prompts[0];
		const explorePrompt = created.get(exploreId)?.prompts[0];
		expect(planPrompt).toContain("<parent-sibling-batch parent=\"root\">");
		expect(planPrompt).toContain(exploreId);
		expect(planPrompt).toContain("inspect the frontend flow");
		expect(explorePrompt).toContain("<parent-sibling-batch parent=\"root\">");
		expect(explorePrompt).toContain(planId);
		expect(explorePrompt).toContain("inspect the backend flow");
	});

	it("serializes repeated forks and recovers after a failed worklog turn", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const firstGate = Promise.withResolvers<void>();
		const secondGate = Promise.withResolvers<void>();
		const thirdGate = Promise.withResolvers<void>();
		let callCount = 0;
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			streamFn: vi.fn(async () => {
				callCount += 1;
				if (callCount === 1) {
					await firstGate.promise;
					throw new Error("simulated overflow");
				}
				if (callCount === 2) {
					await secondGate.promise;
					return {
						result: async () => createWorklogAssistant("## Findings\n- Second turn persisted."),
					} as never;
				}
				await thirdGate.promise;
				return {
					result: async () => createWorklogAssistant("## Findings\n- Third turn persisted."),
				} as never;
			}),
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect repeated worklog turns",
		});

		child.emit({ type: "turn_end", messages: [] });
		child.emit({ type: "turn_end", messages: [] });
		child.emit({ type: "turn_end", messages: [] });

		await vi.waitFor(() => {
			expect(callCount).toBe(1);
		});

		firstGate.resolve();
		await vi.waitFor(() => {
			expect(callCount).toBe(2);
		});

		secondGate.resolve();
		await vi.waitFor(() => {
			expect(callCount).toBe(3);
		});

		thirdGate.resolve();
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(3);
		});

		const worklogFile = orchestrator.getRecord(childId).worklogFile;
		const worklog = await readFile(worklogFile, "utf-8");
		expect(root.sentMessages).toHaveLength(0);
		expect(worklog).toContain("Second turn persisted.");
		expect(worklog).toContain("Third turn persisted.");
		expect(worklog.indexOf("Second turn persisted.")).toBeLessThan(worklog.indexOf("Third turn persisted."));
		expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(3);
	});

	it("forks from a snapshot of the completed turn even if new messages arrive before the fork runs", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const transformGate = Promise.withResolvers<void>();
		let transformedMessages: unknown[] | undefined;
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "Inspect the orchestrator." }],
					timestamp: Date.now(),
				},
			],
			transformContext: vi.fn(async (messages) => {
				transformedMessages = messages;
				await transformGate.promise;
				return messages;
			}),
			streamFn: vi.fn(async () => ({
				result: async () => createWorklogAssistant("## Findings\n- Snapshot stays stable."),
			}) as never),
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect the orchestrator",
		});

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(transformedMessages).toBeDefined();
		});

		child.agent.state.messages.push({
			role: "custom",
			customType: "agent_directive",
			content: "later message",
			display: true,
			timestamp: Date.now(),
		});
		transformGate.resolve();

		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
		});
		expect(root.sentMessages).toHaveLength(0);
		expect(Array.isArray(transformedMessages)).toBe(true);
		expect((transformedMessages as unknown[]).length).toBe(1);
	});
});
