import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { GitComparisonList, GitComparisonSummary } from "./gitComparison.tsx";
import type { GitComparison } from "./types.ts";

const comparison: GitComparison = {
	base: {
		branch: "feat/base",
		oid: "base-oid",
		pull_request: {
			number: 370,
			title: "Base files work",
			url: "https://github.com/example/repo/pull/370",
		},
	},
	tip: {
		branch: "feat/tip",
		oid: "tip-oid",
		pull_request: {
			number: 374,
			title: "Add git file views",
			url: "https://github.com/example/repo/pull/374",
		},
	},
	merge_base_oid: "base-oid",
};

describe("GitComparisonSummary", () => {
	it("shows branch names and linked PR names for a stack", () => {
		const html = renderToStaticMarkup(<GitComparisonSummary comparison={comparison} />);

		expect(html).toContain("feat/base");
		expect(html).toContain("#370 Base files work");
		expect(html).toContain("feat/tip");
		expect(html).toContain("#374 Add git file views");
		expect(html).toContain("https://github.com/example/repo/pull/374");
	});

	it("falls back to branch names without PR metadata", () => {
		const html = renderToStaticMarkup(
			<GitComparisonSummary
				comparison={{
					base: { branch: "main", oid: "base" },
					tip: { branch: "local-work", oid: "tip" },
					merge_base_oid: "base",
				}}
			/>,
		);

		expect(html).toContain("main");
		expect(html).toContain("local-work");
		expect(html).not.toContain("<a");
	});
});

describe("GitComparisonList", () => {
	it("labels each repository independently", () => {
		const html = renderToStaticMarkup(
			<GitComparisonList
				roots={[
					{ workspace_dir: "repo-a", comparison, entries: [] },
					{
						workspace_dir: "repo-b",
						comparison: {
							base: { branch: "release", oid: "release-oid" },
							tip: { branch: "fix/sidebar", oid: "fix-oid" },
							merge_base_oid: "release-oid",
						},
						entries: [],
					},
				]}
			/>,
		);

		expect(html).toContain("repo-a");
		expect(html).toContain("repo-b");
		expect(html).toContain("release");
		expect(html).toContain("fix/sidebar");
	});
});
