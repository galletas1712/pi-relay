import Anthropic from "@anthropic-ai/sdk";
import type { ModelInfo as AnthropicModelInfo } from "@anthropic-ai/sdk/resources/models.js";
import type { Api, Model, ModelCapabilities } from "./types.js";

type HydrationOptions = {
	apiKey: string;
	headers?: Record<string, string>;
};

const listCache = new Map<string, Promise<Map<string, AnthropicModelInfo>>>();
const retrieveCache = new Map<string, Promise<AnthropicModelInfo>>();

function isOAuthToken(apiKey: string): boolean {
	return apiKey.includes("sk-ant-oat");
}

function createClient(baseUrl: string, options: HydrationOptions): Anthropic {
	const defaultHeaders = options.headers && Object.keys(options.headers).length > 0 ? options.headers : undefined;
	if (isOAuthToken(options.apiKey)) {
		return new Anthropic({
			apiKey: null,
			authToken: options.apiKey,
			baseURL: baseUrl,
			defaultHeaders,
		});
	}

	return new Anthropic({
		apiKey: options.apiKey,
		baseURL: baseUrl,
		defaultHeaders,
	});
}

export function isDirectAnthropicBaseUrl(baseUrl: string): boolean {
	try {
		return new URL(baseUrl).hostname === "api.anthropic.com";
	} catch {
		return baseUrl.includes("api.anthropic.com");
	}
}

export function canHydrateAnthropicCapabilities<TApi extends Api>(model: Model<TApi>): boolean {
	return model.api === "anthropic-messages" && isDirectAnthropicBaseUrl(model.baseUrl);
}

function applyAnthropicModelInfo<TApi extends Api>(model: Model<TApi>, info: AnthropicModelInfo): Model<TApi> {
	model.capabilities = (info.capabilities as ModelCapabilities | null | undefined) ?? null;
	if (typeof info.max_input_tokens === "number") {
		model.contextWindow = info.max_input_tokens;
	}
	if (typeof info.max_tokens === "number") {
		model.maxTokens = info.max_tokens;
	}
	return model;
}

async function getAnthropicModelList(
	baseUrl: string,
	options: HydrationOptions,
): Promise<Map<string, AnthropicModelInfo>> {
	const cacheKey = baseUrl;
	let cached = listCache.get(cacheKey);
	if (!cached) {
		cached = (async () => {
			const client = createClient(baseUrl, options);
			const infos = new Map<string, AnthropicModelInfo>();
			for await (const info of client.models.list({ limit: 1000 })) {
				infos.set(info.id, info);
			}
			return infos;
		})();
		listCache.set(cacheKey, cached);
	}

	try {
		return await cached;
	} catch (error) {
		listCache.delete(cacheKey);
		throw error;
	}
}

async function getAnthropicModelInfo(
	baseUrl: string,
	modelId: string,
	options: HydrationOptions,
): Promise<AnthropicModelInfo> {
	const cacheKey = `${baseUrl}|${modelId}`;
	let cached = retrieveCache.get(cacheKey);
	if (!cached) {
		cached = createClient(baseUrl, options).models.retrieve(modelId);
		retrieveCache.set(cacheKey, cached);
	}

	try {
		return await cached;
	} catch (error) {
		retrieveCache.delete(cacheKey);
		throw error;
	}
}

export async function hydrateAnthropicModelCapabilities<TApi extends Api>(
	model: Model<TApi>,
	options: HydrationOptions,
): Promise<Model<TApi>> {
	if (!canHydrateAnthropicCapabilities(model) || model.capabilities) {
		return model;
	}

	const info = await getAnthropicModelInfo(model.baseUrl, model.id, options);
	return applyAnthropicModelInfo(model, info);
}

export async function hydrateAnthropicModelsCapabilities<TApi extends Api>(
	models: Model<TApi>[],
	options: HydrationOptions,
): Promise<Model<TApi>[]> {
	const eligibleModels = models.filter((model) => canHydrateAnthropicCapabilities(model) && !model.capabilities);
	if (eligibleModels.length === 0) {
		return models;
	}

	const modelInfos = await getAnthropicModelList(eligibleModels[0].baseUrl, options);
	const missingModels: Model<TApi>[] = [];

	for (const model of eligibleModels) {
		const info = modelInfos.get(model.id);
		if (info) {
			applyAnthropicModelInfo(model, info);
		} else {
			missingModels.push(model);
		}
	}

	await Promise.all(
		missingModels.map(async (model) => {
			try {
				await hydrateAnthropicModelCapabilities(model, options);
			} catch {
				// Keep the existing static metadata when capability discovery fails.
			}
		}),
	);

	return models;
}
