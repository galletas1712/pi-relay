import type { ContentBlock, ProviderConfig, ReasoningEffort } from "./types.ts";

export interface ModelOption {
	id: string;
	label: string;
	provider: ProviderConfig;
}

export const MODEL_OPTIONS: ModelOption[] = [
	{
		id: "openai:gpt-5.5",
		label: "OpenAI GPT-5.5",
		provider: { kind: "openai", model: "gpt-5.5", reasoning_effort: "xhigh" }
	},
	{
		id: "claude:claude-opus-4-8",
		label: "Claude Opus 4.8",
		provider: { kind: "claude", model: "claude-opus-4-8", reasoning_effort: "xhigh" }
	}
];

export const OPENAI_REASONING_EFFORTS: ReasoningEffort[] = ["none", "minimal", "low", "medium", "high", "xhigh"];
export const CLAUDE_REASONING_EFFORTS: ReasoningEffort[] = ["low", "medium", "high", "xhigh", "max"];

export const DEFAULT_PROVIDER: ProviderConfig = {
	kind: "openai",
	model: "gpt-5.5",
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
	return provider.kind === "claude" ? CLAUDE_REASONING_EFFORTS : OPENAI_REASONING_EFFORTS;
}
