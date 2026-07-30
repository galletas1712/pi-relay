import { useQueries, useQuery } from "@tanstack/react-query";
import { fetchGitHubViewer, fetchOpenPullRequests, type GitHubPullRequest } from "./githubApi.ts";
import { githubRepoLabel, parseGithubRemoteUrl } from "./parseRemote.ts";
import type { SessionWorkspace } from "../types.ts";

export const GITHUB_REFRESH_MS = 60_000;

export interface GitWorkspaceRepo {
	workspace: SessionWorkspace;
	owner: string;
	repo: string;
	label: string;
}

export function gitWorkspaceRepos(workspaces: SessionWorkspace[]): GitWorkspaceRepo[] {
	const repos: GitWorkspaceRepo[] = [];
	for (const workspace of workspaces) {
		if (workspace.kind === "local" || !workspace.remote_url?.trim()) continue;
		const parsed = parseGithubRemoteUrl(workspace.remote_url);
		if (!parsed) continue;
		repos.push({
			workspace,
			owner: parsed.owner,
			repo: parsed.repo,
			label: githubRepoLabel(parsed.owner, parsed.repo),
		});
	}
	return repos;
}

export function useGitHubViewer(enabled: boolean) {
	return useQuery({
		queryKey: ["github", "viewer"],
		queryFn: fetchGitHubViewer,
		enabled,
		staleTime: GITHUB_REFRESH_MS,
		refetchInterval: enabled ? GITHUB_REFRESH_MS : false,
	});
}

export function useWorkspacePullRequests(repos: GitWorkspaceRepo[], enabled: boolean) {
	return useQueries({
		queries: repos.map((entry) => ({
			queryKey: ["github", "pulls", entry.owner, entry.repo],
			queryFn: () => fetchOpenPullRequests(entry.owner, entry.repo),
			enabled,
			staleTime: GITHUB_REFRESH_MS,
			refetchInterval: enabled ? GITHUB_REFRESH_MS : false,
		})),
	});
}

export interface WorkspacePullRequestBundle {
	repo: GitWorkspaceRepo;
	pulls: GitHubPullRequest[];
	loading: boolean;
	error: string | null;
}

export function workspacePullRequestBundles(
	repos: GitWorkspaceRepo[],
	queries: ReturnType<typeof useWorkspacePullRequests>,
): WorkspacePullRequestBundle[] {
	return repos.map((repo, index) => {
		const query = queries[index];
		return {
			repo,
			pulls: query.data ?? [],
			loading: query.isLoading || query.isFetching,
			error: query.error instanceof Error ? query.error.message : query.error ? String(query.error) : null,
		};
	});
}
