import { readFileSync } from "node:fs";
import { resolve } from "node:path";
import { describe, expect, it } from "vitest";

const WCAG_AA_NORMAL_TEXT = 4.5;
const foregroundTokens = ["muted-foreground", "warning", "success"] as const;
const surfaceTokens = ["background", "card", "muted"] as const;

const styles = readFileSync(resolve(import.meta.dirname, "styles.css"), "utf8");
const lightTheme = lightThemeBlock(styles);

describe("light theme semantic color contrast", () => {
	for (const foregroundToken of foregroundTokens) {
		it(`${foregroundToken} meets WCAG AA on common light surfaces`, () => {
			const foreground = hexToken(lightTheme, foregroundToken);

			for (const surfaceToken of surfaceTokens) {
				const surface = hexToken(lightTheme, surfaceToken);
				expect(
					contrastRatio(foreground, surface),
					`--${foregroundToken} on --${surfaceToken}`,
				).toBeGreaterThanOrEqual(WCAG_AA_NORMAL_TEXT);
			}
		});
	}
});

function lightThemeBlock(css: string): string {
	const matches = [...css.matchAll(/^:root\s*\{([^}]*)\}/gm)];
	if (matches.length !== 1 || !matches[0][1]) {
		throw new Error(`Expected exactly one plain :root light theme block, found ${matches.length}`);
	}
	return matches[0][1];
}

function hexToken(theme: string, name: string): string {
	const declarations = [
		...theme.matchAll(new RegExp(`^\\s*--${name}:\\s*([^;]+);`, "gm")),
	].map((match) => match[1]?.trim());

	if (declarations.length !== 1) {
		throw new Error(`Expected exactly one --${name} declaration, found ${declarations.length}`);
	}

	const value = declarations[0];
	if (!value || !/^#[0-9a-f]{6}$/i.test(value)) {
		throw new Error(`Expected --${name} to be a six-digit hex color, received ${String(value)}`);
	}
	return value;
}

function contrastRatio(first: string, second: string): number {
	const [lighter, darker] = [relativeLuminance(first), relativeLuminance(second)].sort(
		(a, b) => b - a,
	);
	return (lighter + 0.05) / (darker + 0.05);
}

function relativeLuminance(hex: string): number {
	const [red, green, blue] = hex
		.slice(1)
		.match(/.{2}/g)!
		.map((channel) => linearize(Number.parseInt(channel, 16) / 255));
	return 0.2126 * red + 0.7152 * green + 0.0722 * blue;
}

function linearize(channel: number): number {
	return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4;
}
