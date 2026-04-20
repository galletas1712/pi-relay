/**
 * Smoke tests for the @pi-relay/extensions pack entry points.
 *
 * Avoids real network / auth work. Just asserts the entry shapes third
 * parties will import and that:
 *   - each bundled tool-provider factory is invokable,
 *   - the pack's default-export factory registers both providers AND
 *     calls `configureTools` to pick a default for `web_search`, so the
 *     resolver never sees ambiguity when both providers are loaded.
 */

import { describe, expect, it } from "vitest";
import registerExtensions, {
	PACK_DEFAULT_TOOLS,
	registerAllTools,
	registerCodexWebSearch,
	registerPerplexitySonar,
} from "../src/index.js";

type RegisteredProvider = { id: string; implements: string };

interface StubApi {
	registerToolProvider: (provider: { id: string; implements: string }) => void;
	configureTools: (config: Record<string, { provider: string }>) => void;
}

function makeStubApi(): { api: StubApi; providers: RegisteredProvider[]; configs: Array<Record<string, { provider: string }>> } {
	const providers: RegisteredProvider[] = [];
	const configs: Array<Record<string, { provider: string }>> = [];
	const api: StubApi = {
		registerToolProvider: (p) => {
			providers.push({ id: p.id, implements: p.implements });
		},
		configureTools: (cfg) => {
			configs.push(cfg);
		},
	};
	return { api, providers, configs };
}

describe("@pi-relay/extensions", () => {
	it("default export is an async factory", () => {
		expect(typeof registerExtensions).toBe("function");
		expect(registerExtensions.constructor.name).toBe("AsyncFunction");
	});

	it("registerAllTools registers every bundled tool provider", async () => {
		const { api, providers } = makeStubApi();
		await registerAllTools(api as never);
		expect(providers.map((p) => p.id).sort()).toEqual([
			"com.openai.codex.web-search",
			"com.perplexity.sonar",
		]);
		for (const p of providers) {
			expect(p.implements).toBe("web_search");
		}
	});

	it("default export fans out to registerAllTools AND applies the pack default configureTools", async () => {
		const { api, providers, configs } = makeStubApi();
		await registerExtensions(api as never);
		expect(providers).toHaveLength(2);
		expect(configs).toEqual([PACK_DEFAULT_TOOLS]);
		expect(PACK_DEFAULT_TOOLS.web_search.provider).toBe("com.perplexity.sonar");
	});

	it("each provider has its own named factory export", () => {
		expect(typeof registerCodexWebSearch).toBe("function");
		expect(typeof registerPerplexitySonar).toBe("function");
	});
});
