import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("MCP picker layout", () => {
	it("does not retain an obsolete hidden inventory rule", () => {
		const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");

		expect(css).not.toContain(".mcp-picker-list[hidden]");
	});

	it("uses the central transcript scroll and stacks full-width setup cards on mobile", () => {
		const css = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
		const setupRule = css.match(/\.new-session-setup\s*\{[^}]+\}/)?.[0] ?? "";
		const gridRule = css.match(/\.new-session-setup-grid\s*\{[^}]+\}/)?.[0] ?? "";

		expect(setupRule).toContain("width: 100%");
		expect(gridRule).toContain(
			"grid-template-columns: repeat(auto-fit, minmax(min(100%, 340px), 1fr))",
		);
		expect(css).not.toMatch(/\.mcp-picker-list\s*\{[^}]*max-height/);
		expect(css).not.toMatch(/\.workspace-scope-list\s*\{[^}]*max-height/);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.new-session-setup-grid\s*\{\s*grid-template-columns: minmax\(0, 1fr\)/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.workspace-scope-name,[\s\S]*?min-height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.workspace-scope-detail\s*\{[^}]*flex-wrap: wrap/,
		);
		expect(css).toContain(
			`.workspace-scope-toggle,
	.mcp-picker-toggle,
	.workspace-scope-name,
	.workspace-scope-branch,
	.mcp-picker-tool,
	.mcp-picker-server-name,
	.mcp-picker-auth-action,
	.new-session-setup-error button {
		min-height: 44px;
	}`,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.new-session-setup-error\s*\{[^}]*flex-wrap: wrap/,
		);
	});
});
