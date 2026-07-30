import { useMemo } from "react";
import type { GitHubPullRequest } from "../github/githubApi.ts";
import {
	projectWorkspaceRepos,
	useGitHubViewer,
	useWorkspacePullRequests,
	workspacePullRequestBundles,
	type GitWorkspaceRepo,
	type WorkspacePullRequestBundle,
} from "../github/useGitHubPullRequests.ts";
import type { ProjectWorkspace, SessionWorkspace } from "../types.ts";

export interface GitCenterState {
	repos: GitWorkspaceRepo[];
	bundles: WorkspacePullRequestBundle[];
	viewerLogin: string | null;
	authError: string | null;
	activeRepo: GitWorkspaceRepo | null;
	selectedPull: GitHubPullRequest | null;
	activeRepoPulls: GitHubPullRequest[];
}

export function useGitCenterState({
	projectWorkspaces,
	sessionWorkspaces,
	gitRepo,
	selectedPrNumber,
	enabled,
}: {
	projectWorkspaces: ProjectWorkspace[];
	sessionWorkspaces: SessionWorkspace[];
	gitRepo: string | null | undefined;
	selectedPrNumber: number | null | undefined;
	enabled: boolean;
}): GitCenterState {
	const repos = useMemo(() => projectWorkspaceRepos(projectWorkspaces), [projectWorkspaces]);
	const viewerQuery = useGitHubViewer(enabled && repos.length > 0);
	const pullQueries = useWorkspacePullRequests(repos, enabled && repos.length > 0);
	const bundles = useMemo(
		() => workspacePullRequestBundles(repos, pullQueries),
		[repos, pullQueries],
	);

	const sessionWorkspaceByDir = useMemo(() => {
		const map = new Map<string, SessionWorkspace>();
		for (const workspace of sessionWorkspaces) map.set(workspace.workspace_dir, workspace);
		return map;
	}, [sessionWorkspaces]);

	const activeRepo = useMemo(() => {
		if (gitRepo) {
			return repos.find((repo) => repo.workspace.workspace_dir === gitRepo) ?? repos[0] ?? null;
		}
		return repos[0] ?? null;
	}, [gitRepo, repos]);

	const activeBundle = useMemo(
		() => bundles.find((bundle) => bundle.repo.workspace.workspace_dir === activeRepo?.workspace.workspace_dir) ?? null,
		[bundles, activeRepo],
	);

	const selectedPull = useMemo(() => {
		if (!activeRepo || selectedPrNumber === undefined || selectedPrNumber === null) return null;
		const bundle =
			bundles.find((entry) => entry.repo.workspace.workspace_dir === activeRepo.workspace.workspace_dir) ?? null;
		return bundle?.pulls.find((pull) => pull.number === selectedPrNumber) ?? null;
	}, [activeRepo, bundles, selectedPrNumber]);

	const authError = viewerQuery.error instanceof Error ? viewerQuery.error.message : null;

	return {
		repos,
		bundles,
		viewerLogin: viewerQuery.data?.login ?? null,
		authError,
		activeRepo,
		selectedPull,
		activeRepoPulls: activeBundle?.pulls ?? [],
	};
}

export function sessionWorkspaceForRepo(
	sessionWorkspaces: SessionWorkspace[],
	repo: GitWorkspaceRepo | null,
): SessionWorkspace | null {
	if (!repo) return null;
	return sessionWorkspaces.find((workspace) => workspace.workspace_dir === repo.workspace.workspace_dir) ?? null;
}
