import { describe, expect, it } from "vitest";
import {
	DEFAULT_PROVIDER,
	MODEL_OPTIONS,
	newSessionCompactionConfig,
	providerFromModelKey,
	reasoningEffortsForProvider,
} from "./sessionDefaults.ts";

describe("session defaults", () => {
	it("uses gpt-5.6-sol as the default OpenAI/Codex model", () => {
		expect(DEFAULT_PROVIDER).toMatchObject({
			kind: "openai",
			model: "gpt-5.6-sol",
			reasoning_effort: "xhigh",
		});
	});

	it("exposes the picker Claude models and a Fable ZDR warning", () => {
		const claude = MODEL_OPTIONS.filter((option) => option.provider.kind === "claude");
		expect(claude.map((option) => option.provider.model)).toEqual([
			"claude-opus-4-8",
			"claude-fable-5",
		]);
		const fable = claude.find((option) => option.provider.model === "claude-fable-5");
		expect(fable?.label).toBe("Claude Fable 5");
		expect(fable?.description).toBe("Explicit opt-in: not ZDR.");
		expect(fable?.provider.reasoning_effort).toBe("high");
		expect(`${fable?.label} ${fable?.description}`).not.toMatch(/30[- ]day|data retention/i);
		expect(`${fable?.label} ${fable?.description}`).toMatch(/not ZDR/i);
	});

	it("exposes the seeded OpenAI/Codex model picker options", () => {
		expect(MODEL_OPTIONS.filter((option) => option.provider.kind === "openai").map((option) => option.provider.model)).toEqual([
			"gpt-5.6-sol",
			"gpt-5.6-terra",
			"gpt-5.6-luna",
		]);
	});

	it("maps OpenAI/Codex model keys to provider config", () => {
		expect(providerFromModelKey("openai:gpt-5.6-terra", DEFAULT_PROVIDER)).toMatchObject({
			kind: "openai",
			model: "gpt-5.6-terra",
			reasoning_effort: "xhigh",
		});
	});

	it("offers max reasoning for all hosted GPT-5.6 models but not older models", () => {
		for (const model of ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]) {
			expect(reasoningEffortsForProvider({ kind: "openai", model })).toContain("max");
		}
		expect(reasoningEffortsForProvider({ kind: "openai", model: "gpt-5.5" })).not.toContain("max");
	});

	it("uses provider-independent native compaction scheduler defaults", () => {
		expect(newSessionCompactionConfig()).toEqual({
			auto_enabled: true,
			max_consecutive_failures: 3,
		});
	});
});
