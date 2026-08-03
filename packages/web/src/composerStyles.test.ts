import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const css = readFileSync(resolve(import.meta.dirname, "domain.css"), "utf8");

function responsiveComposerRule(maxWidth: number): string {
	const marker = `@media (max-width: ${maxWidth}px)`;
	const start = css.indexOf(marker);
	const end = css.indexOf("@media", start + marker.length);
	const media = css.slice(start, end < 0 ? undefined : end);
	return media.match(/\.composer-wrap\s*\{[^}]+\}/)?.[0] ?? "";
}

function gridTemplateColumns(rule: string): string {
	return rule.match(/grid-template-columns:\s*([^;]+);/)?.[1].trim() ?? "";
}

describe("responsive composer layout", () => {
	it("keeps one track for the textarea and each of the three in-flow controls", () => {
		expect(gridTemplateColumns(responsiveComposerRule(760))).toBe(
			"minmax(0, 1fr) 42px 42px 42px",
		);
		expect(gridTemplateColumns(responsiveComposerRule(430))).toBe(
			"minmax(0, 1fr) 40px 40px 40px",
		);
	});
});
