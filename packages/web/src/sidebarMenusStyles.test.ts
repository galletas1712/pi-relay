import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const styles = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");

function rule(selector: string): string {
	const escaped = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
	const match = styles.match(new RegExp(`${escaped}\\s*\\{([^}]+)\\}`));
	if (!match) throw new Error(`missing CSS rule: ${selector}`);
	return match[1];
}

describe("sidebar menu geometry", () => {
	it("reserves one stable sibling trigger column without hover overlays or text-shift padding", () => {
		expect(rule(".project-row")).toContain("grid-template-columns: minmax(0, 1fr) auto");
		expect(rule(".session-row")).toContain("grid-template-columns: minmax(0, 1fr) auto");
		expect(rule(".action-menu-trigger")).toContain("width: 34px");
		expect(rule(".action-menu-trigger")).toContain("height: 34px");
		expect(styles).not.toMatch(/\.session-row-actions?\b/);
		expect(styles).not.toContain("--session-row-actions-width");
		expect(rule(".session-main")).not.toContain("padding-right");
		expect(rule(".session-main")).not.toContain("transition");
	});

	it("provides a 44px overflow target for coarse pointers and mobile", () => {
		expect(styles).toMatch(
			/@media \(pointer: coarse\), \(max-width: 700px\)\s*\{[\s\S]*?\.action-menu-trigger\s*\{[\s\S]*?width: 44px;[\s\S]*?height: 44px;/,
		);
	});

	it("styles destructive items distinctly", () => {
		expect(rule(".action-menu-item.destructive")).toContain("color: var(--destructive)");
		expect(rule('.action-menu-item.destructive[data-highlighted]')).toContain("var(--destructive)");
	});
});
