import type { AssistantMessage, Model, Usage } from "@pi-relay/ai";
import { beforeEach, describe, expect, it, vi } from "vitest";
import { generateBranchSummary } from "../src/core/compaction/index.js";
import type { SessionEntry } from "../src/core/session-manager.js";

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
		id: "branch-model",
		name: "Branch Model",
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
		model: "branch-model",
		usage,
		stopReason: "stop",
		timestamp: Date.now(),
	};
}

function createEntries(): SessionEntry[] {
	return [
		{
			type: "message",
			id: "e1",
			parentId: null,
			timestamp: new Date().toISOString(),
			message: { role: "user", content: "Branch A question", timestamp: 1 },
		},
		{
			type: "message",
			id: "e2",
			parentId: "e1",
			timestamp: new Date().toISOString(),
			message: {
				role: "assistant",
				api: "anthropic-messages",
				provider: "anthropic",
				model: "branch-model",
				content: [{ type: "text", text: "branch A answer" }],
				usage: {
					input: 10,
					output: 5,
					cacheRead: 0,
					cacheWrite: 0,
					totalTokens: 15,
					cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				},
				stopReason: "stop",
				timestamp: 2,
			},
		},
	];
}

describe("generateBranchSummary onUsage callback", () => {
	beforeEach(() => {
		completeSimpleMock.mockReset();
	});

	it("invokes onUsage with the summary's assistant usage on success", async () => {
		const summaryUsage: Usage = {
			input: 800,
			output: 150,
			cacheRead: 5000,
			cacheWrite: 200,
			totalTokens: 6150,
			cost: { input: 0.03, output: 0.01, cacheRead: 0.001, cacheWrite: 0.0005, total: 0.0415 },
		};
		completeSimpleMock.mockResolvedValue(createResponseWithUsage("## Branch A summary", summaryUsage));

		const captured: Usage[] = [];
		const result = await generateBranchSummary(createEntries(), {
			model: createModel(),
			apiKey: "test-key",
			signal: new AbortController().signal,
			onUsage: (usage) => captured.push(usage),
		});

		expect(result.summary).toBeTruthy();
		expect(captured).toHaveLength(1);
		expect(captured[0]).toEqual(summaryUsage);
	});

	it("does not invoke onUsage when the summary is aborted", async () => {
		const abortedResponse: AssistantMessage = {
			role: "assistant",
			api: "anthropic-messages",
			provider: "anthropic",
			model: "branch-model",
			content: [],
			usage: {
				input: 100,
				output: 0,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 100,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			},
			stopReason: "aborted",
			timestamp: Date.now(),
		};
		completeSimpleMock.mockResolvedValue(abortedResponse);

		const captured: Usage[] = [];
		const result = await generateBranchSummary(createEntries(), {
			model: createModel(),
			apiKey: "test-key",
			signal: new AbortController().signal,
			onUsage: (usage) => captured.push(usage),
		});

		expect(result.aborted).toBe(true);
		expect(captured).toHaveLength(0);
	});

	it("is optional — omitting onUsage doesn't throw", async () => {
		completeSimpleMock.mockResolvedValue(
			createResponseWithUsage("summary", {
				input: 10,
				output: 5,
				cacheRead: 0,
				cacheWrite: 0,
				totalTokens: 15,
				cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
			}),
		);

		const result = await generateBranchSummary(createEntries(), {
			model: createModel(),
			apiKey: "test-key",
			signal: new AbortController().signal,
		});

		expect(result.summary).toBeTruthy();
	});
});
