/**
 * Smoke test: load real TS extension files that each register a
 * `ToolProvider` via the extension loader and assert:
 *   - the providers show up in `extension.toolProviders`,
 *   - the shared `ToolRegistry` resolves them into `ToolDefinition[]`,
 *   - a single provider auto-binds under the bare interface name,
 *   - two providers for the same interface + no config throws at resolve
 *     time with a clear message,
 *   - two providers + `pi.configureTools({...})` selects the right one.
 */

import * as fs from "node:fs";
import * as os from "node:os";
import * as path from "node:path";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { loadExtensions } from "../src/core/extensions/loader.js";

const CODEX_LIKE_PROVIDER = `
import { defineToolProvider } from "@pi-relay/tool-kit";
import { Type } from "@sinclair/typebox";

const provider = defineToolProvider({
  id: "com.test.codex-search",
  implements: "web_search",
  displayName: "Codex-like",
  version: "0.0.1",
  parameters: Type.Object({ query: Type.String() }),
  async execute(params, ctx) {
    return {
      content: [{ type: "text", text: "codex:" + params.query + ":" + ctx.toolName }],
      details: { toolName: ctx.toolName },
    };
  },
});

export default function (pi) {
  pi.registerToolProvider(provider);
}
`;

const PERPLEXITY_LIKE_PROVIDER = `
import { defineToolProvider } from "@pi-relay/tool-kit";
import { Type } from "@sinclair/typebox";

const provider = defineToolProvider({
  id: "com.test.perplexity-search",
  implements: "web_search",
  displayName: "Perplexity-like",
  version: "0.0.1",
  parameters: Type.Object({ query: Type.String() }),
  async execute(params, ctx) {
    return {
      content: [{ type: "text", text: "perplexity:" + params.query + ":" + ctx.toolName }],
      details: { toolName: ctx.toolName },
    };
  },
});

export default function (pi) {
  pi.registerToolProvider(provider);
}
`;

const CONFIGURE_WEB_SEARCH_PICKS_PERPLEXITY = `
export default function (pi) {
  pi.configureTools({ web_search: { provider: "com.test.perplexity-search" } });
}
`;

function makeFakeExtCtx(cwd: string) {
	return {
		ui: {} as never,
		hasUI: false,
		cwd,
		sessionManager: {} as never,
		modelRegistry: {} as never,
		model: undefined,
		isIdle: () => true,
		signal: undefined,
		abort: () => {
			throw new Error("abort not expected");
		},
		hasPendingMessages: () => false,
		shutdown: () => {
			throw new Error("shutdown not expected");
		},
		getContextUsage: () => undefined,
		compact: () => {
			throw new Error("compact not expected");
		},
		getSystemPrompt: () => "",
	};
}

describe("loadExtensions + registerToolProvider (smoke)", () => {
	let tempDir: string;

	beforeEach(() => {
		tempDir = fs.mkdtempSync(path.join(os.tmpdir(), "pi-tool-provider-smoke-"));
	});

	afterEach(() => {
		fs.rmSync(tempDir, { recursive: true, force: true });
	});

	it("single provider registers under the bare interface name", async () => {
		const extPath = path.join(tempDir, "codex.ts");
		fs.writeFileSync(extPath, CODEX_LIKE_PROVIDER, "utf-8");
		const result = await loadExtensions([extPath], tempDir);
		expect(result.errors).toEqual([]);
		expect(result.extensions).toHaveLength(1);
		const ext = result.extensions[0];
		expect(ext.toolProviders?.has("com.test.codex-search")).toBe(true);

		const defs = result.runtime.toolRegistry.resolve();
		expect(defs.map((d) => d.name)).toEqual(["web_search"]);

		// Execute end-to-end.
		// biome-ignore lint/suspicious/noExplicitAny: minimal fake ExtensionContext is sufficient for the execute path here.
		const ctx = makeFakeExtCtx(tempDir) as any;
		const out = await defs[0].execute("c1", { query: "hi" }, undefined, undefined, ctx);
		expect((out.content[0] as { text: string }).text).toContain("codex:hi:web_search");
	});

	it("two providers + no config throws a helpful error at resolve time", async () => {
		const a = path.join(tempDir, "codex.ts");
		const b = path.join(tempDir, "perplexity.ts");
		fs.writeFileSync(a, CODEX_LIKE_PROVIDER, "utf-8");
		fs.writeFileSync(b, PERPLEXITY_LIKE_PROVIDER, "utf-8");
		const result = await loadExtensions([a, b], tempDir);
		expect(result.errors).toEqual([]);
		expect(result.extensions).toHaveLength(2);

		expect(() => result.runtime.toolRegistry.resolve()).toThrow(/Multiple providers implement "web_search"/);
	});

	it("two providers + configureTools picks the selected one", async () => {
		const a = path.join(tempDir, "codex.ts");
		const b = path.join(tempDir, "perplexity.ts");
		const cfg = path.join(tempDir, "config.ts");
		fs.writeFileSync(a, CODEX_LIKE_PROVIDER, "utf-8");
		fs.writeFileSync(b, PERPLEXITY_LIKE_PROVIDER, "utf-8");
		fs.writeFileSync(cfg, CONFIGURE_WEB_SEARCH_PICKS_PERPLEXITY, "utf-8");
		const result = await loadExtensions([a, b, cfg], tempDir);
		expect(result.errors).toEqual([]);

		const defs = result.runtime.toolRegistry.resolve();
		expect(defs.map((d) => d.name)).toEqual(["web_search"]);

		// biome-ignore lint/suspicious/noExplicitAny: minimal fake ExtensionContext is sufficient here.
		const ctx = makeFakeExtCtx(tempDir) as any;
		const out = await defs[0].execute("c1", { query: "hi" }, undefined, undefined, ctx);
		expect((out.content[0] as { text: string }).text).toContain("perplexity:hi");
	});
});
