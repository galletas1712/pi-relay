import { describe, expect, it } from "vitest";
import {
	DEFAULT_PROVIDER,
	MODEL_OPTIONS,
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

	it("offers Sol max reasoning while keeping Terra/Luna on the common OpenAI set", () => {
		expect(reasoningEffortsForProvider({ kind: "openai", model: "gpt-5.6-sol" })).toContain("max");
		expect(reasoningEffortsForProvider({ kind: "openai", model: "gpt-5.6-terra" })).not.toContain("max");
		expect(reasoningEffortsForProvider({ kind: "openai", model: "gpt-5.6-luna" })).not.toContain("max");
	});
});
