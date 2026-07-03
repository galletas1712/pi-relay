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

	it("exposes current Claude models with model-appropriate defaults and a Fable retention warning", () => {
		const claude = MODEL_OPTIONS.filter((option) => option.provider.kind === "claude");
		expect(claude.map((option) => option.provider.model)).toEqual([
			"claude-sonnet-5",
			"claude-opus-4-8",
			"claude-fable-5",
		]);
		expect(claude.find((option) => option.provider.model === "claude-sonnet-5")?.provider.reasoning_effort).toBe("high");
		const fable = claude.find((option) => option.provider.model === "claude-fable-5");
		expect(fable?.provider.reasoning_effort).toBe("high");
		expect(`${fable?.label} ${fable?.description}`).toMatch(/30-day data retention|30-day retention/i);
		expect(`${fable?.label} ${fable?.description}`).toMatch(/not ZDR|zero data retention is unavailable/i);
	});

	it("exposes exactly the supported OpenAI/Codex model options", () => {
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

	it("keeps Anthropic native compaction opt-in", () => {
		expect(newSessionCompactionConfig({ kind: "claude", model: "claude-sonnet-5" })).toEqual({
			auto_enabled: true,
			remote_mode: "never",
			anthropic_native_compaction: null,
			max_consecutive_failures: 3,
		});
		expect(newSessionCompactionConfig({ kind: "openai", model: "gpt-5.6-sol" })).toEqual({
			auto_enabled: true,
			remote_mode: "auto",
			max_consecutive_failures: 3,
		});
	});
});
