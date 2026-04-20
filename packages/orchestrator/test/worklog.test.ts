import { readFile } from "node:fs/promises";
import type { Usage } from "@pi-relay/ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { isLikelyTrivialTurn, Orchestrator } from "../src/orchestrator.js";
import { appendWorklogEntry, buildWorklogPrompt } from "../src/worklog.js";
import { cleanupTempDir, createTempDir, FakeSession, waitForMicrotasks } from "./test-helpers.js";

function createWorklogAssistant(content: string, usage?: Usage) {
	return {
		role: "assistant" as const,
		content: [
			{
				type: "toolCall" as const,
				id: "worklog-call",
				name: "worklog_update",
				arguments: { content },
			},
		],
		stopReason: "toolUse" as const,
		timestamp: Date.now(),
		...(usage ? { usage } : {}),
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
			// transformContext appends a custom message; the fork filter drops
			// non-user/assistant roles, so only the user + assistant pair reaches
			// convertToLlm (the custom transform message is filtered out).
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
				{
					role: "assistant",
					content: [{ type: "text", text: "Looking now." }],
					stopReason: "stop",
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

	it("sends only the delta since lastWorklogMessageCount to the fork, not the full transcript", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		// Eight pre-existing messages that simulate a prior turn already covered
		// by a previous worklog entry. After setting lastWorklogMessageCount to
		// their length, the fork should see ONLY the two new messages appended
		// below as its context — not all ten.
		const priorMessages = Array.from({ length: 8 }, (_, i) => ({
			role: "user" as const,
			content: [{ type: "text" as const, text: `prior ${i}` }],
			timestamp: Date.now(),
		}));
		const newTurnMessages = [
			{
				role: "user" as const,
				content: [{ type: "text" as const, text: "latest question" }],
				timestamp: Date.now(),
			},
			{
				role: "assistant" as const,
				content: [{ type: "text" as const, text: "latest answer" }],
				stopReason: "stop" as const,
				timestamp: Date.now(),
			},
		];
		const convertToLlm = vi.fn(async (messages) => [
			{
				role: "user" as const,
				content: [{ type: "text" as const, text: `converted:${messages.length}` }],
				timestamp: Date.now(),
			},
		]);
		const streamFn = vi.fn(async (_model, context) => {
			expect(context.messages[0]?.role).toBe("user");
			expect((context.messages[0]?.content[0] as { text: string }).text).toBe("converted:2");
			return {
				result: async () => createWorklogAssistant("## Findings\n- Delta slicing works."),
			} as never;
		});
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [...priorMessages, ...newTurnMessages],
			convertToLlm,
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});
		// Simulate a prior worklog entry that already covered the 8 prior messages.
		orchestrator.getRecord(childId).lastWorklogMessageCount = priorMessages.length;
		orchestrator.getRecord(childId).lastWorklogTurn = 1;

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});

		// Delta of 2 messages was converted, not all 10.
		expect(convertToLlm).toHaveBeenCalledTimes(1);
		const convertArg = convertToLlm.mock.calls[0]?.[0] as unknown[];
		expect(convertArg).toHaveLength(2);
	});

	it("drops tool-results and custom messages from the fork input, keeping only user and assistant turns", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const convertToLlm = vi.fn(async (messages) => [
			{
				role: "user" as const,
				content: [{ type: "text" as const, text: `converted:${messages.length}` }],
				timestamp: Date.now(),
			},
		]);
		const streamFn = vi.fn(async () => ({
			result: async () => createWorklogAssistant("## Findings\n- Filter works."),
		}) as never);
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user" as const,
					content: [{ type: "text" as const, text: "hello" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant" as const,
					content: [
						{
							type: "toolCall" as const,
							id: "call-1",
							name: "read",
							arguments: { path: "foo" },
						},
					],
					stopReason: "toolUse" as const,
					timestamp: Date.now(),
				},
				{
					role: "toolResult" as const,
					toolCallId: "call-1",
					toolName: "read",
					content: [{ type: "text" as const, text: "huge tool result" }],
					timestamp: Date.now(),
				},
				{
					role: "custom" as const,
					customType: "agent_roster",
					content: "## Running Subagents\n...",
					display: false,
					timestamp: Date.now(),
				},
				{
					role: "custom" as const,
					customType: "agent_directive",
					content: "[DIRECTIVE]",
					display: true,
					timestamp: Date.now(),
				},
				{
					role: "assistant" as const,
					content: [{ type: "text" as const, text: "final answer" }],
					stopReason: "stop" as const,
					timestamp: Date.now(),
				},
			],
			convertToLlm,
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});

		// Six messages in state; after filter: 1 user + 2 assistant = 3. No
		// toolResult, no custom messages.
		const convertArg = convertToLlm.mock.calls[0]?.[0] as Array<{ role: string }>;
		expect(convertArg).toHaveLength(3);
		for (const message of convertArg) {
			expect(["user", "assistant"]).toContain(message.role);
		}
	});

	it("falls back to sending everything on the first fork when lastWorklogMessageCount is zero", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const convertToLlm = vi.fn(async (messages) => [
			{
				role: "user" as const,
				content: [{ type: "text" as const, text: `converted:${messages.length}` }],
				timestamp: Date.now(),
			},
		]);
		const streamFn = vi.fn(async () => ({
			result: async () => createWorklogAssistant("## First\n- Bootstrap worked."),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user" as const,
					content: [{ type: "text" as const, text: "only message" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant" as const,
					content: [{ type: "text" as const, text: "ack" }],
					stopReason: "stop" as const,
					timestamp: Date.now(),
				},
			],
			convertToLlm,
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		const convertArg = convertToLlm.mock.calls[0]?.[0] as unknown[];
		expect(convertArg).toHaveLength(2);
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

		// Seed one user+assistant pair so each turn_end's gate sees a new
		// assistant message in the delta and lets the fork fire.
		const seedTurn = (label: string) => {
			child.agent.state.messages.push(
				{
					role: "user",
					content: [{ type: "text", text: `q ${label}` }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: `a ${label}` }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			);
		};
		seedTurn("1");
		child.emit({ type: "turn_end", messages: [] });
		seedTurn("2");
		child.emit({ type: "turn_end", messages: [] });
		seedTurn("3");
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
				{
					role: "assistant",
					content: [{ type: "text", text: "Inspecting." }],
					stopReason: "stop",
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
		// Snapshot was taken before the later custom message was pushed, so the
		// transform still sees exactly the user+assistant pair that existed at
		// turn_end time.
		expect((transformedMessages as unknown[]).length).toBe(2);
	});

	it("attributes worklog-fork usage to the child session via addBackgroundUsage", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const workUsage: Usage = {
			input: 123,
			output: 45,
			cacheRead: 678,
			cacheWrite: 0,
			totalTokens: 846,
			cost: { input: 0.01, output: 0.02, cacheRead: 0.001, cacheWrite: 0, total: 0.031 },
		};
		const streamFn = vi.fn(async () => ({
			result: async () => createWorklogAssistant("## Findings\n- Usage is captured.", workUsage),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "Inspect the orchestrator." }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: "Done." }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
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

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
		});
		// Usage lands on the CHILD session (the one that owns the worklog), not
		// the root, so subtree aggregation and the child's footer pick it up.
		expect(child.backgroundUsageCalls).toHaveLength(1);
		expect(child.backgroundUsageCalls[0]).toEqual({ usage: workUsage, scope: "worklog" });
		expect(root.backgroundUsageCalls).toHaveLength(0);
	});

	it("still records worklog usage when the assistant turn did not produce a tool call", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const noToolUsage: Usage = {
			input: 500,
			output: 12,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 512,
			cost: { input: 0.02, output: 0.001, cacheRead: 0, cacheWrite: 0, total: 0.021 },
		};
		const streamFn = vi.fn(async () => ({
			result: async () => ({
				role: "assistant" as const,
				content: [{ type: "text", text: "I have nothing durable to add yet." }],
				stopReason: "stop" as const,
				usage: noToolUsage,
				timestamp: Date.now(),
			}),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "Inspect the orchestrator." }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: "Ack." }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect the orchestrator",
		});

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await waitForMicrotasks();
		// Even a worklog fork that produces no worklog update still spent
		// tokens — they should be attributed, not dropped.
		expect(child.backgroundUsageCalls).toEqual([{ usage: noToolUsage, scope: "worklog" }]);
	});
});

describe("isLikelyTrivialTurn gate", () => {
	const baseRecord = { lastWorklogMessageCount: 0, lastWorklogTurn: 0, turnCount: 1 };

	it("HARD GATE: skips when the delta contains no assistant message", () => {
		const result = isLikelyTrivialTurn(baseRecord, [
			{
				role: "user",
				content: [{ type: "text", text: "hi" }],
				timestamp: Date.now(),
			},
		]);
		expect(result).toEqual({ skip: true, reason: "no-new-assistant-message" });
	});

	it("HARD GATE: skips when the delta is only an agent_directive delivery", () => {
		const result = isLikelyTrivialTurn(baseRecord, [
			{
				role: "custom",
				customType: "agent_directive",
				content: "[DIRECTIVE]",
				display: true,
				timestamp: Date.now(),
			},
		]);
		expect(result.skip).toBe(true);
		expect(result.reason).toBe("no-new-assistant-message");
	});

	it("HARD GATE: skips when the delta is only tool-results after an older assistant message", () => {
		// Prior assistant message is BEFORE lastWorklogMessageCount, so it's not
		// part of the delta. The delta is just the tool-result.
		const result = isLikelyTrivialTurn(
			{ lastWorklogMessageCount: 2, lastWorklogTurn: 1, turnCount: 2 },
			[
				{
					role: "user",
					content: [{ type: "text", text: "earlier q" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [
						{ type: "toolCall", id: "c1", name: "read", arguments: { path: "x" } },
					],
					stopReason: "toolUse",
					timestamp: Date.now(),
				},
				{
					role: "toolResult",
					toolCallId: "c1",
					toolName: "read",
					content: [{ type: "text", text: "huge payload" }],
					timestamp: Date.now(),
				},
			],
		);
		expect(result).toEqual({ skip: true, reason: "no-new-assistant-message" });
	});

	it("skips tool-chatter-only: delta has assistant messages but only toolCall content", () => {
		const result = isLikelyTrivialTurn(baseRecord, [
			{
				role: "user",
				content: [{ type: "text", text: "q" }],
				timestamp: Date.now(),
			},
			{
				role: "assistant",
				content: [
					{ type: "toolCall", id: "c1", name: "bash", arguments: { command: "ls" } },
				],
				stopReason: "toolUse",
				timestamp: Date.now(),
			},
		]);
		expect(result).toEqual({ skip: true, reason: "tool-chatter-only" });
	});

	it("skips tiny-delta-after-recent-entry: 1-msg delta within 2 turns of last entry", () => {
		const result = isLikelyTrivialTurn(
			{ lastWorklogMessageCount: 0, lastWorklogTurn: 5, turnCount: 6 },
			[
				{
					role: "assistant",
					content: [{ type: "text", text: "ok" }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
		);
		expect(result).toEqual({ skip: true, reason: "tiny-delta-after-recent-entry" });
	});

	it("does not skip: substantive turn with user + assistant text", () => {
		const result = isLikelyTrivialTurn(baseRecord, [
			{
				role: "user",
				content: [{ type: "text", text: "what did you learn?" }],
				timestamp: Date.now(),
			},
			{
				role: "assistant",
				content: [{ type: "text", text: "Here are the findings." }],
				stopReason: "stop",
				timestamp: Date.now(),
			},
		]);
		expect(result).toEqual({ skip: false });
	});

	it("does not skip: assistant with substantive thinking passes the tool-chatter gate", () => {
		const result = isLikelyTrivialTurn(baseRecord, [
			{
				role: "user",
				content: [{ type: "text", text: "q" }],
				timestamp: Date.now(),
			},
			{
				role: "assistant",
				content: [
					{ type: "thinking", thinking: "x".repeat(400) },
					{ type: "toolCall", id: "c1", name: "bash", arguments: { command: "ls" } },
				],
				stopReason: "toolUse",
				timestamp: Date.now(),
			},
		]);
		expect(result).toEqual({ skip: false });
	});
});

describe("worklog fork model override", () => {
	const tempDirs: string[] = [];

	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	it("uses config.forkModel and config.forkThinkingLevel when set, with a distinct :worklog cache key", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const forkModel = {
			id: "gpt-5.4",
			name: "GPT-5.4 (fork)",
			api: "openai-responses" as const,
			provider: "openai" as const,
			baseUrl: "https://api.openai.com/v1",
			reasoning: true,
			input: ["text"] as const,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
			contextWindow: 272_000,
			maxTokens: 128_000,
		};
		const streamFn = vi.fn(async (model, _context, options) => {
			expect(model).toBe(forkModel);
			expect(options?.reasoning).toBe("medium");
			// The fork must carry a distinct sessionId so providers that key
			// their prompt cache off sessionId don't cross-contaminate
			// main-loop caches.
			expect(options?.sessionId).toBe("child-session:worklog");
			return {
				result: async () => createWorklogAssistant("## Findings\n- Override wired."),
			} as never;
		});
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "q" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: "a" }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
			config: { forkModel: forkModel as never, forkThinkingLevel: "medium" },
		});
		await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
	});

	it("falls back to the session model and thinkingLevel when forkModel is not configured", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const streamFn = vi.fn(async (model, _context, options) => {
			// The child's default FakeSession model is the test TEST_MODEL; no
			// override configured, so that's what the fork must use.
			expect(model.id).toBe("gpt-5.4");
			expect(options?.reasoning).toBe("medium");
			expect(options?.sessionId).toBe("child-session:worklog");
			return {
				result: async () => createWorklogAssistant("## Findings\n- Fallback wired."),
			} as never;
		});
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "user",
					content: [{ type: "text", text: "q" }],
					timestamp: Date.now(),
				},
				{
					role: "assistant",
					content: [{ type: "text", text: "a" }],
					stopReason: "stop",
					timestamp: Date.now(),
				},
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
	});
});

describe("worklog fork gating integration", () => {
	const tempDirs: string[] = [];

	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	it("does not fire the fork on a turn that delivered only an agent_directive", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		const streamFn = vi.fn(async () => ({
			result: async () => createWorklogAssistant("## should not be written"),
		}) as never);
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{
					role: "custom",
					customType: "agent_directive",
					content: "[DIRECTIVE] something",
					display: true,
					timestamp: Date.now(),
				},
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "turn_end", messages: [] });
		await waitForMicrotasks();
		await waitForMicrotasks();
		expect(streamFn).not.toHaveBeenCalled();
		// Gated turn still advances turnCount but MUST NOT advance
		// lastWorklogMessageCount — so the delta is preserved.
		expect(orchestrator.getRecord(childId).turnCount).toBe(1);
		expect(orchestrator.getRecord(childId).lastWorklogMessageCount).toBe(0);
	});

	it("accumulated delta from a skipped turn is picked up by the next substantive turn", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-");
		tempDirs.push(sessionDir);
		let capturedDelta: number | undefined;
		const convertToLlm = vi.fn(async (messages) => {
			capturedDelta = messages.length;
			return [
				{
					role: "user" as const,
					content: [{ type: "text" as const, text: "converted" }],
					timestamp: Date.now(),
				},
			];
		});
		const streamFn = vi.fn(async () => ({
			result: async () => createWorklogAssistant("## Findings\n- Substantive content."),
		}) as never);
		const root = new FakeSession("root-session", { sessionDir });
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [],
			convertToLlm,
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		// Turn 1: trivial — directive only, no assistant. Fork is skipped.
		child.agent.state.messages.push({
			role: "custom",
			customType: "agent_directive",
			content: "[DIRECTIVE] a",
			display: true,
			timestamp: Date.now(),
		});
		child.emit({ type: "turn_end", messages: [] });
		await waitForMicrotasks();
		expect(streamFn).not.toHaveBeenCalled();

		// Turn 2: substantive — user + assistant. Fork should fire and the
		// skipped directive from turn 1 must still be in the context delta.
		child.agent.state.messages.push(
			{
				role: "user",
				content: [{ type: "text", text: "real question" }],
				timestamp: Date.now(),
			},
			{
				role: "assistant",
				content: [{ type: "text", text: "real answer" }],
				stopReason: "stop",
				timestamp: Date.now(),
			},
		);
		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(2);
		});
		// The fork's convertToLlm saw the role-filtered delta: user +
		// assistant (the skipped directive is filtered by the role filter).
		// Critically, turnCount advanced across both turns.
		expect(capturedDelta).toBe(2);
		expect(orchestrator.getRecord(childId).turnCount).toBe(2);
	});
});
