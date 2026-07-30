import type { SessionWorkspace } from "../types.ts";
import type { GitHubPullRequest } from "./githubApi.ts";

export type GitPullRequestFilter = "all" | "session-branch" | "mine";

export function gitWorkspaceEntries(workspaces: SessionWorkspace[]): SessionWorkspace[] {
	return workspaces.filter(
		(workspace) => workspace.kind !== "local" && typeof workspace.remote_url === "string" && workspace.remote_url.trim(),
	);
}

export function pullRequestInvolvesSessionBranch(
	pull: GitHubPullRequest,
	workspace: SessionWorkspace,
): boolean {
	const sessionBranch = workspace.local_branch?.trim();
	if (!sessionBranch) return false;
	const headRef = pull.head.ref;
	if (headRef === sessionBranch) return true;
	if (sessionBranch.endsWith(`/${headRef}`)) return true;
	if (sessionBranch.endsWith(headRef)) return true;
	if (pull.base.ref === sessionBranch) return true;
	return false;
}

export function filterPullRequests(
	pulls: GitHubPullRequest[],
	options: {
		filter: GitPullRequestFilter;
		viewerLogin: string | null;
		authorLogin: string | null;
		workspace: SessionWorkspace;
	},
): GitHubPullRequest[] {
	let filtered = pulls;
	if (options.filter === "session-branch") {
		filtered = filtered.filter((pull) => pullRequestInvolvesSessionBranch(pull, options.workspace));
	} else if (options.filter === "mine") {
		if (!options.viewerLogin) return [];
		filtered = filtered.filter((pull) => pull.user.login === options.viewerLogin);
	}
	if (options.authorLogin) {
		filtered = filtered.filter((pull) => pull.user.login === options.authorLogin);
	}
	return filtered;
}

export function uniqueAuthorLogins(pulls: GitHubPullRequest[]): string[] {
	const authors = new Set<string>();
	for (const pull of pulls) {
		authors.add(pull.user.login);
	}
	return [...authors].sort((left, right) => left.localeCompare(right));
}
