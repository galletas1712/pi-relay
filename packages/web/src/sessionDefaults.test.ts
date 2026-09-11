import { describe, expect, it } from "vitest";
import {
	DEFAULT_PROVIDER,
	MODEL_OPTIONS,
	newSessionCompactionConfig,
	providerFromModelKey,
	providerModelKey,
	reasoningEffortsForProvider,
} from "./sessionDefaults.ts";

describe("session defaults", () => {
	it("uses gpt-5.6-luna with high reasoning as the default OpenAI/Codex provider", () => {
		expect(DEFAULT_PROVIDER).toMatchObject({
			kind: "openai",
			model: "gpt-5.6-luna",
			reasoning_effort: "high",
		});
	});

	it("uses canonical provider:model keys as picker identities and labels", () => {
		expect(MODEL_OPTIONS.map((option) => option.id)).toEqual([
			"openai:gpt-5.6-sol",
			"openai:gpt-5.6-terra",
			"openai:gpt-5.6-luna",
			"openai:gpt-6-astra",
			"claude:claude-opus-5",
			"claude:claude-fable-5-1",
		]);
		for (const option of MODEL_OPTIONS) {
			expect(option.id).toBe(providerModelKey(option.provider));
			expect(option.label).toBe(option.id);
		}
	});

	it("exposes only the current picker Claude models and a Fable 5.1 ZDR warning", () => {
		const claude = MODEL_OPTIONS.filter((option) => option.provider.kind === "claude");
		expect(claude.map((option) => option.provider.model)).toEqual([
			"claude-opus-5",
			"claude-fable-5-1",
		]);
		expect(claude[0]?.provider.reasoning_effort).toBe("high");
		const fable = claude.find((option) => option.provider.model === "claude-fable-5-1");
		expect(fable?.label).toBe("claude:claude-fable-5-1");
		expect(fable?.description).toBe("Explicit opt-in: not ZDR.");
		expect(fable?.provider.reasoning_effort).toBe("high");
		expect(`${fable?.label} ${fable?.description}`).not.toMatch(/30[- ]day|data retention/i);
		expect(`${fable?.label} ${fable?.description}`).toMatch(/not ZDR/i);
	});

	it("maps OpenAI/Codex model keys to provider config", () => {
		expect(providerFromModelKey("openai:gpt-5.6-terra", DEFAULT_PROVIDER)).toMatchObject({
			kind: "openai",
			model: "gpt-5.6-terra",
			reasoning_effort: "xhigh",
		});
	});

	it("maps the Astra model key to its high-effort OpenAI route", () => {
		const provider = providerFromModelKey("openai:gpt-6-astra", DEFAULT_PROVIDER);
		expect(provider).toEqual({
			kind: "openai",
			model: "gpt-6-astra",
			reasoning_effort: "high",
		});
		expect(providerModelKey(provider)).toBe("openai:gpt-6-astra");
	});

	it("resolves Claude composite model keys without leaking the picker key internally", () => {
		const provider = providerFromModelKey("claude:claude-fable-5-1", DEFAULT_PROVIDER);
		expect(provider).toMatchObject({
			kind: "claude",
			model: "claude-fable-5-1",
			reasoning_effort: "high",
		});
		expect(providerModelKey(provider)).toBe("claude:claude-fable-5-1");
	});

	it("keeps the current provider unchanged for an unknown model key", () => {
		const current = { ...DEFAULT_PROVIDER, reasoning_effort: "high" as const };
		expect(providerFromModelKey("unknown:model", current)).toEqual(current);
	});

	it("preserves GPT-5.6 efforts and gives Astra exactly low through max", () => {
		for (const model of ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"]) {
			expect(reasoningEffortsForProvider({ kind: "openai", model })).toEqual([
				"none",
				"minimal",
				"low",
				"medium",
				"high",
				"xhigh",
				"max",
			]);
		}
		expect(reasoningEffortsForProvider({ kind: "openai", model: "gpt-6-astra" })).toEqual([
			"low",
			"medium",
			"high",
			"xhigh",
			"max",
		]);
		expect(reasoningEffortsForProvider({ kind: "openai", model: "gpt-5.5" })).not.toContain("max");
	});

	it("uses provider-independent native compaction scheduler defaults", () => {
		expect(newSessionCompactionConfig()).toEqual({
			auto_enabled: true,
			max_consecutive_failures: 3,
		});
	});
});
