import { Agent } from "@pi-relay/agent-core";
import { type AssistantMessage, getModel, type Usage } from "@pi-relay/ai";
import { describe, expect, it } from "vitest";
import { AgentSession, createEmptyUsage } from "../src/core/agent-session.js";
import { AuthStorage } from "../src/core/auth-storage.js";
import { ModelRegistry } from "../src/core/model-registry.js";
import { SessionManager } from "../src/core/session-manager.js";
import { SettingsManager } from "../src/core/settings-manager.js";
import { createTestResourceLoader } from "./utilities.js";

const model = getModel("anthropic", "claude-sonnet-4-5")!;

function createUsage(totalTokens: number): Usage {
	return {
		input: totalTokens,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens,
		cost: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			total: 0,
		},
	};
}

function createAssistantMessage(text: string, totalTokens: number, timestamp: number): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		api: model.api,
		provider: model.provider,
		model: model.id,
		usage: createUsage(totalTokens),
		stopReason: "stop",
		timestamp,
	};
}

function createUserMessage(text: string, timestamp: number) {
	return {
		role: "user" as const,
		content: text,
		timestamp,
	};
}

function createSession() {
	const settingsManager = SettingsManager.inMemory();
	const sessionManager = SessionManager.inMemory();
	const authStorage = AuthStorage.inMemory();
	authStorage.setRuntimeApiKey("anthropic", "test-key");
	const session = new AgentSession({
		agent: new Agent({
			getApiKey: () => "test-key",
			initialState: {
				model,
				systemPrompt: "You are a helpful assistant.",
				tools: [],
				thinkingLevel: "high",
			},
		}),
		sessionManager,
		settingsManager,
		cwd: process.cwd(),
		modelRegistry: ModelRegistry.inMemory(authStorage),
		resourceLoader: createTestResourceLoader(),
	});

	return { session, sessionManager };
}

function syncAgentMessages(session: AgentSession, sessionManager: SessionManager): void {
	session.agent.state.messages = sessionManager.buildSessionContext().messages;
}

describe("AgentSession.getSessionStats", () => {
	it("exposes the current context usage alongside token totals", () => {
		const { session, sessionManager } = createSession();

		try {
			sessionManager.appendMessage(createUserMessage("hello", 1));
			sessionManager.appendMessage(createAssistantMessage("hi", 200, 2));
			syncAgentMessages(session, sessionManager);

			const stats = session.getSessionStats();
			expect(stats.contextUsage).toEqual(session.getContextUsage());
			expect(stats.contextUsage?.tokens).toBe(200);
			expect(stats.contextUsage?.contextWindow).toBe(model.contextWindow);
			expect(stats.contextUsage?.percent).toBe((200 / model.contextWindow) * 100);
		} finally {
			session.dispose();
		}
	});

	it("reports unknown current context usage immediately after compaction", () => {
		const { session, sessionManager } = createSession();

		try {
			sessionManager.appendMessage(createUserMessage("first", 1));
			sessionManager.appendMessage(createAssistantMessage("response1", 180_000, 2));
			const keptUserId = sessionManager.appendMessage(createUserMessage("second", 3));
			sessionManager.appendMessage(createAssistantMessage("response2", 195_000, 4));
			sessionManager.appendCompaction("summary", keptUserId, 195_000);
			sessionManager.appendMessage(createUserMessage("third", 5));
			syncAgentMessages(session, sessionManager);

			const stats = session.getSessionStats();
			// Token totals are cumulative over the full session file (including
			// pre-compaction assistants), so both response1 (180k) and response2
			// (195k) contribute. Context usage is separate and reports unknown
			// immediately after compaction until the next assistant response.
			expect(stats.tokens.input).toBe(180_000 + 195_000);
			expect(stats.contextUsage).toBeDefined();
			expect(stats.contextUsage?.tokens).toBeNull();
			expect(stats.contextUsage?.percent).toBeNull();
		} finally {
			session.dispose();
		}
	});

	it("uses post-compaction usage for current context instead of stale kept usage", () => {
		const { session, sessionManager } = createSession();

		try {
			sessionManager.appendMessage(createUserMessage("first", 1));
			sessionManager.appendMessage(createAssistantMessage("response1", 180_000, 2));
			const keptUserId = sessionManager.appendMessage(createUserMessage("second", 3));
			sessionManager.appendMessage(createAssistantMessage("response2", 195_000, 4));
			sessionManager.appendCompaction("summary", keptUserId, 195_000);
			sessionManager.appendMessage(createUserMessage("third", 5));
			sessionManager.appendMessage(createAssistantMessage("response3", 25_000, 6));
			syncAgentMessages(session, sessionManager);

			const stats = session.getSessionStats();
			// Cumulative across compaction: response1 (180k) + response2 (195k) +
			// response3 (25k) = 400k.
			expect(stats.tokens.input).toBe(180_000 + 195_000 + 25_000);
			expect(stats.contextUsage).toBeDefined();
			expect(stats.contextUsage?.tokens).toBe(25_000);
			expect(stats.contextUsage?.percent).toBe((25_000 / model.contextWindow) * 100);
		} finally {
			session.dispose();
		}
	});

	it("includes background usage in aggregate token/cost totals without touching message counts", () => {
		const { session, sessionManager } = createSession();

		try {
			sessionManager.appendMessage(createUserMessage("hi", 1));
			sessionManager.appendMessage(createAssistantMessage("there", 100, 2));
			syncAgentMessages(session, sessionManager);

			const baseline = session.getSessionStats();
			expect(baseline.tokens.input).toBe(100);
			expect(baseline.assistantMessages).toBe(1);

			// Background calls (worklog, compaction, etc.) don't show up in the
			// transcript, but their tokens and dollars must still be counted.
			const bg: Usage = {
				input: 50,
				output: 25,
				cacheRead: 10,
				cacheWrite: 5,
				totalTokens: 90,
				cost: { input: 0.01, output: 0.02, cacheRead: 0.001, cacheWrite: 0.003, total: 0.034 },
			};
			session.addBackgroundUsage(bg, "worklog");

			const stats = session.getSessionStats();
			expect(stats.tokens.input).toBe(100 + 50);
			expect(stats.tokens.output).toBe(25);
			expect(stats.tokens.cacheRead).toBe(10);
			expect(stats.tokens.cacheWrite).toBe(5);
			expect(stats.tokens.total).toBe(150 + 25 + 10 + 5);
			expect(stats.cost).toBeCloseTo(0.034, 6);
			// Message counts stay tied to the transcript; background usage does
			// not materialize as messages.
			expect(stats.assistantMessages).toBe(1);
			expect(stats.userMessages).toBe(1);
			expect(stats.totalMessages).toBe(2);
		} finally {
			session.dispose();
		}
	});

	it("sums multiple background usage calls monotonically", () => {
		const { session } = createSession();

		try {
			const a: Usage = {
				input: 100,
				output: 50,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 150,
				cost: { input: 0.01, output: 0.02, cacheRead: 0, cacheWrite: 0, total: 0.03 },
			};
			const b: Usage = {
				input: 200,
				output: 75,
				cacheRead: 20,
				cacheWrite: 10,
				totalTokens: 305,
				cost: { input: 0.02, output: 0.03, cacheRead: 0.001, cacheWrite: 0.005, total: 0.056 },
			};
			session.addBackgroundUsage(a, "compaction");
			session.addBackgroundUsage(b, "branch");

			const acc = session.getBackgroundUsage();
			expect(acc.input).toBe(300);
			expect(acc.output).toBe(125);
			expect(acc.cacheRead).toBe(20);
			expect(acc.cacheWrite).toBe(10);
			expect(acc.totalTokens).toBe(455);
			expect(acc.cost.total).toBeCloseTo(0.086, 6);

			const stats = session.getSessionStats();
			expect(stats.tokens.input).toBe(300);
			expect(stats.cost).toBeCloseTo(0.086, 6);
		} finally {
			session.dispose();
		}
	});

	it("preserves 5m/1h cache-write breakdown across background usage adds", () => {
		const { session } = createSession();

		try {
			const a: Usage = {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 100,
				cacheWrite5m: 60,
				cacheWrite1h: 40,
				totalTokens: 100,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0.002, total: 0.002 },
			};
			const b: Usage = {
				input: 0,
				output: 0,
				cacheRead: 0,
				cacheWrite: 50,
				cacheWrite5m: 30,
				cacheWrite1h: 20,
				totalTokens: 50,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0.001, total: 0.001 },
			};
			session.addBackgroundUsage(a);
			session.addBackgroundUsage(b);

			const acc = session.getBackgroundUsage();
			expect(acc.cacheWrite).toBe(150);
			expect(acc.cacheWrite5m).toBe(90);
			expect(acc.cacheWrite1h).toBe(60);
			// Invariant: cacheWrite === (cacheWrite5m ?? 0) + (cacheWrite1h ?? 0)
			expect(acc.cacheWrite).toBe((acc.cacheWrite5m ?? 0) + (acc.cacheWrite1h ?? 0));
		} finally {
			session.dispose();
		}
	});

	it("returns a zero-initialized Usage before any background add", () => {
		const { session } = createSession();

		try {
			const bg = session.getBackgroundUsage();
			expect(bg).toEqual(createEmptyUsage());
		} finally {
			session.dispose();
		}
	});

	it("sums assistant usage across a compaction boundary", () => {
		// Anchor test for the compaction-invariant contract: walking
		// sessionManager.getEntries() must include BOTH pre- and post-compaction
		// assistant messages. state.messages (post-compaction) would drop the
		// pre-compaction assistants and produce a silent undercount.
		const { session, sessionManager } = createSession();

		try {
			sessionManager.appendMessage(createUserMessage("q1", 1));
			sessionManager.appendMessage(createAssistantMessage("a1", 100, 2));
			sessionManager.appendMessage(createUserMessage("q2", 3));
			sessionManager.appendMessage(createAssistantMessage("a2", 200, 4));
			const keptUserId = sessionManager.appendMessage(createUserMessage("q3", 5));
			sessionManager.appendMessage(createAssistantMessage("a3", 300, 6));
			sessionManager.appendCompaction("summary", keptUserId, 600);
			sessionManager.appendMessage(createUserMessage("q4", 7));
			sessionManager.appendMessage(createAssistantMessage("a4", 50, 8));
			syncAgentMessages(session, sessionManager);

			const stats = session.getSessionStats();
			// All four assistants (100 + 200 + 300 + 50) contribute to cumulative tokens.
			expect(stats.tokens.input).toBe(100 + 200 + 300 + 50);
			expect(stats.tokens.total).toBe(100 + 200 + 300 + 50);
			expect(stats.assistantMessages).toBe(4);
			expect(stats.userMessages).toBe(4);
		} finally {
			session.dispose();
		}
	});
});
