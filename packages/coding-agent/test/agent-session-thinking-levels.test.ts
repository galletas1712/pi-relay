import type { ModelCapabilities } from "@pi-relay/ai";
import { afterEach, describe, expect, it } from "vitest";
import { createHarness, type Harness } from "./suite/harness.js";

describe("AgentSession thinking levels", () => {
	const harnesses: Harness[] = [];

	afterEach(() => {
		while (harnesses.length > 0) {
			harnesses.pop()?.cleanup();
		}
	});

	it("surfaces provider-native Anthropic thinking strings directly", async () => {
		const harness = await createHarness({
			models: [
				{ id: "claude-opus-4-6", name: "Claude Opus 4.6", reasoning: true },
				{ id: "gpt-5.4", name: "GPT-5.4", reasoning: true },
			],
		});
		harnesses.push(harness);

		const anthropicModel = harness.getModel("claude-opus-4-6")!;
		anthropicModel.api = "anthropic-messages";
		anthropicModel.capabilities = {
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
		} satisfies ModelCapabilities;

		expect(harness.session.getAvailableThinkingLevels()).toEqual(["low", "medium", "high", "max"]);

		harness.session.setThinkingLevel("max");
		expect(harness.session.thinkingLevel).toBe("max");

		await harness.session.setModel(harness.getModel("gpt-5.4")!);
		expect(harness.session.thinkingLevel).toBe("high");
	});
});
