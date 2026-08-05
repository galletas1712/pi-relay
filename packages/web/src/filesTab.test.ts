import { describe, expect, it } from "vitest";
import { GitStatusIndex, statusLetter } from "./gitStatus.ts";
import type { WorkspaceGitStatus } from "./types.ts";
import { parseUnifiedDiff } from "./unifiedDiff.ts";
import { visibleBrowseDirectories } from "./filesTab.tsx";

describe("visibleBrowseDirectories", () => {
	it("always includes the cwd root", () => {
		expect(visibleBrowseDirectories([])).toEqual([""]);
	});

	it("adds expanded folder paths only", () => {
		expect(visibleBrowseDirectories(["src", "src/util"])).toEqual(["", "src", "src/util"]);
	});

	it("ignores the synthetic root item id", () => {
		expect(visibleBrowseDirectories(["__root__", "docs"])).toEqual(["", "docs"]);
	});
});

describe("GitStatusIndex", () => {
	const report: WorkspaceGitStatus = {
		against: "head",
		roots: [
			{
				workspace_dir: "repo",
				entries: [
					{ path: "repo/src/deep/nested.rs", status: "modified" },
					{ path: "repo/new.txt", status: "untracked" },
					{ path: "repo/conflict.rs", status: "conflict" },
				],
			},
		],
	};

	it("bubbles nested file status to unloaded ancestors", () => {
		const index = new GitStatusIndex(report);
		expect(index.statusFor("repo/src/deep")).toBe("modified");
		expect(index.statusFor("repo/src")).toBe("modified");
		expect(index.statusFor("repo")).toBe("conflict");
		expect(index.get("repo/src/deep/nested.rs")).toBe("modified");
		expect(index.statusFor("other")).toBeNull();
	});

	it("maps status letters", () => {
		expect(statusLetter("modified")).toBe("M");
		expect(statusLetter("untracked")).toBe("?");
		expect(statusLetter("conflict")).toBe("U");
	});
});

describe("parseUnifiedDiff", () => {
	it("classifies add and remove lines", () => {
		const rows = parseUnifiedDiff(
			["diff --git a/f b/f", "--- a/f", "+++ b/f", "@@ -1 +1 @@", "-old", "+new", " context"].join(
				"\n",
			),
		);
		expect(rows.map((row) => row.kind)).toEqual([
			"meta",
			"meta",
			"meta",
			"hunk",
			"remove",
			"add",
			"context",
		]);
	});
});
