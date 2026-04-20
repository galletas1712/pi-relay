import { MODELS } from "./models.generated.js";
import {
	type Api,
	type KnownProvider,
	type Model,
	PRESET_THINKING_LEVELS,
	type ThinkingLevel,
	type Usage,
} from "./types.js";

const modelRegistry: Map<string, Map<string, Model<Api>>> = new Map();

// Initialize registry from MODELS on module load
for (const [provider, models] of Object.entries(MODELS)) {
	const providerModels = new Map<string, Model<Api>>();
	for (const [id, model] of Object.entries(models)) {
		providerModels.set(id, model as Model<Api>);
	}
	modelRegistry.set(provider, providerModels);
}

type ModelApi<
	TProvider extends KnownProvider,
	TModelId extends keyof (typeof MODELS)[TProvider],
> = (typeof MODELS)[TProvider][TModelId] extends { api: infer TApi } ? (TApi extends Api ? TApi : never) : never;

export function getModel<TProvider extends KnownProvider, TModelId extends keyof (typeof MODELS)[TProvider]>(
	provider: TProvider,
	modelId: TModelId,
): Model<ModelApi<TProvider, TModelId>> {
	const providerModels = modelRegistry.get(provider);
	return providerModels?.get(modelId as string) as Model<ModelApi<TProvider, TModelId>>;
}

export function getProviders(): KnownProvider[] {
	return Array.from(modelRegistry.keys()) as KnownProvider[];
}

export function getModels<TProvider extends KnownProvider>(
	provider: TProvider,
): Model<ModelApi<TProvider, keyof (typeof MODELS)[TProvider]>>[] {
	const models = modelRegistry.get(provider);
	return models ? (Array.from(models.values()) as Model<ModelApi<TProvider, keyof (typeof MODELS)[TProvider]>>[]) : [];
}

/**
 * Ratio of Anthropic's 1-hour cache-write price to its 5-minute cache-write
 * price. From the prompt-caching docs: 5m writes bill at 1.25× the base input
 * rate; 1h writes bill at 2×. 2 / 1.25 = 1.6, and this ratio is uniform across
 * every active Claude model family (Opus 4.7, Sonnet 4.6, Haiku 4.5, and all
 * their predecessors we ship pricing for).
 *
 * `model.cost.cacheWrite` in the catalog encodes the 5-minute rate, so when a
 * provider reports a TTL breakdown via `usage.cacheWrite1h`, we bill those
 * tokens at 1.6× the catalog rate to match actual invoices.
 */
const CACHE_WRITE_1H_TO_5M_RATIO = 1.6;

export function calculateCost<TApi extends Api>(model: Model<TApi>, usage: Usage): Usage["cost"] {
	usage.cost.input = (model.cost.input / 1000000) * usage.input;
	usage.cost.output = (model.cost.output / 1000000) * usage.output;
	usage.cost.cacheRead = (model.cost.cacheRead / 1000000) * usage.cacheRead;

	// When the provider reported a 5m/1h breakdown, bill each tier at its own
	// rate. Otherwise, charge the whole `cacheWrite` aggregate at the 5m rate
	// (back-compat for providers that don't surface TTL metadata).
	if (usage.cacheWrite1h !== undefined && usage.cacheWrite1h > 0) {
		const fiveM =
			usage.cacheWrite5m !== undefined
				? usage.cacheWrite5m
				: Math.max(0, usage.cacheWrite - usage.cacheWrite1h);
		const effectiveTokens = fiveM + CACHE_WRITE_1H_TO_5M_RATIO * usage.cacheWrite1h;
		usage.cost.cacheWrite = (model.cost.cacheWrite / 1000000) * effectiveTokens;
	} else {
		usage.cost.cacheWrite = (model.cost.cacheWrite / 1000000) * usage.cacheWrite;
	}

	usage.cost.total = usage.cost.input + usage.cost.output + usage.cost.cacheRead + usage.cost.cacheWrite;
	return usage.cost;
}

const ANTHROPIC_ADAPTIVE_LEVEL_ORDER = ["low", "medium", "high", "xhigh", "max"] as const;
const THINKING_LEVEL_ORDER = ["off", "minimal", "low", "medium", "high", "xhigh", "max"] as const;
const OPENAI_REASONING_LEVELS = [...PRESET_THINKING_LEVELS, "xhigh"] as const;

function getAnthropicAdaptiveThinkingLevels(model: Model<"anthropic-messages">): ThinkingLevel[] {
	const adaptiveThinkingSupported = model.capabilities?.thinking?.types.adaptive.supported;
	const effort = model.capabilities?.effort;

	if (adaptiveThinkingSupported && effort?.supported) {
		const levels = ANTHROPIC_ADAPTIVE_LEVEL_ORDER.filter((level) => {
			const capability = effort[level];
			return capability?.supported === true;
		});
		// Opus 4.7 accepts effort=xhigh on the Messages API even though the Models
		// API does not advertise it in capabilities. Inject it between high and max.
		if (model.id === "claude-opus-4-7" && !levels.includes("xhigh")) {
			const maxIndex = levels.indexOf("max");
			levels.splice(maxIndex >= 0 ? maxIndex : levels.length, 0, "xhigh");
		}
		return levels;
	}

	if (adaptiveThinkingSupported === false) {
		return [];
	}
	return ["low", "medium", "high", "xhigh"];
}

function isOpenAIReasoningModel<TApi extends Api>(model: Model<TApi>): boolean {
	return (
		model.reasoning &&
		(model.provider === "openai" || model.provider === "openai-codex" || model.provider === "azure-openai-responses")
	);
}

export function getThinkingLevels<TApi extends Api>(model: Model<TApi>): ThinkingLevel[] {
	if (!model.reasoning) {
		return [];
	}

	if (model.api === "anthropic-messages") {
		const adaptiveLevels = getAnthropicAdaptiveThinkingLevels(model as Model<"anthropic-messages">);
		if (adaptiveLevels.length > 0) {
			return adaptiveLevels;
		}
		return [...PRESET_THINKING_LEVELS];
	}

	if (isOpenAIReasoningModel(model)) {
		return [...OPENAI_REASONING_LEVELS];
	}

	return [...PRESET_THINKING_LEVELS];
}

export function clampThinkingLevel(
	level: ThinkingLevel | undefined,
	availableLevels: ThinkingLevel[],
): ThinkingLevel | undefined {
	if (level === undefined || availableLevels.includes(level)) {
		return level;
	}

	const requestedIndex = THINKING_LEVEL_ORDER.indexOf(level as (typeof THINKING_LEVEL_ORDER)[number]);
	if (requestedIndex === -1) {
		return availableLevels[0];
	}

	for (let i = requestedIndex; i < THINKING_LEVEL_ORDER.length; i++) {
		const candidate = THINKING_LEVEL_ORDER[i];
		if (availableLevels.includes(candidate)) {
			return candidate;
		}
	}

	for (let i = requestedIndex - 1; i >= 0; i--) {
		const candidate = THINKING_LEVEL_ORDER[i];
		if (availableLevels.includes(candidate)) {
			return candidate;
		}
	}

	return availableLevels[0];
}
/**
 * Check if two models are equal by comparing both their id and provider.
 * Returns false if either model is null or undefined.
 */
export function modelsAreEqual<TApi extends Api>(
	a: Model<TApi> | null | undefined,
	b: Model<TApi> | null | undefined,
): boolean {
	if (!a || !b) return false;
	return a.id === b.id && a.provider === b.provider;
}
