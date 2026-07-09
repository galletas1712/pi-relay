import { readdir, readFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const SRC_ROOT = fileURLToPath(new URL("../src", import.meta.url));
const WEB_DOCS_ROOT = fileURLToPath(new URL("../docs", import.meta.url));
const RUST_ROOT = fileURLToPath(new URL("../../../rust", import.meta.url));
const ACTIVE_SOURCE = /\.(ts|tsx)$/;
const GUARDED_SURFACE = /\.(ts|tsx|css|md|rs)$/;
const TEST_SOURCE = /\.test\.(ts|tsx)$/;

async function filesMatching(
	dir: string,
	include: RegExp,
	exclude: RegExp | null = null,
): Promise<string[]> {
	const entries = await readdir(dir, { withFileTypes: true });
	const files = await Promise.all(
		entries.map(async (entry) => {
			const path = join(dir, entry.name);
			if (entry.isDirectory()) return filesMatching(path, include, exclude);
			if (!entry.isFile()) return [];
			if (!include.test(entry.name) || exclude?.test(entry.name)) return [];
			return [path];
		}),
	);
	return files.flat();
}

const activeSourceFiles = (dir: string) => filesMatching(dir, ACTIVE_SOURCE, TEST_SOURCE);

describe("active frontend subagent surface", () => {
	it("does not call the retired subagent.list UI/RPC path", async () => {
		const forbidden = [/subagent\.list/, /\blistSubagents\b/, /\bSubagentsSection\b/, /\btaskBySessionId\b/];
		const matches: string[] = [];

		for (const file of await activeSourceFiles(SRC_ROOT)) {
			const text = await readFile(file, "utf8");
			for (const pattern of forbidden) {
				if (pattern.test(text)) {
					matches.push(`${relative(SRC_ROOT, file)} matches ${pattern}`);
				}
			}
		}

		expect(matches).toEqual([]);
	});

	it("does not expose delegated-work restart helpers, callbacks, or product copy", async () => {
		const forbidden: { label: string; pattern: RegExp }[] = [
			{ label: "retired capability helper", pattern: /\bcanReRunDelegation\b/ },
			{ label: "retired parameter helper", pattern: /\breRunParamsForDelegation\b/ },
			{ label: "retired callback", pattern: /\bonReRunDelegation\b/ },
			{ label: "retired handler", pattern: /\breRunDelegation\b/ },
			{ label: "retired prompt-file helper", pattern: /\bsubagentHasNonEmptyPromptFile\b/ },
			{ label: "delegation restart API identifier", pattern: /\b(?:rerun|restart)_delegation\b/i },
			{ label: "delegation restart API identifier", pattern: /\bdelegation_(?:rerun|restart)\b/i },
			{ label: "delegation restart RPC", pattern: /\bdelegation\.(?:rerun|restart)\b/i },
			{
				label: "delegated-work restart product copy",
				pattern: /\b(?:re-?run|restart)(?: this)? delegated work\b/i,
			},
			{
				label: "positive delegated-work restart prescription",
				pattern: /\b(?:add|offer|show|expose|implement|support)\b.{0,60}\b(?:re-?run|rerun|restart)\b.{0,30}\b(?:delegation|delegated work)\b/i,
			},
		];
		const matches: string[] = [];
		// Guard shipped product code (including CSS), product docs/roadmap, Rust
		// APIs, tool implementation comments, and Rust docs. Tests are excluded so
		// they can name the retired behavior they prove absent. Patterns are
		// delegation-qualified so terminal-turn Retry and workflow loops remain valid.
		const guardedFiles = [
			...await filesMatching(SRC_ROOT, GUARDED_SURFACE, TEST_SOURCE),
			...await filesMatching(WEB_DOCS_ROOT, /\.md$/),
			...await filesMatching(join(RUST_ROOT, "crates"), /\.rs$/),
			...await filesMatching(join(RUST_ROOT, "docs"), /\.md$/),
		];

		for (const file of guardedFiles) {
			const text = await readFile(file, "utf8");
			for (const { label, pattern } of forbidden) {
				if (pattern.test(text)) matches.push(`${relative(SRC_ROOT, file)}: ${label}`);
			}
		}

		expect(matches).toEqual([]);
	});
});
