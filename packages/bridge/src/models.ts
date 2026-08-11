// M11a: model surface probe — pi's ModelRuntime composed on the bridge's
// agentDir, EXACTLY as createAgentSessionServices does for every host
// (authPath <agentDir>/auth.json, modelsPath <agentDir>/models.json). No
// loaded extension registers providers (grep-verified 2026-08-10), and hosts
// inherit the bridge env, so this probe's catalog + availability equals what
// every session's modelRuntime sees. Cheaper than a probe host: no process,
// no junk session file, refresh-per-TTL.
//
// Availability semantics are upstream's: getAvailableSnapshot() runs
// checkAuth per provider (OAuth = credential presence; apiKey providers may
// run provider checks — the same work session start performs). Auth status
// per provider comes from getProviderAuthStatus (stored/runtime/environment/
// models_json_* sources, no secrets).
import { readFileSync } from "node:fs";
import path from "node:path";
import { ModelRuntime } from "@earendil-works/pi-coding-agent";
import { config } from "./config.ts";

/** pi-ai levels (dist/models.js EXTENDED_THINKING_LEVELS). */
const EXTENDED_THINKING_LEVELS = ["off", "minimal", "low", "medium", "high", "xhigh", "max"] as const;

/** Inline copy of pi-ai getSupportedThinkingLevels (not exported through the
 * pi-coding-agent package root; the logic is stable: non-reasoning → ["off"],
 * else EXTENDED filtered by thinkingLevelMap nulls, xhigh/max only when the
 * map names them explicitly). */
function supportedThinkingLevels(model: { reasoning: boolean; thinkingLevelMap?: Record<string, string | null> }): string[] {
	if (!model.reasoning) return ["off"];
	return EXTENDED_THINKING_LEVELS.filter((level) => {
		const mapped = model.thinkingLevelMap?.[level];
		if (mapped === null) return false;
		if (level === "xhigh" || level === "max") return mapped !== undefined;
		return true;
	});
}

export interface ModelEntry {
	provider: string;
	id: string;
	name: string;
	api: string;
	reasoning: boolean;
	thinkingLevels: string[];
	contextWindow: number;
	maxTokens: number;
	input: string[];
	cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
	baseUrl: string;
	/** "models.json" for custom providers, "builtin" for the catalog. */
	source: "models.json" | "builtin";
	/** In the runtime's available snapshot (upstream set_model gate). */
	available: boolean;
	/** Provider auth configured (cheap, presence-level — never secrets). */
	authConfigured: boolean;
	/** stored | runtime | environment | models_json_key | models_json_command */
	authSource?: string;
}

export interface ModelsResult {
	models: ModelEntry[];
	providers: Array<{ id: string; source: "models.json" | "builtin"; authConfigured: boolean; authSource?: string }>;
	/** settings.json defaults (what new sessions start on). */
	defaults: { provider: string; modelId: string } | null;
	fetchedAt: string;
	/** Composition error (bad models.json etc.) — models may be partial. */
	error?: string;
}

const TTL_MS = 15_000;
let cache: { at: number; result: ModelsResult } | null = null;
let inflight: Promise<ModelsResult> | null = null;

function defaultsFromSettings(): { provider: string; modelId: string } | null {
	try {
		const raw = JSON.parse(readFileSync(path.join(config.agentDir, "settings.json"), "utf8")) as {
			defaultProvider?: string;
			defaultModel?: string;
		};
		if (typeof raw.defaultModel !== "string" || !raw.defaultModel) return null;
		return { provider: raw.defaultProvider ?? "", modelId: raw.defaultModel };
	} catch {
		return null;
	}
}

function modelsJsonProviderIds(): Set<string> {
	try {
		const raw = JSON.parse(readFileSync(path.join(config.agentDir, "models.json"), "utf8")) as {
			providers?: Record<string, unknown>;
		};
		return new Set(Object.keys(raw.providers ?? {}));
	} catch {
		return new Set();
	}
}

async function probe(): Promise<ModelsResult> {
	const runtime = await ModelRuntime.create({
		authPath: path.join(config.agentDir, "auth.json"),
		modelsPath: path.join(config.agentDir, "models.json"),
		signal: AbortSignal.timeout(20_000),
	});
	const custom = modelsJsonProviderIds();
	const available = new Set(runtime.getAvailableSnapshot().map((m) => `${m.provider}/${m.id}`));
	const models: ModelEntry[] = [];
	for (const m of runtime.getModels()) {
		const auth = runtime.getProviderAuthStatus(m.provider);
		models.push({
			provider: m.provider,
			id: m.id,
			name: m.name,
			api: String(m.api),
			reasoning: m.reasoning === true,
			thinkingLevels: supportedThinkingLevels(m),
			contextWindow: m.contextWindow,
			maxTokens: m.maxTokens,
			input: [...m.input],
			cost: { input: m.cost.input, output: m.cost.output, cacheRead: m.cost.cacheRead, cacheWrite: m.cost.cacheWrite },
			baseUrl: m.baseUrl,
			source: custom.has(m.provider) ? "models.json" : "builtin",
			available: available.has(`${m.provider}/${m.id}`),
			authConfigured: auth?.configured === true,
			authSource: auth?.source,
		});
	}
	const providers = runtime.getProviders().map((p) => {
		const auth = runtime.getProviderAuthStatus(p.id);
		return {
			id: p.id,
			source: (custom.has(p.id) ? "models.json" : "builtin") as "models.json" | "builtin",
			authConfigured: auth?.configured === true,
			authSource: auth?.source,
		};
	});
	const result: ModelsResult = {
		models,
		providers,
		defaults: defaultsFromSettings(),
		fetchedAt: new Date().toISOString(),
	};
	const err = runtime.getError();
	if (err) result.error = String(err);
	return result;
}

/** TTL-cached probe; concurrent callers share one refresh; on probe failure
 * serve the last good snapshot (stale) rather than flapping the UI. */
export async function listModels(opts?: { refresh?: boolean }): Promise<ModelsResult> {
	if (!opts?.refresh && cache && Date.now() - cache.at < TTL_MS) return cache.result;
	if (inflight) return inflight;
	inflight = (async () => {
		try {
			const result = await probe();
			cache = { at: Date.now(), result };
			return result;
		} catch (err) {
			if (cache) return { ...cache.result, error: `refresh failed: ${String(err)}` };
			throw err;
		} finally {
			inflight = null;
		}
	})();
	return inflight;
}

/** Test seam: drop the cache so the next call re-probes. */
export function resetModelsCache(): void {
	cache = null;
}
