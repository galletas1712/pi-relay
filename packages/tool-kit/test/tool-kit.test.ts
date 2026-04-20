/**
 * Unit tests for @pi-relay/tool-kit author-facing helpers.
 */

import { Type } from "@sinclair/typebox";
import { describe, expect, it } from "vitest";
import {
	defineToolInterface,
	defineToolProvider,
	isToolProvider,
	ToolConfigMissingError,
	type ToolProvider,
} from "../src/index.js";

describe("defineToolInterface", () => {
	it("is an identity function that preserves inference", () => {
		const iface = defineToolInterface({
			name: "web_search",
			description: "search the web",
			parameters: Type.Object({ query: Type.String() }),
		});
		expect(iface.name).toBe("web_search");
		expect(iface.parameters).toBeDefined();
	});
});

describe("defineToolProvider", () => {
	it("preserves config and secret generics", () => {
		interface Config {
			retries: number;
		}
		interface Secrets {
			apiKey: string;
		}
		const provider: ToolProvider<Config, Secrets> = defineToolProvider<Config, Secrets>({
			id: "com.example.cfg",
			implements: "web_search",
			displayName: "Config Example",
			version: "0.0.1",
			defaultConfig: { retries: 3 },
			secrets: [{ key: "apiKey", displayName: "API Key", kind: "api_key", envVar: "EXAMPLE_KEY" }],
			parameters: Type.Object({ query: Type.String() }),
			async execute(_params, ctx) {
				// Type assertion: config.retries is typed as number, secrets.apiKey as string.
				const retries: number = ctx.config.retries;
				const key: string = ctx.secrets.apiKey;
				return {
					content: [{ type: "text", text: `${key}:${retries}` }],
					details: undefined,
				};
			},
		});
		expect(provider.defaultConfig?.retries).toBe(3);
		expect(provider.secrets?.[0].envVar).toBe("EXAMPLE_KEY");
		expect(provider.implements).toBe("web_search");
	});
});

describe("isToolProvider", () => {
	it("accepts a valid provider", () => {
		const provider = defineToolProvider({
			id: "com.example.a",
			implements: "web_search",
			displayName: "A",
			version: "0.0.1",
			parameters: Type.Object({}),
			async execute() {
				return { content: [], details: undefined };
			},
		});
		expect(isToolProvider(provider)).toBe(true);
	});

	it("rejects extension factory functions and other values", () => {
		expect(isToolProvider(null)).toBe(false);
		expect(isToolProvider(undefined)).toBe(false);
		expect(isToolProvider(() => {})).toBe(false);
		expect(isToolProvider({})).toBe(false);
		expect(isToolProvider({ id: "", implements: "x", displayName: "x", version: "0", execute: () => 0 })).toBe(false);
		expect(isToolProvider({ id: "x", implements: "", displayName: "x", version: "0", execute: () => 0 })).toBe(false);
		expect(isToolProvider({ id: "x", implements: "x", displayName: "x", version: "0" })).toBe(false);
	});
});

describe("ToolConfigMissingError", () => {
	it("formats a helpful message", () => {
		const err = new ToolConfigMissingError("com.example.a", ["apiKey"], "Set EXAMPLE_KEY.");
		expect(err.providerId).toBe("com.example.a");
		expect(err.missing).toEqual(["apiKey"]);
		expect(err.message).toContain("com.example.a");
		expect(err.message).toContain("apiKey");
		expect(err.message).toContain("EXAMPLE_KEY");
	});
});
