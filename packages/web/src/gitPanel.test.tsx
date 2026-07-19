import { renderToStaticMarkup } from "react-dom/server";
import { describe, expect, it } from "vitest";
import { GitPanel } from "./gitPanel.tsx";
import type { GitStatusResponse } from "./types.ts";

function status(): GitStatusResponse {
	return {
		session_id: "session-1",
		limit: 12,
		workspaces_truncated: false,
		workspaces: [
			{
				workspace_dir: "app",
				kind: "git",
				status: "ready",
				branch: "feature/git-view",
				detached: false,
				unborn: false,
				head_sha: "abcdef0123456789",
				remote_url: "git@github.com:example/app.git",
				pull_request: {
					number: 42,
					title: "Add Git browser",
					url: "https://github.com/example/app/pull/42",
					state: "open",
					is_draft: false,
					head_ref_name: "feature/git-view",
				},
				pull_request_lookup: "found",
				has_more: true,
				commits: [
					{
						sha: "abcdef0123456789",
						parents: ["1111111111111111", "2222222222222222"],
						author_name: "Ada",
						authored_at: "2024-01-02T03:04:05Z",
						summary: "Merge repository view",
					},
				],
			},
			{
				workspace_dir: "notes",
				kind: "local",
				status: "not_git",
				detached: false,
				unborn: false,
				pull_request: null,
				pull_request_lookup: "not_applicable",
				commits: [],
				has_more: false,
			},
		],
	};
}

describe("GitPanel", () => {
	it("shows repositories, branch, PR, commit lineage, and bounded expansion", () => {
		const html = renderToStaticMarkup(
			<GitPanel
				status={status()}
				loading={false}
				error={null}
				onLoadMore={() => {}}
				onRetry={() => {}}
			/>,
		);

		expect(html).toContain("feature/git-view");
		expect(html).toContain("PR #42");
		expect(html).toContain("Add Git browser");
		expect(html).toContain("Merge repository view");
		expect(html).toContain("<details");
		expect(html).toContain("abcdef0123456789");
		expect(html).toContain("1111111111111111");
		expect(html).toContain("2222222222222222");
		expect(html).toContain("Load more history");
		expect(html).toContain("This workspace is not a Git repository.");
	});

	it("renders detached, unavailable, loading, empty, and error states", () => {
		const detached = status();
		detached.workspaces = [{
			...detached.workspaces[0],
			branch: undefined,
			detached: true,
			pull_request: null,
			pull_request_lookup: "unavailable",
			has_more: false,
		}];
		const detachedHtml = renderToStaticMarkup(
			<GitPanel status={detached} loading={false} error={null} />,
		);
		expect(detachedHtml).toContain("detached @ abcdef01");
		expect(detachedHtml).toContain("Pull request lookup unavailable.");

		const loading = renderToStaticMarkup(
			<GitPanel status={null} loading error={null} />,
		);
		expect(loading).toContain("Loading repositories");

		const empty = renderToStaticMarkup(
			<GitPanel
				status={{
					session_id: "session-1",
					limit: 12,
					workspaces: [],
					workspaces_truncated: false,
				}}
				loading={false}
				error={null}
			/>,
		);
		expect(empty).toContain("This session has no workspace directories.");

		const error = renderToStaticMarkup(
			<GitPanel status={null} loading={false} error="offline" onRetry={() => {}} />,
		);
		expect(error).toContain("Couldn’t load Git status. offline");
		expect(error).toContain("Retry");

		const stale = renderToStaticMarkup(
			<GitPanel status={status()} loading={false} error="refresh offline" />,
		);
		expect(stale).toContain("Showing saved Git data");
		expect(stale).toContain("refresh offline");

		const disabled = renderToStaticMarkup(
			<GitPanel
				status={null}
				loading={false}
				error={null}
				unavailableReason="Conversation route is loading."
			/>,
		);
		expect(disabled).toContain("Conversation route is loading.");
		expect(disabled).not.toContain("Loading repositories");
	});
});
