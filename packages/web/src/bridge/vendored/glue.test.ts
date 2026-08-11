// Smoke tests for the vendored T3 glue + @pierre/diffs parse path against the
// mock workspace fixture. The UI consumes exactly these helpers.
import { describe, expect, it } from "vitest";
import { MOCK_FIXTURE_DIFF } from "../workspace.ts";
import {
	buildFileDiffRenderKey,
	getDiffLineStat,
	getRenderablePatch,
	resolveDiffThemeName,
	resolveFileDiffPath,
} from "./diffRendering.ts";
import { areAllDiffFilesCollapsed, toggleAllDiffFiles } from "./diffCollapse.ts";
import { buildTurnDiffTree, summarizeTurnDiffStats, type TurnDiffTreeNode } from "./turnDiffTree.ts";

function flatten(nodes: TurnDiffTreeNode[]): TurnDiffTreeNode[] {
	return nodes.flatMap((n) => (n.kind === "directory" ? [n, ...flatten(n.children)] : [n]));
}

describe("vendored glue on the mock fixture diff", () => {
	const renderable = getRenderablePatch(MOCK_FIXTURE_DIFF, "test");

	it("parses the 3-file patch into FileDiffMetadata", () => {
		expect(renderable?.kind).toBe("files");
		if (renderable?.kind !== "files") return;
		expect(renderable.files).toHaveLength(3);
		const paths = renderable.files.map(resolveFileDiffPath);
		expect(paths).toContain("src/agent/planner.ts");
		expect(paths).toContain("src/agent/tools/ipython.ts");
		expect(paths).toContain("docs/bridge-contract.md");
	});

	it("counts additions/deletions via getDiffLineStat", () => {
		if (renderable?.kind !== "files") throw new Error("expected files");
		const stat = getDiffLineStat(renderable.files);
		expect(stat.additions).toBeGreaterThan(10);
		expect(stat.deletions).toBeGreaterThan(3);
	});

	it("groups files into a diff tree with rolled-up stats", () => {
		if (renderable?.kind !== "files") throw new Error("expected files");
		const changes = renderable.files.map((f) => {
			const s = getDiffLineStat([f]);
			return { path: resolveFileDiffPath(f), additions: s.additions, deletions: s.deletions };
		});
		const tree = buildTurnDiffTree(changes);
		const flat = flatten(tree);
		expect(flat.filter((n) => n.kind === "file")).toHaveLength(3);
		const summary = summarizeTurnDiffStats(changes);
		expect(summary.additions).toBeGreaterThan(10);
		expect(summary.additions + summary.deletions).toBeGreaterThan(0);
	});

	it("collapses/expands all files via diffCollapse keys", () => {
		if (renderable?.kind !== "files") throw new Error("expected files");
		const keys = renderable.files.map(buildFileDiffRenderKey);
		expect(areAllDiffFilesCollapsed(keys, new Set())).toBe(false);
		const collapsed = new Set(toggleAllDiffFiles(keys, new Set()));
		expect(areAllDiffFilesCollapsed(keys, collapsed)).toBe(true);
		const expanded = new Set(toggleAllDiffFiles(keys, collapsed));
		expect(expanded.size).toBe(0);
	});

	it("resolves a pierre theme name for light/dark", () => {
		expect(resolveDiffThemeName("light")).toBeTruthy();
		expect(resolveDiffThemeName("dark")).toBeTruthy();
		expect(resolveDiffThemeName("dark")).not.toBe(resolveDiffThemeName("light"));
	});
});
