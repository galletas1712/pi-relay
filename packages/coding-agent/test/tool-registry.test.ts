/**
 * Unit tests for the shared `ToolRegistry`.
 *
 * Exercises the two-layer resolver without touching the ExtensionRunner so
 * we can verify:
 *   - single-provider auto-bind -> bare interface name,
 *   - multi-provider + no config -> throws with candidate list,
 *   - multi-provider + explicit config -> picks correct one, ignores others,
 *   - user-declared two tools with different names (bash + bash_prod),
 *   - unknown provider id -> throws,
 *   - provider implementing unknown interface -> diagnostic + skip,
 *   - merge semantics across multiple configureTools calls,
 *   - missing required secret -> ToolConfigMissingError.
 */

import { defineToolInterface, defineToolProvider, ToolConfigMissingError } from "@pi-relay/tool-kit";
import type { ToolHost } from "@pi-relay/tool-kit";
import { Type } from "@sinclair/typebox";
import { describe, expect, it, vi } from "vitest";
import type { ExtensionContext } from "../src/core/extensions/types.js";
import { ToolInterfaceRegistry, webSearchInterface } from "../src/core/tool-packages/interfaces.js";
import { ToolRegistry } from "../src/core/tool-packages/tools.js";

function makeFakeExtCtx(): ExtensionContext {
	const fail = () => {
		throw new Error("not used in this test");
	};
	return {
		ui: {} as ExtensionContext["ui"],
		hasUI: false,
		cwd: "/tmp",
		sessionManager: {} as ExtensionContext["sessionManager"],
		modelRegistry: {} as ExtensionContext["modelRegistry"],
		model: undefined,
		isIdle: () => true,
		signal: undefined,
		abort: fail,
		hasPendingMessages: () => false,
		shutdown: fail,
		getContextUsage: () => undefined,
		compact: fail,
		getSystemPrompt: () => "",
	};
}

function fakeHost(): ToolHost {
	return {
		getModel: () => undefined,
		getApiKey: async () => ({ ok: false, error: "none" }),
		http: globalThis.fetch.bind(globalThis),
	};
}

function webSearchProvider(idSuffix: string, opts?: { envVar?: string; implementsOverride?: string }) {
	return defineToolProvider<{ model: string }, { apiKey: string }>({
		id: `com.test.${idSuffix}`,
		displayName: idSuffix,
		version: "0.0.1",
		implements: opts?.implementsOverride ?? "web_search",
		defaultConfig: { model: "default" },
		secrets: opts?.envVar
			? [{ key: "apiKey", displayName: "API", kind: "api_key", envVar: opts.envVar }]
			: undefined,
		parameters: Type.Object({ query: Type.String() }),
		async execute(params, ctx) {
			return {
				content: [
					{
						type: "text",
						text: `${idSuffix}:${params.query}:${ctx.config.model}:${ctx.secrets.apiKey ?? "<none>"}:${ctx.toolName}`,
					},
				],
				details: { idSuffix, toolName: ctx.toolName },
			};
		},
	});
}

const bashInterface = defineToolInterface({
	name: "bash",
	description: "Run a shell command.",
	parameters: Type.Object({ command: Type.String() }),
});

function bashProvider(idSuffix: string) {
	return defineToolProvider<{ host?: string }, Record<string, never>>({
		id: `bash.${idSuffix}`,
		displayName: `bash ${idSuffix}`,
		version: "0.0.1",
		implements: "bash",
		defaultConfig: {},
		parameters: Type.Object({ command: Type.String() }),
		async execute(params, ctx) {
			return {
				content: [
					{
						type: "text",
						text: `${idSuffix}:${ctx.toolName}:${ctx.config.host ?? "<none>"}:${params.command}`,
					},
				],
				details: { idSuffix, host: ctx.config.host },
			};
		},
	});
}

function bashEnabledInterfaces(): ToolInterfaceRegistry {
	const interfaces = new ToolInterfaceRegistry(new Map());
	interfaces.register(webSearchInterface);
	interfaces.register(bashInterface);
	return interfaces;
}

describe("ToolRegistry: single-provider auto-bind", () => {
	it("exposes a single provider under the bare interface name", () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		registry.registerProvider(webSearchProvider("only"));
		const defs = registry.resolve();
		expect(defs.map((d) => d.name)).toEqual(["web_search"]);
	});
});

describe("ToolRegistry: multiple-provider behavior", () => {
	it("throws with a helpful message when >1 providers implement the same interface and no config disambiguates", () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		registry.registerProvider(webSearchProvider("codex"));
		registry.registerProvider(webSearchProvider("perplexity"));
		expect(() => registry.resolve()).toThrow(/Multiple providers implement "web_search"/);
		try {
			registry.resolve();
		} catch (e) {
			expect((e as Error).message).toContain("com.test.codex");
			expect((e as Error).message).toContain("com.test.perplexity");
		}
	});

	it("picks the configured provider and ignores the others", async () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		registry.registerProvider(webSearchProvider("codex"));
		registry.registerProvider(webSearchProvider("perplexity"));
		registry.configureTools({ web_search: { provider: "com.test.perplexity" } });
		const defs = registry.resolve();
		expect(defs.map((d) => d.name)).toEqual(["web_search"]);
		const ctx = makeFakeExtCtx();
		const out = await defs[0].execute("c1", { query: "hi" }, undefined, undefined, ctx);
		expect((out.content[0] as { text: string }).text).toContain("perplexity:hi");
	});
});

describe("ToolRegistry: user-declared named tools", () => {
	it("exposes two bash tools with user-chosen names", async () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		registry.registerProvider(bashProvider("local"));
		registry.registerProvider(bashProvider("ssh"));
		registry.configureTools({
			bash: { provider: "bash.local" },
			bash_prod: { provider: "bash.ssh", config: { host: "prod.example.com" } },
		});
		const defs = registry.resolve();
		expect(defs.map((d) => d.name).sort()).toEqual(["bash", "bash_prod"]);

		const bashDef = defs.find((d) => d.name === "bash")!;
		const prodDef = defs.find((d) => d.name === "bash_prod")!;
		const ctx = makeFakeExtCtx();
		const a = await bashDef.execute("c1", { command: "ls" }, undefined, undefined, ctx);
		const b = await prodDef.execute("c2", { command: "ls" }, undefined, undefined, ctx);
		expect((a.content[0] as { text: string }).text).toContain("local:bash:<none>:ls");
		expect((b.content[0] as { text: string }).text).toContain("ssh:bash_prod:prod.example.com:ls");
	});
});

describe("ToolRegistry: diagnostics", () => {
	it("throws if a tool config references an unknown provider id", () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		registry.registerProvider(webSearchProvider("real"));
		registry.configureTools({ web_search: { provider: "com.test.does-not-exist" } });
		expect(() => registry.resolve()).toThrow(/references unknown provider "com.test.does-not-exist"/);
	});

	it("skips a tool whose provider doesn't implement any registered interface, with a warning", () => {
		const warn = vi.fn();
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
			warn,
		});
		// Register a provider implementing an unknown interface.
		// `registerProvider` warns up-front about that.
		registry.registerProvider(webSearchProvider("odd", { implementsOverride: "some.unknown.interface" }));
		expect(warn).toHaveBeenCalledWith(expect.stringContaining("unknown interface"));
		registry.configureTools({ odd_tool: { provider: "com.test.odd" } });
		const defs = registry.resolve();
		expect(defs).toEqual([]);
	});

	it("rejects a configureTools entry with a missing provider string", () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		expect(() =>
			// biome-ignore lint/suspicious/noExplicitAny: intentionally exercising the runtime validation path.
			registry.configureTools({ bad: { provider: "" } as any }),
		).toThrow(/must be an object with a non-empty "provider" string/);
	});
});

describe("ToolRegistry: configureTools merge semantics", () => {
	it("later call wins per tool name, with a warning when the provider changes", () => {
		const warn = vi.fn();
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
			warn,
		});
		registry.registerProvider(webSearchProvider("a"));
		registry.registerProvider(webSearchProvider("b"));
		registry.configureTools({ web_search: { provider: "com.test.a" } });
		registry.configureTools({ web_search: { provider: "com.test.b" } });
		expect(warn).toHaveBeenCalledWith(
			expect.stringContaining('tool "web_search" was previously bound to "com.test.a"; overriding with "com.test.b"'),
		);
		const defs = registry.resolve();
		expect(defs.map((d) => d.name)).toEqual(["web_search"]);
	});

	it("does not warn when the same provider is re-asserted", () => {
		const warn = vi.fn();
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
			warn,
		});
		registry.registerProvider(webSearchProvider("a"));
		registry.configureTools({ web_search: { provider: "com.test.a" } });
		registry.configureTools({ web_search: { provider: "com.test.a", config: { model: "override" } } });
		expect(warn).not.toHaveBeenCalled();
	});
});

describe("ToolRegistry: secrets", () => {
	it("throws ToolConfigMissingError when a required secret is absent", async () => {
		delete process.env.REGISTRY_TEST_MISSING_KEY;
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		registry.registerProvider(webSearchProvider("needs-secret", { envVar: "REGISTRY_TEST_MISSING_KEY" }));
		const [def] = registry.resolve();
		expect(def.name).toBe("web_search");
		await expect(def.execute("c1", { query: "q" }, undefined, undefined, makeFakeExtCtx())).rejects.toBeInstanceOf(
			ToolConfigMissingError,
		);
	});
});

describe("ToolRegistry: LLM-facing description", () => {
	it("uses the interface description and ignores provider.description", () => {
		const registry = new ToolRegistry({
			interfaces: bashEnabledInterfaces(),
			hostFactory: () => fakeHost(),
		});
		// A provider with a distinct description that would leak its identity
		// if the resolver used it verbatim.
		const leaky = defineToolProvider({
			id: "com.test.leaky",
			displayName: "Leaky",
			version: "0.0.1",
			implements: "web_search",
			description: "Search the web via ACME Corp.'s proprietary search (should not reach the LLM).",
			parameters: Type.Object({ query: Type.String() }),
			async execute() {
				return { content: [], details: undefined };
			},
		});
		registry.registerProvider(leaky);
		const [def] = registry.resolve();
		expect(def.description).toBe(webSearchInterface.description);
		expect(def.description).not.toContain("ACME");
	});
});
