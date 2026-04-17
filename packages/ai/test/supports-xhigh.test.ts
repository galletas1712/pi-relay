import { describe, expect, it } from "vitest";
import { getModel, getThinkingLevels } from "../src/models.js";

describe("getThinkingLevels", () => {
	it("falls back to xhigh for Anthropic models before capability hydration", () => {
		const model = getModel("anthropic", "claude-opus-4-6");
		expect(model).toBeDefined();
		expect(getThinkingLevels(model!)).toContain("xhigh");
	});

	it("surfaces xhigh for Anthropic Opus 4.7 on anthropic-messages API", () => {
		const model = getModel("anthropic", "claude-opus-4-7");
		expect(model).toBeDefined();
		expect(getThinkingLevels(model!)).toContain("xhigh");
	});

	it("uses the same Anthropic fallback for non-Opus models before capability hydration", () => {
		const model = getModel("anthropic", "claude-sonnet-4-5");
		expect(model).toBeDefined();
		expect(getThinkingLevels(model!)).toContain("xhigh");
	});

	it("surfaces xhigh for GPT-5.4 models", () => {
		const model = getModel("openai-codex", "gpt-5.4");
		expect(model).toBeDefined();
		expect(getThinkingLevels(model!)).toContain("xhigh");
	});

	it("does not expose Anthropic max through OpenRouter Opus 4.6", () => {
		const model = getModel("openrouter", "anthropic/claude-opus-4.6");
		expect(model).toBeDefined();
		expect(getThinkingLevels(model!)).not.toContain("max");
	});
});
