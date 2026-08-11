import type { ContentBlock, ProviderConfig, ReasoningEffort } from "./types.ts";

export interface ModelOption {
	id: string;
	label: string;
	description?: string;
	provider: ProviderConfig;
	/** M11b (bridge profile): the exact pi "provider/modelId" string this
	 * option maps to on the bridge (session.create/setModel). Absent on the
	 * legacy daemon profile. */
	bridgeModel?: string;
	/** M11b: pi thinking levels this model advertises (models.list). */
	bridgeThinkingLevels?: string[];
}

const HOSTED_GPT56_MODELS = [
	"gpt-5.6-sol",
	"gpt-5.6-terra",
	"gpt-5.6-luna"
] as const;

export const MODEL_OPTIONS: ModelOption[] = [
	...HOSTED_GPT56_MODELS.map((model) => ({
		id: `openai:${model}`,
		label: `openai:${model}`,
		provider: { kind: "openai" as const, model, reasoning_effort: "xhigh" as const }
	})),
	{
		id: "claude:claude-opus-5",
		label: "claude:claude-opus-5",
		provider: { kind: "claude", model: "claude-opus-5", reasoning_effort: "high" }
	},
	{
		id: "claude:claude-opus-4-8",
		label: "claude:claude-opus-4-8",
		provider: { kind: "claude", model: "claude-opus-4-8", reasoning_effort: "xhigh" }
	},
	{
		id: "claude:claude-fable-5",
		label: "claude:claude-fable-5",
		description: "Explicit opt-in: not ZDR.",
		provider: { kind: "claude", model: "claude-fable-5", reasoning_effort: "high" }
	}
];

/** M11b: the bridge profile feeds model options dynamically from the bridge
 * models.list contract (pi provider/model ids, live availability). Consumers
 * call availableModelOptions(); the static MODEL_OPTIONS remain the legacy
 * daemon default and the fallback before the first models.list resolves. */
let dynamicModelOptions: ModelOption[] | null = null;

export function setDynamicModelOptions(options: ModelOption[] | null): void {
	dynamicModelOptions = options;
}

export function availableModelOptions(): ModelOption[] {
	return dynamicModelOptions ?? MODEL_OPTIONS;
}

export const OPENAI_REASONING_EFFORTS: ReasoningEffort[] = ["none", "minimal", "low", "medium", "high", "xhigh"];
export const OPENAI_GPT56_REASONING_EFFORTS: ReasoningEffort[] = [...OPENAI_REASONING_EFFORTS, "max"];
export const CLAUDE_REASONING_EFFORTS: ReasoningEffort[] = ["low", "medium", "high", "xhigh", "max"];

export const DEFAULT_PROVIDER: ProviderConfig = {
	kind: "openai",
	model: "gpt-5.6-luna",
	reasoning_effort: "high"
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
	const option = availableModelOptions().find((candidate) => candidate.id === modelKey);
	if (!option) return current;
	return { ...current, ...option.provider };
}

export function reasoningEffortsForProvider(provider: ProviderConfig): ReasoningEffort[] {
	if (provider.kind === "claude") return CLAUDE_REASONING_EFFORTS;
	return HOSTED_GPT56_MODELS.some((model) => model === provider.model)
		? OPENAI_GPT56_REASONING_EFFORTS
		: OPENAI_REASONING_EFFORTS;
}

export function newSessionCompactionConfig() {
	return {
		auto_enabled: true,
		max_consecutive_failures: 3,
	};
}
