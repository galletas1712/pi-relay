import type { AgentMessage } from "@pi-relay/agent-core";
import type { AssistantMessage, Model, Usage } from "@pi-relay/ai";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { compact, generateSummary } from "../src/core/compaction/index.js";
import { createFileOps } from "../src/core/compaction/utils.js";
import type { BackgroundUsageScope } from "../src/core/agent-session.js";

const { completeSimpleMock } = vi.hoisted(() => ({
	completeSimpleMock: vi.fn(),
}));

vi.mock("@pi-relay/ai", async (importOriginal) => {
	const actual = await importOriginal<typeof import("@pi-relay/ai")>();
	return {
		...actual,
		completeSimple: completeSimpleMock,
	};
});

function createModel(): Model<"anthropic-messages"> {
	return {
		id: "test-compaction-model",
		name: "Test Compaction Model",
		api: "anthropic-messages",
		provider: "anthropic",
		baseUrl: "https://api.anthropic.com",
		reasoning: false,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 200_000,
		maxTokens: 8_192,
	};
}

function createResponseWithUsage(text: string, usage: Usage): AssistantMessage {
	return {
		role: "assistant",
		content: [{ type: "text", text }],
		api: "anthropic-messages",
		provider: "anthropic",
		model: "test-compaction-model",
		usage,
		stopReason: "stop",
		timestamp: Date.now(),
	};
}

const messages: AgentMessage[] = [{ role: "user", content: "Summarize this.", timestamp: Date.now() }];

describe("compaction onUsage callback", () => {
	beforeEach(() => {
		completeSimpleMock.mockReset();
	});

	it("invokes the callback with the summary's assistant usage and scope 'compaction'", async () => {
		const summaryUsage: Usage = {
			input: 1000,
			output: 250,
			cacheRead: 20_000,
			cacheWrite: 500,
			totalTokens: 21_750,
			cost: { input: 0.05, output: 0.02, cacheRead: 0.002, cacheWrite: 0.001, total: 0.073 },
		};
		completeSimpleMock.mockResolvedValue(createResponseWithUsage("summary text", summaryUsage));

		const captured: Array<{ usage: Usage; scope: "compaction" | "turn-prefix" }> = [];
		await generateSummary(
			messages,
			createModel(),
			2000,
			"test-key",
			undefined,
			undefined,
			undefined,
			undefined,
			(usage, scope) => captured.push({ usage, scope }),
		);

		expect(captured).toHaveLength(1);
		expect(captured[0]).toEqual({ usage: summaryUsage, scope: "compaction" });
	});

	it("does not crash when callback is omitted", async () => {
		const summaryUsage: Usage = {
			input: 10,
			output: 5,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 15,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		};
		completeSimpleMock.mockResolvedValue(createResponseWithUsage("ok", summaryUsage));

		// Call with no onUsage argument — should not throw.
		const result = await generateSummary(messages, createModel(), 2000, "test-key");
		expect(result).toBe("ok");
	});

	it("skips callback when response has no usage field", async () => {
		// Some providers / edge cases (aborted streams) may surface a response
		// without a populated usage. The callback must not fire with undefined.
		const response: AssistantMessage = {
			role: "assistant",
			content: [{ type: "text", text: "summary" }],
			api: "anthropic-messages",
			provider: "anthropic",
			model: "test-compaction-model",
			usage: undefined as unknown as Usage, // simulate missing usage
			stopReason: "stop",
			timestamp: Date.now(),
		};
		completeSimpleMock.mockResolvedValue(response);

		const captured: Array<{ usage: Usage; scope: string }> = [];
		await generateSummary(
			messages,
			createModel(),
			2000,
			"test-key",
			undefined,
			undefined,
			undefined,
			undefined,
			(usage, scope) => captured.push({ usage, scope }),
		);

		expect(captured).toHaveLength(0);
	});

	it("forwards the onUsage callback through compact() for single-turn compaction", async () => {
		const summaryUsage: Usage = {
			input: 500,
			output: 80,
			cacheRead: 0,
			cacheWrite: 100,
			totalTokens: 680,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		};
		completeSimpleMock.mockResolvedValue(createResponseWithUsage("## Goal\nOk.", summaryUsage));

		const preparation: Parameters<typeof compact>[0] = {
			firstKeptEntryId: "entry-keep-1",
			messagesToSummarize: [
				{ role: "user", content: "hi", timestamp: 1 },
				{ role: "user", content: "more", timestamp: 2 },
			],
			turnPrefixMessages: [],
			isSplitTurn: false,
			tokensBefore: 1234,
			previousSummary: undefined,
			fileOps: createFileOps(),
			settings: { enabled: true, reserveTokens: 2000, keepRecentTokens: 20_000 },
		};

		const captured: Array<{ usage: Usage; scope: BackgroundUsageScope }> = [];
		await compact(
			preparation,
			createModel(),
			"test-key",
			undefined,
			undefined,
			undefined,
			(usage, scope) => captured.push({ usage, scope }),
		);

		expect(captured).toHaveLength(1);
		expect(captured[0]).toEqual({ usage: summaryUsage, scope: "compaction" });
	});

	it("invokes onUsage twice on split-turn compaction, once per parallel summary", async () => {
		const compactionUsage: Usage = {
			input: 600,
			output: 90,
			cacheRead: 10,
			cacheWrite: 0,
			totalTokens: 700,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		};
		const turnPrefixUsage: Usage = {
			input: 200,
			output: 40,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 240,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		};
		// The parallel summaries each invoke completeSimple once. Return the
		// compaction response first and the turn-prefix response second.
		completeSimpleMock
			.mockResolvedValueOnce(createResponseWithUsage("history", compactionUsage))
			.mockResolvedValueOnce(createResponseWithUsage("turn prefix", turnPrefixUsage));

		const preparation: Parameters<typeof compact>[0] = {
			firstKeptEntryId: "entry-keep-split",
			messagesToSummarize: [{ role: "user", content: "history message", timestamp: 1 }],
			turnPrefixMessages: [{ role: "user", content: "turn prefix message", timestamp: 2 }],
			isSplitTurn: true,
			tokensBefore: 2222,
			previousSummary: undefined,
			fileOps: createFileOps(),
			settings: { enabled: true, reserveTokens: 2000, keepRecentTokens: 20_000 },
		};

		const captured: Array<{ usage: Usage; scope: BackgroundUsageScope }> = [];
		await compact(
			preparation,
			createModel(),
			"test-key",
			undefined,
			undefined,
			undefined,
			(usage, scope) => captured.push({ usage, scope }),
		);

		// Order isn't strictly guaranteed because the two completeSimple calls
		// run via Promise.all, but we mock them in sequence with
		// mockResolvedValueOnce so callers see the same order.
		const scopes = captured.map((c) => c.scope);
		expect(scopes.sort()).toEqual(["compaction", "turn-prefix"]);
		const byScope = new Map(captured.map((c) => [c.scope, c.usage]));
		expect(byScope.get("compaction")).toEqual(compactionUsage);
		expect(byScope.get("turn-prefix")).toEqual(turnPrefixUsage);
	});

});
