/**
 * Verifies that the Bedrock provider reads the per-TTL cache-write breakdown
 * from `ConverseStreamMetadataEvent.usage.cacheDetails[]` into `Usage.cacheWrite5m`
 * and `Usage.cacheWrite1h`, so `calculateCost` can bill 1h writes at 1.6×
 * the 5m rate (matching Anthropic's pricing on the Bedrock bridge too).
 */

import { describe, expect, it, vi } from "vitest";
import { getModel } from "../src/models.js";
import type { Context, Usage } from "../src/types.js";

const mockState = vi.hoisted(() => ({
	metadataUsage: undefined as Record<string, unknown> | undefined,
}));

vi.mock("@aws-sdk/client-bedrock-runtime", async () => {
	const actual = await vi.importActual<any>("@aws-sdk/client-bedrock-runtime");

	async function* fakeStream() {
		yield {
			messageStart: { role: "assistant" },
		};
		yield {
			contentBlockDelta: { delta: { text: "hi" }, contentBlockIndex: 0 },
		};
		yield {
			messageStop: { stopReason: "end_turn" },
		};
		yield {
			metadata: {
				usage: mockState.metadataUsage,
				metrics: { latencyMs: 0 },
			},
		};
	}

	class FakeBedrockRuntimeClient {
		send() {
			return {
				stream: fakeStream(),
			};
		}
	}

	return {
		...actual,
		BedrockRuntimeClient: FakeBedrockRuntimeClient,
	};
});

describe("Bedrock provider — cacheDetails TTL breakdown", () => {
	const context: Context = {
		systemPrompt: "sys",
		messages: [{ role: "user", content: "hi", timestamp: Date.now() }],
	};

	it("captures cacheWrite5m and cacheWrite1h from metadata.usage.cacheDetails[]", async () => {
		mockState.metadataUsage = {
			inputTokens: 100,
			outputTokens: 5,
			totalTokens: 105,
			cacheReadInputTokens: 0,
			cacheWriteInputTokens: 1500,
			cacheDetails: [
				{ ttl: "1h", inputTokens: 1000 },
				{ ttl: "5m", inputTokens: 500 },
			],
		};

		const model = getModel("amazon-bedrock", "anthropic.claude-opus-4-7");
		// Skip if the model isn't in the catalog for this worktree version.
		if (!model) return;

		const { streamBedrock } = await import("../src/providers/amazon-bedrock.js");

		let finalUsage: Usage | undefined;
		const s = streamBedrock(model, context, { region: "us-east-1" });
		for await (const event of s) {
			if (event.type === "done") {
				finalUsage = event.message.usage;
				break;
			}
			if (event.type === "error") break;
		}

		expect(finalUsage).toBeDefined();
		expect(finalUsage!.cacheWrite).toBe(1500);
		expect(finalUsage!.cacheWrite5m).toBe(500);
		expect(finalUsage!.cacheWrite1h).toBe(1000);
		// Opus 4.7 cacheWrite catalog rate = $6.25/MTok (5m). 500 * 6.25 + 1000 * 10 = $0.013125.
		expect(finalUsage!.cost.cacheWrite).toBeCloseTo(0.013125, 6);
	});

	it("leaves breakdown undefined when cacheDetails is absent (older API response shape)", async () => {
		mockState.metadataUsage = {
			inputTokens: 100,
			outputTokens: 5,
			totalTokens: 105,
			cacheReadInputTokens: 0,
			cacheWriteInputTokens: 500,
			// No cacheDetails
		};

		const model = getModel("amazon-bedrock", "anthropic.claude-opus-4-7");
		if (!model) return;

		const { streamBedrock } = await import("../src/providers/amazon-bedrock.js");

		let finalUsage: Usage | undefined;
		const s = streamBedrock(model, context, { region: "us-east-1" });
		for await (const event of s) {
			if (event.type === "done") {
				finalUsage = event.message.usage;
				break;
			}
			if (event.type === "error") break;
		}

		expect(finalUsage).toBeDefined();
		expect(finalUsage!.cacheWrite).toBe(500);
		expect(finalUsage!.cacheWrite5m).toBeUndefined();
		expect(finalUsage!.cacheWrite1h).toBeUndefined();
		// Falls back to aggregate * 5m rate: 500 * $6.25 = $0.003125
		expect(finalUsage!.cost.cacheWrite).toBeCloseTo(0.003125, 6);
	});
});
