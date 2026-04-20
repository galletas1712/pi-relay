/**
 * Regression tests for the 5m/1h cache-write cost accounting gap.
 *
 * Anthropic charges:
 *   - 5-minute cache writes: 1.25× the base input-token rate
 *   - 1-hour cache writes:     2× the base input-token rate (= 1.6× the 5m rate)
 *   - Cache reads:           0.1× the base input-token rate
 *
 * `model.cost.cacheWrite` in `packages/ai/src/models.generated.ts` encodes the
 * 5-minute rate across every Claude model we ship. Before this fix, every 1h
 * write was billed at the 5m rate — an ~37.5% underestimate on Tier-0 writes
 * now that PR #25 emits `ttl: "1h"` on the `role` / `capabilities` / `coordination`
 * block.
 *
 * The fix splits `Usage` into optional `cacheWrite5m` and `cacheWrite1h` fields
 * populated from `message_start.message.usage.cache_creation.ephemeral_*m_input_tokens`.
 * `calculateCost` multiplies 1h tokens by 1.6× the catalog rate.
 */

import { describe, expect, it, vi } from "vitest";
import { calculateCost, getModel } from "../src/models.js";
import type { Context, Usage } from "../src/types.js";

const mockState = vi.hoisted(() => ({
	messageStartUsage: undefined as Record<string, unknown> | undefined,
	messageDeltaUsage: undefined as Record<string, unknown> | undefined,
}));

vi.mock("@anthropic-ai/sdk", () => {
	const fakeStream = {
		async *[Symbol.asyncIterator]() {
			yield {
				type: "message_start",
				message: {
					id: "msg_test",
					usage: mockState.messageStartUsage,
				},
			};
			yield {
				type: "message_delta",
				delta: { stop_reason: "end_turn" },
				usage: mockState.messageDeltaUsage ?? { output_tokens: 5 },
			};
		},
		finalMessage: async () => ({ usage: { output_tokens: 5 } }),
	};

	class FakeAnthropic {
		constructor(_opts: Record<string, unknown>) {}
		messages = {
			stream: () => fakeStream,
		};
	}

	return { default: FakeAnthropic };
});

function makeUsage(overrides: Partial<Usage> = {}): Usage {
	return {
		input: 0,
		output: 0,
		cacheRead: 0,
		cacheWrite: 0,
		totalTokens: 0,
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		...overrides,
	};
}

describe("calculateCost — 5m vs 1h cache-write rate", () => {
	it("bills aggregate cacheWrite at the 5m rate when no breakdown is provided (back-compat)", () => {
		const model = getModel("anthropic", "claude-opus-4-7"); // 5m rate: $6.25/MTok
		const usage = makeUsage({ cacheWrite: 1_000_000 });
		calculateCost(model, usage);
		// 1M tokens * $6.25/MTok = $6.25
		expect(usage.cost.cacheWrite).toBeCloseTo(6.25, 5);
	});

	it("bills pure 1h writes at 1.6× the catalog rate (= Anthropic's 2× base-input billing)", () => {
		const model = getModel("anthropic", "claude-opus-4-7"); // base input: $5/MTok
		const usage = makeUsage({
			cacheWrite: 1_000_000,
			cacheWrite5m: 0,
			cacheWrite1h: 1_000_000,
		});
		calculateCost(model, usage);
		// Expected: 1M * 2.0 * ($5/MTok) = $10 (= docs' "1h Cache Writes: $10/MTok" for Opus 4.7)
		expect(usage.cost.cacheWrite).toBeCloseTo(10, 5);
	});

	it("bills mixed 5m + 1h writes at each tier's rate", () => {
		const model = getModel("anthropic", "claude-sonnet-4-5"); // 5m: $3.75/MTok, 1h: $6/MTok
		const usage = makeUsage({
			cacheWrite: 1_500_000,
			cacheWrite5m: 1_000_000,
			cacheWrite1h: 500_000,
		});
		calculateCost(model, usage);
		// 5m: 1M * $3.75 = $3.75
		// 1h: 0.5M * $6 = $3.00
		// total: $6.75
		expect(usage.cost.cacheWrite).toBeCloseTo(6.75, 5);
	});

	it("falls back to aggregate billing when only cacheWrite1h is set (derives 5m from aggregate)", () => {
		const model = getModel("anthropic", "claude-haiku-4-5"); // 5m: $1.25/MTok, 1h: $2/MTok
		const usage = makeUsage({
			cacheWrite: 1_000_000,
			cacheWrite1h: 400_000,
			// cacheWrite5m intentionally undefined
		});
		calculateCost(model, usage);
		// Derived 5m = 1M - 0.4M = 0.6M
		// 5m cost: 0.6M * $1.25 = $0.75
		// 1h cost: 0.4M * $2 = $0.80
		// total: $1.55
		expect(usage.cost.cacheWrite).toBeCloseTo(1.55, 5);
	});

	it("uses the 5m fallback path when cacheWrite1h is 0", () => {
		const model = getModel("anthropic", "claude-opus-4-7");
		const usage = makeUsage({
			cacheWrite: 1_000_000,
			cacheWrite5m: 1_000_000,
			cacheWrite1h: 0,
		});
		calculateCost(model, usage);
		// Zero 1h writes → aggregate path → 1M * $6.25 = $6.25
		expect(usage.cost.cacheWrite).toBeCloseTo(6.25, 5);
	});

	it("regression: pre-fix behavior underreported 1h writes by ~37.5%", () => {
		// Verifies the fix actually moves the numbers. Pre-fix: pure-1h cacheWrite of
		// 1M tokens on Opus 4.7 billed at $6.25 (the 5m rate). Post-fix: $10.
		const model = getModel("anthropic", "claude-opus-4-7");
		const usage = makeUsage({
			cacheWrite: 1_000_000,
			cacheWrite5m: 0,
			cacheWrite1h: 1_000_000,
		});
		calculateCost(model, usage);
		const underestimate = 1 - 6.25 / usage.cost.cacheWrite;
		expect(underestimate).toBeCloseTo(0.375, 3);
	});
});

describe("Anthropic provider — cache_creation breakdown capture", () => {
	const context: Context = {
		systemPrompt: "sys",
		messages: [{ role: "user", content: "hi", timestamp: Date.now() }],
	};

	it("captures cacheWrite5m and cacheWrite1h from message_start.cache_creation", async () => {
		mockState.messageStartUsage = {
			input_tokens: 100,
			output_tokens: 0,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 1500,
			cache_creation: {
				ephemeral_5m_input_tokens: 500,
				ephemeral_1h_input_tokens: 1000,
			},
		};
		mockState.messageDeltaUsage = { output_tokens: 10 };

		const model = getModel("anthropic", "claude-opus-4-7");
		const { streamAnthropic } = await import("../src/providers/anthropic.js");

		let finalUsage: Usage | undefined;
		const s = streamAnthropic(model, context, { apiKey: "sk-ant-api-key-test" });
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
		// Cost: 500 * $6.25/MTok + 1000 * $10/MTok = 0.003125 + 0.010 = $0.013125
		expect(finalUsage!.cost.cacheWrite).toBeCloseTo(0.013125, 6);
	});

	it("leaves breakdown undefined when cache_creation is absent (back-compat with Bedrock-shape responses)", async () => {
		mockState.messageStartUsage = {
			input_tokens: 100,
			output_tokens: 0,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 500,
			// cache_creation: null / absent
		};
		mockState.messageDeltaUsage = { output_tokens: 10 };

		const model = getModel("anthropic", "claude-opus-4-7");
		const { streamAnthropic } = await import("../src/providers/anthropic.js");

		let finalUsage: Usage | undefined;
		const s = streamAnthropic(model, context, { apiKey: "sk-ant-api-key-test" });
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
		// Cost falls back to aggregate × 5m rate: 500 * $6.25/MTok = $0.003125
		expect(finalUsage!.cost.cacheWrite).toBeCloseTo(0.003125, 6);
	});

	it("rescales 5m/1h proportionally when message_delta overrides cache_creation_input_tokens", async () => {
		mockState.messageStartUsage = {
			input_tokens: 100,
			output_tokens: 0,
			cache_read_input_tokens: 0,
			cache_creation_input_tokens: 1000,
			cache_creation: {
				ephemeral_5m_input_tokens: 400,
				ephemeral_1h_input_tokens: 600,
			},
		};
		// message_delta reports a new (higher) cache_creation total — rare but possible.
		// Scale factor = 2000 / 1000 = 2×.
		mockState.messageDeltaUsage = {
			output_tokens: 10,
			cache_creation_input_tokens: 2000,
		};

		const model = getModel("anthropic", "claude-opus-4-7");
		const { streamAnthropic } = await import("../src/providers/anthropic.js");

		let finalUsage: Usage | undefined;
		const s = streamAnthropic(model, context, { apiKey: "sk-ant-api-key-test" });
		for await (const event of s) {
			if (event.type === "done") {
				finalUsage = event.message.usage;
				break;
			}
			if (event.type === "error") break;
		}

		expect(finalUsage).toBeDefined();
		expect(finalUsage!.cacheWrite).toBe(2000);
		expect(finalUsage!.cacheWrite5m).toBe(800); // 400 * 2
		expect(finalUsage!.cacheWrite1h).toBe(1200); // 600 * 2
	});
});
