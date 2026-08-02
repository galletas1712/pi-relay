import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

describe("MCP picker layout", () => {
	it("does not retain an obsolete hidden inventory rule", () => {
		const css = readFileSync(resolve(import.meta.dirname, "domain.css"), "utf8");

		expect(css).not.toContain(".mcp-picker-list[hidden]");
	});

	it("uses the central transcript scroll and a responsive stacked context manifest", () => {
		const css = readFileSync(resolve(import.meta.dirname, "domain.css"), "utf8");
		const setupRule = css.match(/\.new-session-setup\s*\{[^}]+\}/)?.[0] ?? "";
		const manifestRule = css.match(/\.new-session-setup-manifest\s*\{[^}]+\}/)?.[0] ?? "";
		const srOnlyRule = css.match(/\.sr-only\s*\{[^}]+\}/)?.[0] ?? "";
		const disclosureFocusRule = css.match(
			/\.workspace-scope-toggle:focus-visible,\s*\.mcp-picker-toggle:focus-visible\s*\{[^}]+\}/,
		)?.[0] ?? "";

		expect(setupRule).toContain("width: 100%");
		expect(manifestRule).toContain("overflow: hidden");
		expect(srOnlyRule).toContain("position: absolute");
		expect(srOnlyRule).toContain("clip: rect(0, 0, 0, 0)");
		expect(srOnlyRule).not.toMatch(/display:\s*none|visibility:\s*hidden/);
		expect(disclosureFocusRule).toContain("border-radius: calc(var(--radius-lg) - 1px)");
		expect(disclosureFocusRule).toContain("outline: 2px solid var(--ring)");
		expect(disclosureFocusRule).toContain("outline-offset: -3px");
		expect(css).toContain(".new-session-setup-section + .new-session-setup-section");
		expect(css).not.toContain(".setup-disclosure-copy");
		expect(css).not.toContain(".setup-disclosure-description");
		expect(css).not.toMatch(/\.mcp-picker-list\s*\{[^}]*max-height/);
		expect(css).not.toMatch(/\.workspace-scope-list\s*\{[^}]*max-height/);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.workspace-scope-name,[\s\S]*?min-height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.workspace-scope-detail\s*\{[^}]*flex-wrap: wrap/,
		);
		expect(css).toMatch(
			/\.mcp-picker-tool\s*\{[^}]*min-height: 44px/,
		);
		expect(css).toMatch(
			/\.mcp-picker-server-check-target\s*\{[^}]*width: 44px;[^}]*height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.mcp-oauth-dialog \[data-slot="input-group"\]\s*\{[^}]*height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.mcp-oauth-dialog \[data-slot="input-group-button"\]\s*\{[^}]*width: 44px;[^}]*height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.mcp-oauth-open\s*\{[^}]*min-height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.mcp-oauth-dialog > \.dialog-header \.plain-close-button,[\s\S]*?width: 44px;[^}]*height: 44px/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.new-session-setup-error\s*\{[^}]*flex-wrap: wrap/,
		);
		expect(css).toMatch(
			/@media \(max-width: 640px\)[\s\S]*?\.mcp-oauth-expiration\s*\{[^}]*order: 3;[^}]*width: 100%/,
		);
		expect(css).not.toMatch(/^\s*size\s*:/m);
		expect(css).not.toContain(".new-session-setup-error button");
		expect(css).toContain(".workspace-scope-toggle:focus-visible");
		expect(css).toContain(".mcp-picker-toggle:focus-visible");
		expect(css).not.toContain("transition: all");
	});
});
