import { describe, expect, it } from "vitest";
import { filterPullRequests, pullRequestInvolvesSessionBranch } from "./pullRequestFilters.ts";
import type { GitHubPullRequest } from "./types.ts";
import type { SessionWorkspace } from "../types.ts";

const workspace: SessionWorkspace = {
	kind: "git",
	workspace_dir: "repo-a",
	remote_url: "https://github.com/org/repo-a.git",
	local_branch: "pi/session/s1/repo-a/feature/login",
};

function pull(overrides: Partial<GitHubPullRequest> = {}): GitHubPullRequest {
	return {
		number: 1,
		title: "Test",
		state: "open",
		html_url: "https://github.com/org/repo-a/pull/1",
		updated_at: "2026-01-01T00:00:00Z",
		user: { login: "alice" },
		head: { ref: "feature/login", label: "org:feature/login" },
		base: { ref: "main" },
		...overrides,
	};
}

describe("pullRequestInvolvesSessionBranch", () => {
	it("matches when the PR head ref is a suffix of the session branch", () => {
		expect(pullRequestInvolvesSessionBranch(pull(), workspace)).toBe(true);
	});

	it("matches exact branch names", () => {
		expect(
			pullRequestInvolvesSessionBranch(
				pull({ head: { ref: workspace.local_branch!, label: "x" } }),
				workspace,
			),
		).toBe(true);
	});
});

describe("filterPullRequests", () => {
	it("filters to the viewer's PRs", () => {
		const pulls = [pull({ user: { login: "alice" } }), pull({ number: 2, user: { login: "bob" } })];
		expect(
			filterPullRequests(pulls, {
				filter: "mine",
				viewerLogin: "alice",
				authorLogin: null,
				workspace,
			}),
		).toHaveLength(1);
	});

	it("filters by author independently of the quick filter", () => {
		const pulls = [pull({ user: { login: "alice" } }), pull({ number: 2, user: { login: "bob" } })];
		expect(
			filterPullRequests(pulls, {
				filter: "all",
				viewerLogin: "alice",
				authorLogin: "bob",
				workspace,
			}),
		).toHaveLength(1);
	});
});
