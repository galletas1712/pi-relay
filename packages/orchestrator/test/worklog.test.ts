import { readFile } from "node:fs/promises";
import type { Usage } from "@pi-relay/ai";
import { afterEach, describe, expect, it, vi } from "vitest";
import { isLikelyTrivialTurn, Orchestrator } from "../src/orchestrator.js";
import { appendWorklogEntry, buildAncestorWorklogPrefix, buildWorklogPrompt, computeTopicVocabulary, formatWorklogEntry, parseWorklogEntries, summarizePinnedEntry, updateWorklogEntryPin } from "../src/worklog.js";
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
	const baseRecord = { lastWorklogMessageCount: 0 };

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

	it("does not skip small deltas: a single-message assistant response still fires the fork", () => {
		// There is no "tiny-delta" gate; any delta containing an assistant
		// message with substantive text should proceed to the fork.
		const result = isLikelyTrivialTurn({ lastWorklogMessageCount: 0 }, [
			{
				role: "assistant",
				content: [{ type: "text", text: "ok" }],
				stopReason: "stop",
				timestamp: Date.now(),
			},
		]);
		expect(result).toEqual({ skip: false });
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

describe("parseWorklogEntries", () => {
	it("round-trips: format → parse → reformat yields identical content", () => {
		const iso = "2026-04-20T12:00:00.000Z";
		const entry = formatWorklogEntry("## Decisions\n- First finding.", 1, {
			iso,
			topics: ["orchestrator/restore"],
			supersedes: [],
		});
		const parsed = parseWorklogEntries(`${entry}\n\n`);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]?.raw).toBe(entry);
		expect(parsed[0]?.body).toBe("## Decisions\n- First finding.");
		// A second format call with the same (content, iso) and same meta
		// must yield the same text because entry_id is deterministic.
		const again = formatWorklogEntry("## Decisions\n- First finding.", 1, {
			iso,
			topics: ["orchestrator/restore"],
			supersedes: [],
		});
		expect(again).toBe(entry);
	});

	it("parses a legacy entry (no meta comment) with meta={}, id undefined, correct body", () => {
		const legacy = "## Entry — 2026-01-01T00:00:00.000Z (turn 7)\n\n## Findings\n- legacy body text.\n\n";
		const parsed = parseWorklogEntries(legacy);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]?.meta).toEqual({});
		expect(parsed[0]?.id).toBeUndefined();
		expect(parsed[0]?.turn).toBe(7);
		expect(parsed[0]?.iso).toBe("2026-01-01T00:00:00.000Z");
		expect(parsed[0]?.body).toBe("## Findings\n- legacy body text.");
	});

	it("parses a mixed file with both legacy and structured entries", () => {
		const iso = "2026-02-02T02:02:02.000Z";
		const structured = formatWorklogEntry("## Structured\n- body A.", 2, {
			iso,
			topics: ["foo"],
			supersedes: ["deadbeef"],
		});
		const mixed =
			"## Entry — 2026-01-01T00:00:00.000Z (turn 1)\n\n## Legacy\n- legacy body.\n\n" +
			`${structured}\n\n`;
		const parsed = parseWorklogEntries(mixed);
		expect(parsed).toHaveLength(2);
		expect(parsed[0]?.meta).toEqual({});
		expect(parsed[0]?.id).toBeUndefined();
		expect(parsed[0]?.body).toBe("## Legacy\n- legacy body.");
		expect(parsed[1]?.id).toBeDefined();
		expect(parsed[1]?.meta.topics).toEqual(["foo"]);
		expect(parsed[1]?.meta.supersedes).toEqual(["deadbeef"]);
		expect(parsed[1]?.body).toBe("## Structured\n- body A.");
	});

	it("does NOT throw on malformed meta JSON; meta is {} and id undefined", () => {
		const broken = "## Entry — 2026-03-03T03:03:03.000Z (turn 4) <!-- meta: not-json -->\n\n## Body\n- content.\n\n";
		const parsed = parseWorklogEntries(broken);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]?.meta).toEqual({});
		expect(parsed[0]?.id).toBeUndefined();
		expect(parsed[0]?.body).toBe("## Body\n- content.");
	});

	it("accepts an empty meta object {} with all fields optional", () => {
		const entry = "## Entry — 2026-03-03T03:03:03.000Z (turn 4) <!-- meta: {} -->\n\n## Body\n- content.\n\n";
		const parsed = parseWorklogEntries(entry);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]?.meta).toEqual({});
		expect(parsed[0]?.id).toBeUndefined();
	});

	it("round-trips supersedes and topics exactly", () => {
		const iso = "2026-04-04T04:04:04.000Z";
		const entry = formatWorklogEntry("## X", 3, {
			iso,
			topics: ["caching/anthropic", "orchestrator/restore"],
			supersedes: ["abcd1234", "11112222"],
		});
		const parsed = parseWorklogEntries(entry);
		expect(parsed[0]?.meta.topics).toEqual(["caching/anthropic", "orchestrator/restore"]);
		expect(parsed[0]?.meta.supersedes).toEqual(["abcd1234", "11112222"]);
	});

	it("computes a deterministic entry_id for the same (content, iso) pair", () => {
		const iso = "2026-05-05T05:05:05.000Z";
		const a = formatWorklogEntry("same content", 1, { iso });
		const b = formatWorklogEntry("same content", 2, { iso });
		const pa = parseWorklogEntries(a)[0];
		const pb = parseWorklogEntries(b)[0];
		expect(pa?.id).toBeDefined();
		expect(pa?.id).toBe(pb?.id);
		const c = formatWorklogEntry("different content", 1, { iso });
		const pc = parseWorklogEntries(c)[0];
		expect(pc?.id).not.toBe(pa?.id);
	});

	it("preserves whitespace in a multi-line body with code blocks", () => {
		const iso = "2026-06-06T06:06:06.000Z";
		const content = "## Notes\n\n```ts\nfunction f() {\n  return 1;\n}\n```\n\nTrailing line.";
		const entry = formatWorklogEntry(content, 1, { iso });
		const parsed = parseWorklogEntries(`${entry}\n\n`);
		// Body is the original content (format trims leading/trailing
		// whitespace on write). Parse trims trailing whitespace too.
		expect(parsed[0]?.body).toBe(content);
	});
});

describe("appendWorklogEntry with meta", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	it("writes topics and supersedes into the header comment", async () => {
		const dir = createTempDir("pi-relay-worklog-meta-");
		tempDirs.push(dir);
		const filePath = `${dir}/a.worklog.md`;
		const returned = await appendWorklogEntry(filePath, "## Content", 5, {
			topics: ["foo", "bar"],
			supersedes: ["abcd1234"],
			pin: false,
		});
		const disk = await readFile(filePath, "utf-8");
		expect(disk).toContain(returned);
		expect(disk).toMatch(/<!-- meta: \{.*"topics":\["foo","bar"\].*\} -->/);
		expect(disk).toMatch(/"supersedes":\["abcd1234"\]/);
		expect(disk).toMatch(/"entry_id":"[0-9a-f]{8}"/);
		expect(disk).toMatch(/"pin":false/);
	});

	it("appending to a file without trailing newline does not corrupt format", async () => {
		const dir = createTempDir("pi-relay-worklog-meta-");
		tempDirs.push(dir);
		const filePath = `${dir}/b.worklog.md`;
		// Seed the file with a legacy entry that is missing its trailing newlines.
		const { writeFile } = await import("node:fs/promises");
		await writeFile(filePath, "## Entry — 2026-01-01T00:00:00.000Z (turn 1)\n\nlegacy body", "utf-8");
		await appendWorklogEntry(filePath, "## New\n- second entry.", 2, { topics: ["t"] });
		const disk = await readFile(filePath, "utf-8");
		const parsed = parseWorklogEntries(disk);
		// Both entries must still parse, with no cross-entry corruption.
		expect(parsed).toHaveLength(2);
		expect(parsed[0]?.body).toContain("legacy body");
		expect(parsed[1]?.body).toBe("## New\n- second entry.");
		expect(parsed[1]?.meta.topics).toEqual(["t"]);
	});

	it("returned entry string matches the text persisted on disk", async () => {
		const dir = createTempDir("pi-relay-worklog-meta-");
		tempDirs.push(dir);
		const filePath = `${dir}/c.worklog.md`;
		const returned = await appendWorklogEntry(filePath, "## A", 9, {
			topics: ["z"],
		});
		const disk = await readFile(filePath, "utf-8");
		expect(disk.startsWith(returned)).toBe(true);
		// File must end with a blank-line separator so the next append stays
		// on its own `## Entry —` line start.
		expect(disk.endsWith("\n\n")).toBe(true);
	});
});

describe("buildWorklogPrompt topic vocabulary", () => {
	it("omits the <topic-vocabulary> section when no vocabulary is provided", () => {
		const prompt = buildWorklogPrompt(undefined);
		expect(prompt).not.toContain("<topic-vocabulary>");
		// And omits when given an empty array.
		const empty = buildWorklogPrompt(undefined, []);
		expect(empty).not.toContain("<topic-vocabulary>");
	});

	it("lists vocabulary slugs with counts when a small vocabulary is provided", () => {
		const prompt = buildWorklogPrompt(undefined, [
			{ slug: "caching/anthropic", count: 4 },
			{ slug: "orchestrator/restore", count: 2 },
			{ slug: "worklog/fork", count: 1 },
		]);
		expect(prompt).toContain("<topic-vocabulary>");
		expect(prompt).toContain("- caching/anthropic (4)");
		expect(prompt).toContain("- orchestrator/restore (2)");
		expect(prompt).toContain("- worklog/fork (1)");
	});

	it("caps topic vocabulary via computeTopicVocabulary to the top 30 slugs", () => {
		// Build 40 entries each with a distinct topic and descending counts so
		// the sort is deterministic. We expect only the top 30 to survive.
		const entries = Array.from({ length: 40 }, (_, i) => ({
			id: `id-${i}`,
			iso: `2026-01-01T00:00:0${(i % 10)}.000Z`,
			turn: i,
			meta: { topics: Array.from({ length: 40 - i }, () => `t${i}`) },
			body: "",
			raw: "",
		}));
		const vocab = computeTopicVocabulary(entries);
		expect(vocab).toHaveLength(30);
		// First element is the highest count.
		expect(vocab[0]?.slug).toBe("t0");
		expect(vocab[0]?.count).toBe(40);
	});

	it("returns empty vocabulary when entries carry no topics (legacy-only file)", () => {
		const vocab = computeTopicVocabulary([
			{ id: undefined, iso: "x", turn: 1, meta: {}, body: "", raw: "" },
			{ id: undefined, iso: "y", turn: 2, meta: {}, body: "", raw: "" },
		]);
		expect(vocab).toEqual([]);
	});
});

describe("buildWorklogPrompt currently-pinned section", () => {
	it("omits the <currently-pinned> section when no pins are provided", () => {
		const prompt = buildWorklogPrompt(undefined, []);
		expect(prompt).not.toContain("<currently-pinned>");
		const empty = buildWorklogPrompt(undefined, [], []);
		expect(empty).not.toContain("<currently-pinned>");
	});

	it("lists pinned entry_ids and summaries when provided", () => {
		const prompt = buildWorklogPrompt(undefined, [], [
			{ entry_id: "abcd1234", summary: "cache-key order is tools -> system -> messages" },
			{ entry_id: "11112222", summary: "lastWorklogMessageCount is the hinge" },
		]);
		expect(prompt).toContain("<currently-pinned>");
		expect(prompt).toContain("- abcd1234 — cache-key order is tools -> system -> messages");
		expect(prompt).toContain("- 11112222 — lastWorklogMessageCount is the hinge");
	});

	it("summarizePinnedEntry collapses multi-line bodies to a single line and caps at 80 chars", () => {
		const short = summarizePinnedEntry({ id: "x", iso: "", turn: 0, meta: {}, body: "short body", raw: "" });
		expect(short).toBe("short body");
		const multiline = summarizePinnedEntry({ id: "x", iso: "", turn: 0, meta: {}, body: "line 1\n\nline 2", raw: "" });
		expect(multiline).toBe("line 1 line 2");
		const longBody = "x".repeat(200);
		const capped = summarizePinnedEntry({ id: "x", iso: "", turn: 0, meta: {}, body: longBody, raw: "" });
		expect(capped.length).toBe(80);
		expect(capped.endsWith("...")).toBe(true);
	});
});

describe("worklog fork meta integration", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	function createToolCallAssistant(args: Record<string, unknown>, usage?: Usage) {
		return {
			role: "assistant" as const,
			content: [
				{
					type: "toolCall" as const,
					id: "worklog-call",
					name: "worklog_update",
					arguments: args,
				},
			],
			stopReason: "toolUse" as const,
			timestamp: Date.now(),
			...(usage ? { usage } : {}),
		};
	}

	it("persists topics from the fork tool call into the worklog header", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-meta-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const streamFn = vi.fn(async () => ({
			result: async () =>
				createToolCallAssistant({
					content: "## Findings\n- topics test.",
					topics: ["caching/anthropic", "worklog/fork"],
				}),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
		});

		const worklogFile = orchestrator.getRecord(childId).worklogFile;
		const disk = await readFile(worklogFile, "utf-8");
		const parsed = parseWorklogEntries(disk);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]?.meta.topics).toEqual(["caching/anthropic", "worklog/fork"]);
		expect(parsed[0]?.id).toBeDefined();
	});

	it("persists supersedes citations from the fork tool call", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-meta-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const streamFn = vi.fn(async () => ({
			result: async () =>
				createToolCallAssistant({
					content: "## Correction\n- supersede prior entry.",
					supersedes: ["abcd1234"],
				}),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
		});

		const worklogFile = orchestrator.getRecord(childId).worklogFile;
		const disk = await readFile(worklogFile, "utf-8");
		const parsed = parseWorklogEntries(disk);
		expect(parsed[0]?.meta.supersedes).toEqual(["abcd1234"]);
	});

	it("legacy behavior: fork omitting topics/supersedes still yields a valid meta block", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-meta-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const streamFn = vi.fn(async () => ({
			result: async () =>
				createToolCallAssistant({
					content: "## Findings\n- no explicit topics.",
				}),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(orchestrator.getRecord(childId).lastWorklogTurn).toBe(1);
		});

		const worklogFile = orchestrator.getRecord(childId).worklogFile;
		const disk = await readFile(worklogFile, "utf-8");
		const parsed = parseWorklogEntries(disk);
		expect(parsed).toHaveLength(1);
		// Omitted fields default to empty arrays / false so downstream
		// consumers don't need null-checks.
		expect(parsed[0]?.meta.topics).toEqual([]);
		expect(parsed[0]?.meta.supersedes).toEqual([]);
		expect(parsed[0]?.meta.pin).toBe(false);
		expect(parsed[0]?.id).toBeDefined();
	});
});

describe("buildAncestorWorklogPrefix supersession tombstones", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	async function writeWorklog(filePath: string, body: string): Promise<void> {
		const { writeFile, mkdir } = await import("node:fs/promises");
		const { dirname } = await import("node:path");
		await mkdir(dirname(filePath), { recursive: true });
		await writeFile(filePath, body, "utf-8");
	}

	function entryWithMeta(
		content: string,
		turn: number,
		iso: string,
		meta: { topics?: string[]; supersedes?: string[]; pin?: boolean } = {},
	): { text: string; id: string } {
		const text = formatWorklogEntry(content, turn, { iso, ...meta });
		const parsed = parseWorklogEntries(text);
		const id = parsed[0]?.id;
		if (!id) throw new Error("expected entry_id on structured entry");
		return { text, id };
	}

	it("chain A→B→C: A supersedes B, B supersedes C. Only A survives.", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/a.worklog.md`;
		// Write entries in file order C, B, A so A is newest. A supersedes B,
		// B supersedes C. Tombstone set = {B.id, C.id}. Only A survives.
		const c = entryWithMeta("## C", 1, "2026-01-01T00:00:01.000Z");
		const b = entryWithMeta("## B", 2, "2026-01-01T00:00:02.000Z", { supersedes: [c.id] });
		const a = entryWithMeta("## A", 3, "2026-01-01T00:00:03.000Z", { supersedes: [b.id] });
		await writeWorklog(filePath, `${c.text}\n\n${b.text}\n\n${a.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toContain(a.text);
		expect(out).not.toContain(b.text);
		expect(out).not.toContain(c.text);
	});

	it("single supersede: B supersedes A. Only B survives.", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/single.worklog.md`;
		const a = entryWithMeta("## A-body", 1, "2026-02-01T00:00:01.000Z");
		const b = entryWithMeta("## B-body", 2, "2026-02-01T00:00:02.000Z", { supersedes: [a.id] });
		await writeWorklog(filePath, `${a.text}\n\n${b.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toContain(b.text);
		expect(out).not.toContain("## A-body");
	});

	it("supersedes citing an unknown entry_id: no-op, all entries survive", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/unknown.worklog.md`;
		const a = entryWithMeta("## A", 1, "2026-03-01T00:00:01.000Z");
		const b = entryWithMeta("## B", 2, "2026-03-01T00:00:02.000Z", {
			supersedes: ["deadbeef"],
		});
		await writeWorklog(filePath, `${a.text}\n\n${b.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toContain(a.text);
		expect(out).toContain(b.text);
	});

	it("legacy file (no meta on any entry): filter is a no-op, all survive", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/legacy.worklog.md`;
		const legacy =
			"## Entry — 2026-01-01T00:00:00.000Z (turn 1)\n\n## L1\n- legacy one.\n\n" +
			"## Entry — 2026-01-01T00:00:01.000Z (turn 2)\n\n## L2\n- legacy two.\n\n";
		await writeWorklog(filePath, legacy);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toContain("## L1\n- legacy one.");
		expect(out).toContain("## L2\n- legacy two.");
		// Wrapper is present.
		expect(out).toMatch(/<ancestor-worklog agent="x" role="r">/);
	});

	it("mixed file: structured entry supersedes legacy entry → legacy survives because it has no entry_id", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/mixed.worklog.md`;
		// Legacy entry has no id, so even if a structured entry claims to
		// supersede "some-id", the legacy entry can never match the
		// tombstone set.
		const legacy = "## Entry — 2026-04-01T00:00:00.000Z (turn 1)\n\n## Legacy body\n- kept.\n\n";
		const structured = entryWithMeta("## S", 2, "2026-04-01T00:00:02.000Z", {
			supersedes: ["ffff0000"], // arbitrary; legacy entry has no id so is unaffected
		});
		await writeWorklog(filePath, `${legacy}${structured.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toContain("## Legacy body\n- kept.");
		expect(out).toContain(structured.text);
	});

	it("cross-file: parent worklog supersedes a grandparent entry → grandparent entry tombstoned", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const grandparentFile = `${dir}/gp.worklog.md`;
		const parentFile = `${dir}/p.worklog.md`;

		const gpEntry = entryWithMeta("## grandparent-fact", 1, "2026-05-01T00:00:01.000Z");
		const gpKept = entryWithMeta("## grandparent-other", 2, "2026-05-01T00:00:02.000Z");
		await writeWorklog(grandparentFile, `${gpEntry.text}\n\n${gpKept.text}\n\n`);

		const parentEntry = entryWithMeta("## parent-correction", 1, "2026-05-02T00:00:01.000Z", {
			supersedes: [gpEntry.id],
		});
		await writeWorklog(parentFile, `${parentEntry.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "gp", role: "grandparent", filePath: grandparentFile },
			{ agentId: "p", role: "parent", filePath: parentFile },
		]);
		expect(out).not.toContain("## grandparent-fact");
		expect(out).toContain("## grandparent-other");
		expect(out).toContain("## parent-correction");
		// Both wrappers still emitted (grandparent still has a surviving entry).
		expect(out).toMatch(/<ancestor-worklog agent="gp"/);
		expect(out).toMatch(/<ancestor-worklog agent="p"/);
	});

	it("cross-file: a file whose every entry is tombstoned gets its wrapper skipped", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const gpFile = `${dir}/gp.worklog.md`;
		const pFile = `${dir}/p.worklog.md`;
		const only = entryWithMeta("## only", 1, "2026-05-03T00:00:01.000Z");
		await writeWorklog(gpFile, `${only.text}\n\n`);
		const correction = entryWithMeta("## correction", 1, "2026-05-03T00:00:02.000Z", {
			supersedes: [only.id],
		});
		await writeWorklog(pFile, `${correction.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "gp", role: "grandparent", filePath: gpFile },
			{ agentId: "p", role: "parent", filePath: pFile },
		]);
		expect(out).not.toMatch(/<ancestor-worklog agent="gp"/);
		expect(out).toMatch(/<ancestor-worklog agent="p"/);
	});

	it("circular supersede (A supersedes B, B supersedes A): both tombstoned", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/circular.worklog.md`;
		// Construct the supersedes references after we know both ids. Since
		// entry_id is SHA1(content+iso), we can compute one id first, then
		// reference it. To actually create a cycle we need both entries to
		// cite each other's ids — which means we need to know B's id before
		// writing A. Workaround: write A with a placeholder, parse, then
		// rewrite. Simpler: manually construct entries with chosen ids by
		// varying content until both reference each other. Instead, construct
		// the file contents directly with hand-crafted meta.
		const isoA = "2026-06-01T00:00:01.000Z";
		const isoB = "2026-06-01T00:00:02.000Z";
		const idA = "aaaa1111";
		const idB = "bbbb2222";
		// Neither id will be the real SHA1 of content/iso — but
		// parseWorklogEntries reads meta.entry_id from the JSON, not by
		// recomputing. So handcrafted ids work.
		const remainderId = "cccc3333";
		const entryA = `## Entry — ${isoA} (turn 1) <!-- meta: ${JSON.stringify({ entry_id: idA, topics: [], supersedes: [idB], pin: false })} -->\n\n## A-body`;
		const entryB = `## Entry — ${isoB} (turn 2) <!-- meta: ${JSON.stringify({ entry_id: idB, topics: [], supersedes: [idA], pin: false })} -->\n\n## B-body`;
		const entryC = `## Entry — 2026-06-01T00:00:03.000Z (turn 3) <!-- meta: ${JSON.stringify({ entry_id: remainderId, topics: [], supersedes: [], pin: false })} -->\n\n## C-body`;
		await writeWorklog(filePath, `${entryA}\n\n${entryB}\n\n${entryC}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).not.toContain("## A-body");
		expect(out).not.toContain("## B-body");
		expect(out).toContain("## C-body");
	});

	it("empty worklog: returns empty string, no crash", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/empty.worklog.md`;
		await writeWorklog(filePath, "");
		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toBe("");
	});

	it("nonexistent worklog file: returns empty string, no crash", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/nope.worklog.md`;
		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toBe("");
	});

	it("entry_id referenced in supersedes but not present anywhere: no-op, no crash", async () => {
		const dir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(dir);
		const filePath = `${dir}/orphan-ref.worklog.md`;
		const a = entryWithMeta("## A", 1, "2026-07-01T00:00:01.000Z", {
			supersedes: ["00001111", "22223333"],
		});
		await writeWorklog(filePath, `${a.text}\n\n`);
		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).toContain(a.text);
	});

	// Production call-site regression: buildSpawnPrompt used to call
	// buildAncestorWorklogPrefix once per ancestor, so cross-file tombstoning
	// collapsed to single-file (ineffective). This test spawns grandparent →
	// parent → child via the real Orchestrator so any regression to per-file
	// calls will resurrect the grandparent entry in the grandchild's prompt.
	it("grandchild spawn prompt: parent tombstones grandparent entry end-to-end", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-ts-");
		tempDirs.push(sessionDir);

		const gpIso = "2026-08-01T00:00:01.000Z";
		const gpEntry = entryWithMeta("## gp-fact-body", 1, gpIso);

		const root = new FakeSession("root-session", { sessionDir });
		const parent = new FakeSession("parent-session", { sessionDir });
		const grandchild = new FakeSession("grandchild-session", { sessionDir });
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi
				.fn()
				.mockResolvedValueOnce({ session: parent })
				.mockResolvedValueOnce({ session: grandchild }),
		});

		// Seed root (grandparent) worklog with a single structured entry.
		const rootRecord = orchestrator.getRecord("root");
		await writeWorklog(rootRecord.worklogFile, `${gpEntry.text}\n\n`);

		// Spawn parent first (so the parent record exists and gets a
		// worklog file path we can seed). Note: writing to the parent's
		// worklog AFTER spawnAgent resolves is fine — the grandchild spawn
		// below is the one whose buildSpawnPrompt reads ancestor files.
		const parentId = await orchestrator.spawnAgent("root", {
			role: "parent-role",
			prompt: "parent task",
		});

		// Seed parent worklog with an entry that supersedes the grandparent's.
		const parentRecord = orchestrator.getRecord(parentId);
		const parentEntry = entryWithMeta(
			"## parent-correction-body",
			1,
			"2026-08-02T00:00:01.000Z",
			{ supersedes: [gpEntry.id] },
		);
		await writeWorklog(parentRecord.worklogFile, `${parentEntry.text}\n\n`);

		// Now spawn the grandchild from the parent. buildSpawnPrompt walks
		// ancestors [root, parent] and should tombstone the grandparent entry.
		await orchestrator.spawnAgent(parentId, {
			role: "grandchild-role",
			prompt: "grandchild task",
		});

		await vi.waitFor(() => {
			expect(grandchild.prompts).toHaveLength(1);
		});

		const grandchildPrompt = grandchild.prompts[0] ?? "";
		expect(grandchildPrompt).not.toContain("## gp-fact-body");
		expect(grandchildPrompt).toContain("## parent-correction-body");
		// Both ancestor-worklog wrappers still rendered: the grandparent file
		// parsed to zero surviving entries (its wrapper should be skipped),
		// so only the parent's wrapper should appear. The parent's wrapper
		// must be present.
		expect(grandchildPrompt).toMatch(
			new RegExp(`<ancestor-worklog agent="${parentId}" role="parent-role">`),
		);
		// The grandparent's ancestor-worklog wrapper should be skipped since
		// all of its entries were tombstoned.
		expect(grandchildPrompt).not.toMatch(/<ancestor-worklog agent="root" role="root">/);
	});
});

describe("pinned facts in buildAncestorWorklogPrefix", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	async function writeWorklog(filePath: string, body: string): Promise<void> {
		const { writeFile, mkdir } = await import("node:fs/promises");
		const { dirname } = await import("node:path");
		await mkdir(dirname(filePath), { recursive: true });
		await writeFile(filePath, body, "utf-8");
	}

	function entryWithMeta(
		content: string,
		turn: number,
		iso: string,
		meta: { topics?: string[]; supersedes?: string[]; pin?: boolean } = {},
	): { text: string; id: string } {
		const text = formatWorklogEntry(content, turn, { iso, ...meta });
		const parsed = parseWorklogEntries(text);
		const id = parsed[0]?.id;
		if (!id) throw new Error("expected entry_id on structured entry");
		return { text, id };
	}

	it("no pins anywhere: no <pinned-facts> block; output identical to PR-5 shape", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/p.worklog.md`;
		const a = entryWithMeta("## A", 1, "2026-09-01T00:00:01.000Z");
		const b = entryWithMeta("## B", 2, "2026-09-01T00:00:02.000Z");
		await writeWorklog(filePath, `${a.text}\n\n${b.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "x", role: "r", filePath },
		]);
		expect(out).not.toContain("<pinned-facts>");
		expect(out).toContain(a.text);
		expect(out).toContain(b.text);
	});

	it("one pinned entry in parent: <pinned-facts> contains it; non-pinned entries go to <ancestor-worklog>", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/p.worklog.md`;
		const pinned = entryWithMeta("## pinned-body", 1, "2026-09-02T00:00:01.000Z", { pin: true });
		const ordinary = entryWithMeta("## ordinary-body", 2, "2026-09-02T00:00:02.000Z");
		await writeWorklog(filePath, `${pinned.text}\n\n${ordinary.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "parent", role: "parent-role", filePath },
		]);
		expect(out).toMatch(/<pinned-facts>/);
		expect(out).toMatch(/<\/pinned-facts>/);
		// Pinned body is inside the pinned-facts block, NOT in the per-file wrapper.
		const pinnedFactsIdx = out.indexOf("<pinned-facts>");
		const pinnedFactsEnd = out.indexOf("</pinned-facts>");
		const pinnedFactsBlock = out.slice(pinnedFactsIdx, pinnedFactsEnd);
		expect(pinnedFactsBlock).toContain("## pinned-body");
		expect(pinnedFactsBlock).toMatch(
			new RegExp(`<entry agent="parent" role="parent-role" entry_id="${pinned.id}">`),
		);
		// Ordinary entry is in the ancestor-worklog wrapper.
		const wrapperIdx = out.indexOf(`<ancestor-worklog agent="parent"`);
		expect(wrapperIdx).toBeGreaterThan(pinnedFactsEnd);
		expect(out.slice(wrapperIdx)).toContain("## ordinary-body");
		// Sanity: pinned body appears ONCE across the whole output.
		const matches = out.match(/## pinned-body/g) ?? [];
		expect(matches).toHaveLength(1);
	});

	it("multiple pins across ancestors: all appear in <pinned-facts> in ancestor order, then per-file entry order", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const rootFile = `${dir}/root.worklog.md`;
		const parentFile = `${dir}/parent.worklog.md`;

		// Root: two pins + one plain entry between them.
		const r1 = entryWithMeta("## root-pin-1", 1, "2026-09-03T00:00:01.000Z", { pin: true });
		const r2 = entryWithMeta("## root-plain", 2, "2026-09-03T00:00:02.000Z");
		const r3 = entryWithMeta("## root-pin-2", 3, "2026-09-03T00:00:03.000Z", { pin: true });
		await writeWorklog(rootFile, `${r1.text}\n\n${r2.text}\n\n${r3.text}\n\n`);
		// Parent: one pin.
		const p1 = entryWithMeta("## parent-pin", 1, "2026-09-03T01:00:00.000Z", { pin: true });
		await writeWorklog(parentFile, `${p1.text}\n\n`);

		const out = await buildAncestorWorklogPrefix([
			{ agentId: "root", role: "root", filePath: rootFile },
			{ agentId: "parent", role: "parent-role", filePath: parentFile },
		]);

		const pinnedFactsIdx = out.indexOf("<pinned-facts>");
		const pinnedFactsEnd = out.indexOf("</pinned-facts>");
		expect(pinnedFactsIdx).toBeGreaterThanOrEqual(0);
		const block = out.slice(pinnedFactsIdx, pinnedFactsEnd);
		// Ancestor order: root-pin-1, root-pin-2, parent-pin.
		const idx1 = block.indexOf("## root-pin-1");
		const idx2 = block.indexOf("## root-pin-2");
		const idx3 = block.indexOf("## parent-pin");
		expect(idx1).toBeGreaterThanOrEqual(0);
		expect(idx2).toBeGreaterThan(idx1);
		expect(idx3).toBeGreaterThan(idx2);
	});

	it("pinned entry appears ONCE — never duplicated in its <ancestor-worklog> wrapper", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/p.worklog.md`;
		const pinned = entryWithMeta("## unique-pinned-text", 1, "2026-09-04T00:00:01.000Z", { pin: true });
		await writeWorklog(filePath, `${pinned.text}\n\n`);
		const out = await buildAncestorWorklogPrefix([
			{ agentId: "a", role: "r", filePath },
		]);
		const occurrences = out.match(/## unique-pinned-text/g) ?? [];
		expect(occurrences).toHaveLength(1);
	});

	it("pinned entry in the tombstone set: pin beats tombstone; entry still appears in <pinned-facts>", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/p.worklog.md`;
		const pinned = entryWithMeta("## pinned-and-tombstoned", 1, "2026-09-05T00:00:01.000Z", { pin: true });
		// A later entry claims to supersede the pinned one.
		const supersede = entryWithMeta(
			"## tries-to-tombstone",
			2,
			"2026-09-05T00:00:02.000Z",
			{ supersedes: [pinned.id] },
		);
		await writeWorklog(filePath, `${pinned.text}\n\n${supersede.text}\n\n`);
		const out = await buildAncestorWorklogPrefix([
			{ agentId: "a", role: "r", filePath },
		]);
		// Pin beats tombstone — pinned body must be in <pinned-facts>.
		const pinnedFactsIdx = out.indexOf("<pinned-facts>");
		const pinnedFactsEnd = out.indexOf("</pinned-facts>");
		const block = out.slice(pinnedFactsIdx, pinnedFactsEnd);
		expect(block).toContain("## pinned-and-tombstoned");
		// And the pinned entry is NOT duplicated in the ancestor-worklog wrapper
		// (even though it's also not tombstoned there).
		const wrapperIdx = out.indexOf("<ancestor-worklog");
		expect(wrapperIdx).toBeGreaterThan(pinnedFactsEnd);
		const wrapper = out.slice(wrapperIdx);
		expect(wrapper).not.toContain("## pinned-and-tombstoned");
		// The superseding entry itself is still visible (it's not tombstoned).
		expect(wrapper).toContain("## tries-to-tombstone");
	});

	it("legacy-only file: no <pinned-facts> block (legacy entries have no pin field)", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/legacy.worklog.md`;
		const legacy =
			"## Entry — 2026-09-06T00:00:00.000Z (turn 1)\n\n## Legacy body\n- kept.\n\n";
		await writeWorklog(filePath, legacy);
		const out = await buildAncestorWorklogPrefix([
			{ agentId: "a", role: "r", filePath },
		]);
		expect(out).not.toContain("<pinned-facts>");
		expect(out).toContain("## Legacy body\n- kept.");
	});
});

describe("updateWorklogEntryPin", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	async function writeWorklog(filePath: string, body: string): Promise<void> {
		const { writeFile, mkdir } = await import("node:fs/promises");
		const { dirname } = await import("node:path");
		await mkdir(dirname(filePath), { recursive: true });
		await writeFile(filePath, body, "utf-8");
	}

	function entryWithMeta(
		content: string,
		turn: number,
		iso: string,
		meta: { topics?: string[]; supersedes?: string[]; pin?: boolean } = {},
	): { text: string; id: string } {
		const text = formatWorklogEntry(content, turn, { iso, ...meta });
		const parsed = parseWorklogEntries(text);
		const id = parsed[0]?.id;
		if (!id) throw new Error("expected entry_id on structured entry");
		return { text, id };
	}

	it("flips pin:true → pin:false; returns true; file on disk updated", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/flip.worklog.md`;
		const p = entryWithMeta("## body-X", 1, "2026-10-01T00:00:01.000Z", { pin: true });
		await writeWorklog(filePath, `${p.text}\n\n`);

		const result = await updateWorklogEntryPin(filePath, p.id, false);
		expect(result).toBe(true);
		const disk = await readFile(filePath, "utf-8");
		const parsed = parseWorklogEntries(disk);
		expect(parsed).toHaveLength(1);
		expect(parsed[0]?.id).toBe(p.id);
		expect(parsed[0]?.meta.pin).toBe(false);
		// Body preserved byte-for-byte.
		expect(parsed[0]?.body).toBe("## body-X");
	});

	it("flips pin:false → pin:true; round-trip works", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/flip2.worklog.md`;
		const p = entryWithMeta("## body-Y", 1, "2026-10-02T00:00:01.000Z", { pin: false });
		await writeWorklog(filePath, `${p.text}\n\n`);

		const r1 = await updateWorklogEntryPin(filePath, p.id, true);
		expect(r1).toBe(true);
		const parsed1 = parseWorklogEntries(await readFile(filePath, "utf-8"));
		expect(parsed1[0]?.meta.pin).toBe(true);

		const r2 = await updateWorklogEntryPin(filePath, p.id, false);
		expect(r2).toBe(true);
		const parsed2 = parseWorklogEntries(await readFile(filePath, "utf-8"));
		expect(parsed2[0]?.meta.pin).toBe(false);
	});

	it("entry not found: returns false; file untouched", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/miss.worklog.md`;
		const p = entryWithMeta("## body-Z", 1, "2026-10-03T00:00:01.000Z");
		await writeWorklog(filePath, `${p.text}\n\n`);
		const before = await readFile(filePath, "utf-8");

		const result = await updateWorklogEntryPin(filePath, "deadbeef", false);
		expect(result).toBe(false);
		const after = await readFile(filePath, "utf-8");
		expect(after).toBe(before);
	});

	it("preserves all other meta fields exactly (entry_id, topics, supersedes)", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/preserve.worklog.md`;
		const p = entryWithMeta("## body", 1, "2026-10-04T00:00:01.000Z", {
			pin: true,
			topics: ["alpha", "beta"],
			supersedes: ["abcd1234", "11112222"],
		});
		await writeWorklog(filePath, `${p.text}\n\n`);

		await updateWorklogEntryPin(filePath, p.id, false);
		const disk = await readFile(filePath, "utf-8");
		const parsed = parseWorklogEntries(disk);
		expect(parsed[0]?.id).toBe(p.id);
		expect(parsed[0]?.meta.topics).toEqual(["alpha", "beta"]);
		expect(parsed[0]?.meta.supersedes).toEqual(["abcd1234", "11112222"]);
		expect(parsed[0]?.meta.pin).toBe(false);
	});

	it("preserves body text exactly in mixed-format files (does not clobber legacy entries)", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/mixed.worklog.md`;
		const legacy =
			"## Entry — 2026-10-05T00:00:00.000Z (turn 1)\n\n## Legacy body text\n- line 1\n- line 2\n\n";
		const structured = entryWithMeta("## Structured body\n\n```ts\ncode();\n```", 2, "2026-10-05T00:00:02.000Z", {
			pin: true,
		});
		await writeWorklog(filePath, `${legacy}${structured.text}\n\n`);

		const result = await updateWorklogEntryPin(filePath, structured.id, false);
		expect(result).toBe(true);
		const disk = await readFile(filePath, "utf-8");
		const parsed = parseWorklogEntries(disk);
		expect(parsed).toHaveLength(2);
		expect(parsed[0]?.meta).toEqual({});
		expect(parsed[0]?.body).toBe("## Legacy body text\n- line 1\n- line 2");
		expect(parsed[1]?.meta.pin).toBe(false);
		expect(parsed[1]?.body).toBe("## Structured body\n\n```ts\ncode();\n```");
	});

	it("leaves no .tmp file after a successful write", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/atomic.worklog.md`;
		const p = entryWithMeta("## body", 1, "2026-10-06T00:00:01.000Z", { pin: true });
		await writeWorklog(filePath, `${p.text}\n\n`);
		await updateWorklogEntryPin(filePath, p.id, false);
		// The rename-over pattern means there should never be a lingering
		// .tmp file after completion.
		const { existsSync } = await import("node:fs");
		expect(existsSync(`${filePath}.tmp`)).toBe(false);
		expect(existsSync(filePath)).toBe(true);
	});

	it("no-op when file does not exist: returns false", async () => {
		const dir = createTempDir("pi-relay-worklog-pin-");
		tempDirs.push(dir);
		const filePath = `${dir}/nope.worklog.md`;
		const result = await updateWorklogEntryPin(filePath, "abcd1234", false);
		expect(result).toBe(false);
	});
});

describe("pin cap enforcement", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	async function writeWorklog(filePath: string, body: string): Promise<void> {
		const { writeFile, mkdir } = await import("node:fs/promises");
		const { dirname } = await import("node:path");
		await mkdir(dirname(filePath), { recursive: true });
		await writeFile(filePath, body, "utf-8");
	}

	function entryWithMeta(
		content: string,
		turn: number,
		iso: string,
		meta: { topics?: string[]; supersedes?: string[]; pin?: boolean } = {},
	): { text: string; id: string } {
		const text = formatWorklogEntry(content, turn, { iso, ...meta });
		const parsed = parseWorklogEntries(text);
		const id = parsed[0]?.id;
		if (!id) throw new Error("expected entry_id on structured entry");
		return { text, id };
	}

	function createPinToolCallAssistant(
		args: Record<string, unknown>,
	) {
		return {
			role: "assistant" as const,
			content: [
				{
					type: "toolCall" as const,
					id: "worklog-call",
					name: "worklog_update",
					arguments: args,
				},
			],
			stopReason: "toolUse" as const,
			timestamp: Date.now(),
		};
	}

	async function setupForkWithPins(
		sessionDir: string,
		numExistingPins: number,
		forkArgs: Record<string, unknown>,
	) {
		// Seed the child's worklog with N pinned entries before the fork runs.
		const root = new FakeSession("root-session", { sessionDir });
		const streamFn = vi.fn(async () => ({
			result: async () => createPinToolCallAssistant(forkArgs),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });
		const record = orchestrator.getRecord(childId);
		const pinIds: string[] = [];
		const seedChunks: string[] = [];
		for (let i = 0; i < numExistingPins; i++) {
			const p = entryWithMeta(
				`## existing-pin-${i}`,
				i + 1,
				`2026-11-01T00:00:${String(i).padStart(2, "0")}.000Z`,
				{ pin: true },
			);
			pinIds.push(p.id);
			seedChunks.push(p.text);
		}
		if (seedChunks.length > 0) {
			await writeWorklog(record.worklogFile, `${seedChunks.join("\n\n")}\n\n`);
		}
		return { orchestrator, childId, child, streamFn, pinIds, record };
	}

	it("writes the 1st through 20th pinned entries without replacesPinnedId", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-cap-");
		tempDirs.push(sessionDir);
		// Seed with 19 pins. The fork emits a 20th with pin:true and no
		// replacesPinnedId — should be accepted (count goes to 20).
		const { childId, child, streamFn, record } = await setupForkWithPins(sessionDir, 19, {
			content: "## new-pin-20",
			pin: true,
		});
		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect((child as unknown as FakeSession).backgroundUsageCalls.length >= 0).toBe(true);
		});
		// Wait for worklog write to settle.
		await vi.waitFor(() => {
			expect(record.lastWorklogTurn).toBe(1);
		});
		const disk = await readFile(record.worklogFile, "utf-8");
		const parsed = parseWorklogEntries(disk);
		const livePinCount = parsed.filter((entry) => entry.meta.pin === true).length;
		expect(livePinCount).toBe(20);
		expect(parsed[parsed.length - 1]?.body).toBe("## new-pin-20");
		expect(parsed[parsed.length - 1]?.meta.pin).toBe(true);
		void childId;
	});

	it("at the cap (20 pins), rejects a new pin WITHOUT replacesPinnedId; does not emit an entry", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-cap-");
		tempDirs.push(sessionDir);
		const { child, streamFn, record } = await setupForkWithPins(sessionDir, 20, {
			content: "## overflow-pin",
			pin: true,
		});
		const beforeLen = (await readFile(record.worklogFile, "utf-8")).length;

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		// Give the fork a moment to run its rejection logic.
		await waitForMicrotasks();
		await waitForMicrotasks();

		const afterLen = (await readFile(record.worklogFile, "utf-8")).length;
		expect(afterLen).toBe(beforeLen);
		// Cursor NOT advanced — a rejected write should be retryable on the
		// next substantive turn once the model includes a valid replacement id.
		expect(record.lastWorklogTurn).toBe(0);
	});

	it("at the cap, accepts a new pin WITH a valid replacesPinnedId; new pin appended; replaced pin flipped to false", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-cap-");
		tempDirs.push(sessionDir);
		// Precompute the pinned entry ids the way setupForkWithPins does so we
		// can pass a valid replacesPinnedId into the fork's tool call. The
		// entryWithMeta helper produces the same id given the same (content,
		// iso) pair.
		const targetIso = `2026-11-01T00:00:${String(5).padStart(2, "0")}.000Z`;
		const targetId = entryWithMeta(`## existing-pin-${5}`, 6, targetIso, { pin: true }).id;
		const { child, streamFn, pinIds, record } = await setupForkWithPins(sessionDir, 20, {
			content: "## replacement-pin",
			pin: true,
			replacesPinnedId: targetId,
		});
		expect(pinIds[5]).toBe(targetId);

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(record.lastWorklogTurn).toBe(1);
		});

		const parsed = parseWorklogEntries(await readFile(record.worklogFile, "utf-8"));
		// The displaced pin is now unpinned.
		const displaced = parsed.find((entry) => entry.id === targetId);
		expect(displaced?.meta.pin).toBe(false);
		// Live pin count stays at 20.
		const liveCount = parsed.filter((entry) => entry.meta.pin === true).length;
		expect(liveCount).toBe(20);
		// The new entry is present and pinned.
		const newEntry = parsed.find((entry) => entry.body === "## replacement-pin");
		expect(newEntry?.meta.pin).toBe(true);
	});

	it("at the cap, rejects a new pin with an invalid replacesPinnedId (not an existing pinned entry)", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-cap-");
		tempDirs.push(sessionDir);
		const { child, streamFn, record } = await setupForkWithPins(sessionDir, 20, {
			content: "## bad-replacement",
			pin: true,
			replacesPinnedId: "ffffffff", // not a real pinned id
		});
		const beforeLen = (await readFile(record.worklogFile, "utf-8")).length;

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await waitForMicrotasks();
		await waitForMicrotasks();

		const afterLen = (await readFile(record.worklogFile, "utf-8")).length;
		expect(afterLen).toBe(beforeLen);
		expect(record.lastWorklogTurn).toBe(0);
	});

	it("tombstoned pins don't count toward the cap: 20 pins + 1 tombstoned pin → a new pin writes without replacesPinnedId", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-cap-");
		tempDirs.push(sessionDir);
		// Seed with 19 live pins + 1 pinned entry that is tombstoned (its id
		// appears in another entry's `supersedes`). The tombstoned pin does
		// NOT count toward the cap, so the incoming 20th pin should succeed
		// without replacesPinnedId.
		const root = new FakeSession("root-session", { sessionDir });
		const streamFn = vi.fn(async () => ({
			result: async () =>
				createPinToolCallAssistant({
					content: "## fresh-pin",
					pin: true,
				}),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });
		const record = orchestrator.getRecord(childId);

		const pinEntries = Array.from({ length: 19 }, (_, i) =>
			entryWithMeta(
				`## live-pin-${i}`,
				i + 1,
				`2026-12-01T00:00:${String(i).padStart(2, "0")}.000Z`,
				{ pin: true },
			),
		);
		const tombstonedPin = entryWithMeta(
			"## tombstoned-pin",
			20,
			"2026-12-01T00:00:30.000Z",
			{ pin: true },
		);
		const superseder = entryWithMeta(
			"## superseder",
			21,
			"2026-12-01T00:00:31.000Z",
			{ supersedes: [tombstonedPin.id] },
		);
		const chunks = [
			...pinEntries.map((e) => e.text),
			tombstonedPin.text,
			superseder.text,
		];
		await writeWorklog(record.worklogFile, `${chunks.join("\n\n")}\n\n`);

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(record.lastWorklogTurn).toBe(1);
		});
		const parsed = parseWorklogEntries(await readFile(record.worklogFile, "utf-8"));
		const newEntry = parsed.find((entry) => entry.body === "## fresh-pin");
		expect(newEntry?.meta.pin).toBe(true);
	});
});

describe("worklog_unpin tool", () => {
	const tempDirs: string[] = [];
	afterEach(() => {
		for (const dir of tempDirs.splice(0)) {
			cleanupTempDir(dir);
		}
	});

	async function writeWorklog(filePath: string, body: string): Promise<void> {
		const { writeFile, mkdir } = await import("node:fs/promises");
		const { dirname } = await import("node:path");
		await mkdir(dirname(filePath), { recursive: true });
		await writeFile(filePath, body, "utf-8");
	}

	function entryWithMeta(
		content: string,
		turn: number,
		iso: string,
		meta: { topics?: string[]; supersedes?: string[]; pin?: boolean } = {},
	): { text: string; id: string } {
		const text = formatWorklogEntry(content, turn, { iso, ...meta });
		const parsed = parseWorklogEntries(text);
		const id = parsed[0]?.id;
		if (!id) throw new Error("expected entry_id on structured entry");
		return { text, id };
	}

	function createUnpinToolCallAssistant(entry_id: string) {
		return {
			role: "assistant" as const,
			content: [
				{
					type: "toolCall" as const,
					id: "unpin-call",
					name: "worklog_unpin",
					arguments: { entry_id },
				},
			],
			stopReason: "toolUse" as const,
			timestamp: Date.now(),
		};
	}

	it("fork calls worklog_unpin on a pinned entry: pin flips to false; lastWorklogMessageCount advances", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-unpin-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		// Seed with two pinned entries.
		const p1 = entryWithMeta("## keep-pinned", 1, "2026-10-10T00:00:01.000Z", { pin: true });
		const p2 = entryWithMeta("## target-for-unpin", 2, "2026-10-10T00:00:02.000Z", { pin: true });
		const streamFn = vi.fn(async () => ({
			result: async () => createUnpinToolCallAssistant(p2.id),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });
		const record = orchestrator.getRecord(childId);
		await writeWorklog(record.worklogFile, `${p1.text}\n\n${p2.text}\n\n`);

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(record.lastWorklogTurn).toBe(1);
		});

		const parsed = parseWorklogEntries(await readFile(record.worklogFile, "utf-8"));
		const target = parsed.find((entry) => entry.id === p2.id);
		const kept = parsed.find((entry) => entry.id === p1.id);
		expect(target?.meta.pin).toBe(false);
		expect(kept?.meta.pin).toBe(true);
		// Cursor advances: next fork starts from after this turn.
		expect(record.lastWorklogMessageCount).toBe(child.agent.state.messages.length);
	});

	it("fork calls worklog_unpin on an unknown id: no-op on disk; cursor still advances to prevent retry storms", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-unpin-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const p = entryWithMeta("## keep-pinned", 1, "2026-10-11T00:00:01.000Z", { pin: true });
		const streamFn = vi.fn(async () => ({
			result: async () => createUnpinToolCallAssistant("deadbeef"),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });
		const record = orchestrator.getRecord(childId);
		await writeWorklog(record.worklogFile, `${p.text}\n\n`);
		const before = await readFile(record.worklogFile, "utf-8");

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(record.lastWorklogTurn).toBe(1);
		});

		const after = await readFile(record.worklogFile, "utf-8");
		expect(after).toBe(before);
		expect(record.lastWorklogMessageCount).toBe(child.agent.state.messages.length);
	});

	it("fork emits BOTH worklog_update and worklog_unpin: first tool call wins; second is logged and ignored", async () => {
		const sessionDir = createTempDir("pi-relay-worklog-unpin-");
		tempDirs.push(sessionDir);
		const root = new FakeSession("root-session", { sessionDir });
		const p = entryWithMeta("## existing-pin", 1, "2026-10-12T00:00:01.000Z", { pin: true });
		// Emit worklog_update FIRST, then worklog_unpin as a second tool
		// call. The update path should take effect; the unpin should be
		// logged and ignored (pin should remain true after the turn).
		const streamFn = vi.fn(async () => ({
			result: async () => ({
				role: "assistant" as const,
				content: [
					{
						type: "toolCall" as const,
						id: "call-1",
						name: "worklog_update",
						arguments: { content: "## fresh-entry" },
					},
					{
						type: "toolCall" as const,
						id: "call-2",
						name: "worklog_unpin",
						arguments: { entry_id: p.id },
					},
				],
				stopReason: "toolUse" as const,
				timestamp: Date.now(),
			}),
		}) as never);
		const child = new FakeSession("child-session", {
			sessionDir,
			messages: [
				{ role: "user", content: [{ type: "text", text: "q" }], timestamp: Date.now() },
				{ role: "assistant", content: [{ type: "text", text: "a" }], stopReason: "stop", timestamp: Date.now() },
			],
			streamFn,
		});
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(async () => ({ session: child })),
		});
		const childId = await orchestrator.spawnAgent("root", { role: "explore", prompt: "inspect" });
		const record = orchestrator.getRecord(childId);
		await writeWorklog(record.worklogFile, `${p.text}\n\n`);

		child.emit({ type: "turn_end", messages: [] });
		await vi.waitFor(() => {
			expect(streamFn).toHaveBeenCalledTimes(1);
		});
		await vi.waitFor(() => {
			expect(record.lastWorklogTurn).toBe(1);
		});

		const parsed = parseWorklogEntries(await readFile(record.worklogFile, "utf-8"));
		// The pinned entry's pin remains TRUE — the update path ran, not the unpin.
		const pinned = parsed.find((entry) => entry.id === p.id);
		expect(pinned?.meta.pin).toBe(true);
		// The new entry was appended (update path).
		expect(parsed.find((entry) => entry.body === "## fresh-entry")).toBeDefined();
	});
});
