import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("MCP picker layout", () => {
	it("does not retain an obsolete hidden inventory rule", () => {
		const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");

		expect(css).not.toContain(".mcp-picker-list[hidden]");
	});

	it("keeps workspace and MCP setup lists internally scrollable on desktop and mobile", () => {
		const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
		const mcpListRule = css.match(/\.mcp-picker-list\s*\{[^}]+\}/)?.[0] ?? "";
		const workspaceListRule = css.match(/\.workspace-scope-list\s*\{[^}]+\}/)?.[0] ?? "";

		for (const rule of [workspaceListRule, mcpListRule]) {
			expect(rule).toContain("max-height: min(40dvh, 420px)");
			expect(rule).toContain("overflow-y: auto");
			expect(rule).toContain("overscroll-behavior: contain");
		}
		expect(css).toMatch(/\.workspace-scope-list,\s*\.mcp-picker-list\s*\{\s*max-height: min\(32dvh, 280px\)/);
	});
});
