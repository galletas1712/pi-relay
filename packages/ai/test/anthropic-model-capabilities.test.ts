import { describe, expect, it, vi } from "vitest";
import { hydrateAnthropicModelsCapabilities } from "../src/anthropic-models.js";
import { getModel, getThinkingLevels } from "../src/models.js";
import type { Model, ModelCapabilities } from "../src/types.js";

const mockState = vi.hoisted(() => ({
	listCalls: 0,
	retrieveCalls: [] as string[],
}));

vi.mock("@anthropic-ai/sdk", () => {
	const fakeStream = {
		async *[Symbol.asyncIterator]() {
			yield {
				type: "message_start",
				message: {
					usage: { input_tokens: 1, output_tokens: 0 },
				},
			};
			yield {
				type: "message_delta",
				delta: { stop_reason: "end_turn" },
				usage: { output_tokens: 1 },
			};
		},
		finalMessage: async () => ({
			usage: { input_tokens: 1, output_tokens: 1, cache_creation_input_tokens: 0, cache_read_input_tokens: 0 },
		}),
	};

	class FakeAnthropic {
		messages = {
			stream: () => fakeStream,
		};

		models = {
			list: () => ({
				async *[Symbol.asyncIterator]() {
					mockState.listCalls += 1;
					yield {
						id: "claude-opus-4-6",
						created_at: "2026-02-04T00:00:00Z",
						display_name: "Claude Opus 4.6",
						max_input_tokens: 1000000,
						max_tokens: 128000,
						type: "model",
						capabilities: {
							effort: {
								supported: true,
								low: { supported: true },
								medium: { supported: true },
								high: { supported: true },
								max: { supported: true },
								xhigh: null,
							},
							thinking: {
								supported: true,
								types: {
									adaptive: { supported: true },
									enabled: { supported: false },
								},
							},
						},
					};
				},
			}),
			retrieve: async (modelID: string) => {
				mockState.retrieveCalls.push(modelID);
				return {
					id: modelID,
					created_at: "2026-02-04T00:00:00Z",
					display_name: modelID,
					max_input_tokens: 200000,
					max_tokens: 64000,
					type: "model",
					capabilities: {
						effort: {
							supported: true,
							low: { supported: true },
							medium: { supported: true },
							high: { supported: true },
							max: { supported: false },
							xhigh: { supported: true },
						},
						thinking: {
							supported: true,
							types: {
								adaptive: { supported: true },
								enabled: { supported: false },
							},
						},
					},
				};
			},
		};
	}

	return { default: FakeAnthropic };
});

describe("Anthropic model capability hydration", () => {
	it("hydrates direct Anthropic models from the Models API list", async () => {
		const model = getModel("anthropic", "claude-opus-4-6");
		expect(model).toBeDefined();

		await hydrateAnthropicModelsCapabilities([model!], { apiKey: "sk-ant-test" });

		expect(mockState.listCalls).toBe(1);
		expect(model?.contextWindow).toBe(1000000);
		expect(model?.maxTokens).toBe(128000);
		expect(getThinkingLevels(model!)).toContain("max");
	});

	it("falls back to retrieve() when a model is missing from the list response", async () => {
		const model: Model<"anthropic-messages"> = {
			...getModel("anthropic", "claude-sonnet-4-5")!,
			id: "claude-opus-4-7",
			capabilities: undefined,
		};

		await hydrateAnthropicModelsCapabilities([model], { apiKey: "sk-ant-test" });

		expect(mockState.retrieveCalls).toContain("claude-opus-4-7");
		expect(model.capabilities?.effort?.xhigh?.supported).toBe(true);
	});

	it("uses adaptive thinking plus effort metadata to surface max", () => {
		const model: Model<"anthropic-messages"> = {
			...getModel("anthropic", "claude-sonnet-4-5")!,
			id: "capability-driven-model",
			capabilities: {
				effort: {
					supported: true,
					low: { supported: true },
					medium: { supported: true },
					high: { supported: true },
					max: { supported: true },
					xhigh: null,
				},
				thinking: {
					supported: true,
					types: {
						adaptive: { supported: true },
						enabled: { supported: false },
					},
				},
			} satisfies ModelCapabilities,
		};

		expect(getThinkingLevels(model)).toContain("max");
	});

	it("injects xhigh for Opus 4.7 even when the capabilities omit it", () => {
		const model: Model<"anthropic-messages"> = {
			...getModel("anthropic", "claude-opus-4-7")!,
			capabilities: {
				effort: {
					supported: true,
					low: { supported: true },
					medium: { supported: true },
					high: { supported: true },
					max: { supported: true },
					xhigh: null,
				},
				thinking: {
					supported: true,
					types: {
						adaptive: { supported: true },
						enabled: { supported: false },
					},
				},
			} satisfies ModelCapabilities,
		};

		expect(getThinkingLevels(model)).toEqual(["low", "medium", "high", "xhigh", "max"]);
	});

	it("does not inject xhigh for other Anthropic adaptive models", () => {
		const model: Model<"anthropic-messages"> = {
			...getModel("anthropic", "claude-opus-4-6")!,
			capabilities: {
				effort: {
					supported: true,
					low: { supported: true },
					medium: { supported: true },
					high: { supported: true },
					max: { supported: true },
					xhigh: null,
				},
				thinking: {
					supported: true,
					types: {
						adaptive: { supported: true },
						enabled: { supported: false },
					},
				},
			} satisfies ModelCapabilities,
		};

		expect(getThinkingLevels(model)).not.toContain("xhigh");
	});

	it("does not expose adaptive effort levels when adaptive thinking is unavailable", () => {
		const model: Model<"anthropic-messages"> = {
			...getModel("anthropic", "claude-sonnet-4-5")!,
			id: "manual-thinking-only-model",
			capabilities: {
				effort: {
					supported: true,
					low: { supported: true },
					medium: { supported: true },
					high: { supported: true },
					max: { supported: true },
					xhigh: null,
				},
				thinking: {
					supported: true,
					types: {
						adaptive: { supported: false },
						enabled: { supported: true },
					},
				},
			} satisfies ModelCapabilities,
		};

		expect(getThinkingLevels(model)).toEqual(["minimal", "low", "medium", "high"]);
	});
});
