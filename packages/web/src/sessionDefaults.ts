import type { ContentBlock, ProviderConfig, ReasoningEffort } from "./types.ts";

export interface ModelOption {
	id: string;
	label: string;
	description?: string;
	provider: ProviderConfig;
}

const HOSTED_GPT56_MODELS = [
	{ model: "gpt-5.6-sol", label: "OpenAI GPT-5.6 Sol" },
	{ model: "gpt-5.6-terra", label: "OpenAI GPT-5.6 Terra" },
	{ model: "gpt-5.6-luna", label: "OpenAI GPT-5.6 Luna" }
] as const;

export const MODEL_OPTIONS: ModelOption[] = [
	...HOSTED_GPT56_MODELS.map(({ model, label }) => ({
		id: `openai:${model}`,
		label,
		provider: { kind: "openai" as const, model, reasoning_effort: "xhigh" as const }
	})),
	{
		id: "claude:claude-opus-4-8",
		label: "Claude Opus 4.8",
		provider: { kind: "claude", model: "claude-opus-4-8", reasoning_effort: "xhigh" }
	},
	{
		id: "claude:claude-fable-5",
		label: "Claude Fable 5",
		description: "Explicit opt-in: not ZDR.",
		provider: { kind: "claude", model: "claude-fable-5", reasoning_effort: "high" }
	}
];

export const OPENAI_REASONING_EFFORTS: ReasoningEffort[] = ["none", "minimal", "low", "medium", "high", "xhigh"];
export const OPENAI_GPT56_REASONING_EFFORTS: ReasoningEffort[] = [...OPENAI_REASONING_EFFORTS, "max"];
export const CLAUDE_REASONING_EFFORTS: ReasoningEffort[] = ["low", "medium", "high", "xhigh", "max"];

export const DEFAULT_PROVIDER: ProviderConfig = {
	kind: "openai",
	model: "gpt-5.6-sol",
	reasoning_effort: "xhigh"
};

export function textContent(text: string): ContentBlock[] {
	return [{ type: "text", text }];
}

export function providerModelKey(provider: ProviderConfig): string {
	return `${provider.kind}:${provider.model}`;
}

export function providerReasoningEffort(provider: ProviderConfig): ReasoningEffort {
	return provider.reasoning_effort ?? "xhigh";
}

export function withReasoningEffort(provider: ProviderConfig, reasoningEffort: ReasoningEffort): ProviderConfig {
	return { ...provider, reasoning_effort: reasoningEffort };
}

export function providerFromModelKey(modelKey: string, current: ProviderConfig): ProviderConfig {
	const option = MODEL_OPTIONS.find((candidate) => candidate.id === modelKey);
	if (!option) return current;
	return { ...current, ...option.provider };
}

export function reasoningEffortsForProvider(provider: ProviderConfig): ReasoningEffort[] {
	if (provider.kind === "claude") return CLAUDE_REASONING_EFFORTS;
	return HOSTED_GPT56_MODELS.some(({ model }) => model === provider.model)
		? OPENAI_GPT56_REASONING_EFFORTS
		: OPENAI_REASONING_EFFORTS;
}

export function newSessionCompactionConfig() {
	return {
		auto_enabled: true,
		max_consecutive_failures: 3,
	};
}
