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
		for (const option of MODEL_OPTIONS) {
			expect(option.id).toBe(providerModelKey(option.provider));
			expect(option.label).toBe(option.id);
		}
	});

	it("exposes the picker Claude models and a Fable ZDR warning", () => {
		const claude = MODEL_OPTIONS.filter((option) => option.provider.kind === "claude");
		expect(claude.map((option) => option.provider.model)).toEqual([
			"claude-opus-5",
			"claude-opus-4-8",
			"claude-fable-5",
		]);
		expect(claude[0]?.provider.reasoning_effort).toBe("high");
		const fable = claude.find((option) => option.provider.model === "claude-fable-5");
		expect(fable?.label).toBe("claude:claude-fable-5");
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

	it("resolves Claude composite model keys without leaking the picker key internally", () => {
		const provider = providerFromModelKey("claude:claude-opus-4-8", DEFAULT_PROVIDER);
		expect(provider).toMatchObject({
			kind: "claude",
			model: "claude-opus-4-8",
			reasoning_effort: "xhigh",
		});
		expect(providerModelKey(provider)).toBe("claude:claude-opus-4-8");
	});

	it("keeps the current provider unchanged for an unknown model key", () => {
		const current = { ...DEFAULT_PROVIDER, reasoning_effort: "high" as const };
		expect(providerFromModelKey("unknown:model", current)).toEqual(current);
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
