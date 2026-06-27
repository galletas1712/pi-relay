import type { ContentBlock, ProviderConfig, ReasoningEffort } from "./types.ts";

export interface ModelOption {
	id: string;
	label: string;
	provider: ProviderConfig;
}

export const MODEL_OPTIONS: ModelOption[] = [
	{
		id: "openai:gpt-5.6-sol",
		label: "OpenAI GPT-5.6 Sol",
		provider: { kind: "openai", model: "gpt-5.6-sol", reasoning_effort: "xhigh" }
	},
	{
		id: "openai:gpt-5.6-terra",
		label: "OpenAI GPT-5.6 Terra",
		provider: { kind: "openai", model: "gpt-5.6-terra", reasoning_effort: "xhigh" }
	},
	{
		id: "openai:gpt-5.6-luna",
		label: "OpenAI GPT-5.6 Luna",
		provider: { kind: "openai", model: "gpt-5.6-luna", reasoning_effort: "xhigh" }
	},
	{
		id: "claude:claude-opus-4-8",
		label: "Claude Opus 4.8",
		provider: { kind: "claude", model: "claude-opus-4-8", reasoning_effort: "xhigh" }
	}
];

export const OPENAI_REASONING_EFFORTS: ReasoningEffort[] = ["none", "minimal", "low", "medium", "high", "xhigh"];
export const OPENAI_SOL_REASONING_EFFORTS: ReasoningEffort[] = [...OPENAI_REASONING_EFFORTS, "max"];
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
	return { ...option.provider };
}

export function reasoningEffortsForProvider(provider: ProviderConfig): ReasoningEffort[] {
	if (provider.kind === "claude") return CLAUDE_REASONING_EFFORTS;
	return provider.model === "gpt-5.6-sol" ? OPENAI_SOL_REASONING_EFFORTS : OPENAI_REASONING_EFFORTS;
}
