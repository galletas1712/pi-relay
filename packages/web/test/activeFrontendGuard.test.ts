import { readdir, readFile } from "node:fs/promises";
import { join, relative } from "node:path";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";

const SRC_ROOT = fileURLToPath(new URL("../src", import.meta.url));
const ACTIVE_SOURCE = /\.(ts|tsx)$/;
const TEST_SOURCE = /\.test\.(ts|tsx)$/;

async function activeSourceFiles(dir: string): Promise<string[]> {
	const entries = await readdir(dir, { withFileTypes: true });
	const files = await Promise.all(
		entries.map(async (entry) => {
			const path = join(dir, entry.name);
			if (entry.isDirectory()) return activeSourceFiles(path);
			if (!entry.isFile()) return [];
			if (!ACTIVE_SOURCE.test(entry.name) || TEST_SOURCE.test(entry.name)) return [];
			return [path];
		}),
	);
	return files.flat();
}

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
});
