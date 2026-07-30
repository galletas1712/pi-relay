import { useMemo, useState, type ReactNode } from "react";
import {
	filterPullRequests,
	type GitPullRequestFilter,
	pullRequestInvolvesSessionBranch,
	uniqueAuthorLogins,
} from "../github/pullRequestFilters.ts";
import type { GitHubPullRequest } from "../github/githubApi.ts";
import type { GitWorkspaceRepo, WorkspacePullRequestBundle } from "../github/useGitHubPullRequests.ts";
import type { SessionWorkspace } from "../types.ts";

export function GitPrSidebar({
	bundles,
	viewerLogin,
	sessionWorkspaces,
	selectedWorkspaceDir,
	selectedPrNumber,
	authError,
	onSelectPull,
	onOpenGraph,
}: {
	bundles: WorkspacePullRequestBundle[];
	viewerLogin: string | null;
	sessionWorkspaces: SessionWorkspace[];
	selectedWorkspaceDir: string | null;
	selectedPrNumber: number | null;
	authError: string | null;
	onSelectPull: (workspaceDir: string, number: number) => void;
	onOpenGraph?: () => void;
}) {
	const [filter, setFilter] = useState<GitPullRequestFilter>("mine");
	const [authorLogin, setAuthorLogin] = useState<string>("");

	const allPulls = useMemo(() => bundles.flatMap((bundle) => bundle.pulls), [bundles]);
	const authors = useMemo(() => uniqueAuthorLogins(allPulls), [allPulls]);
	const sessionWorkspaceByDir = useMemo(() => {
		const map = new Map<string, SessionWorkspace>();
		for (const workspace of sessionWorkspaces) map.set(workspace.workspace_dir, workspace);
		return map;
	}, [sessionWorkspaces]);

	if (bundles.length === 0) {
		return (
			<div className="git-sidebar git-sidebar-empty">
				<p className="muted">This project has no GitHub-backed git workspaces.</p>
			</div>
		);
	}

	return (
		<div className="git-sidebar" data-slot="git-sidebar">
			<div className="git-sidebar-toolbar">
				<div className="git-view-filters" role="group" aria-label="Pull request filters">
					<FilterButton active={filter === "mine"} onClick={() => setFilter("mine")}>
						Mine
					</FilterButton>
					<FilterButton active={filter === "all"} onClick={() => setFilter("all")}>
						All open
					</FilterButton>
					<FilterButton active={filter === "session-branch"} onClick={() => setFilter("session-branch")}>
						Session branch
					</FilterButton>
				</div>
				<label className="git-view-author-filter">
					<span className="sr-only">Author</span>
					<select
						value={authorLogin}
						onChange={(event) => setAuthorLogin(event.target.value)}
						aria-label="Filter by author"
					>
						<option value="">All authors</option>
						{authors.map((login) => (
							<option key={login} value={login}>
								{login}
							</option>
						))}
					</select>
				</label>
				{onOpenGraph ? (
					<button className="secondary-button git-sidebar-graph-button" type="button" onClick={onOpenGraph}>
						See Git Graph
					</button>
				) : null}
			</div>
			{authError ? (
				<div className="git-view-banner" role="alert">
					<strong>GitHub unavailable</strong>
					<span>{authError}</span>
				</div>
			) : null}
			<div className="git-sidebar-repos">
				{bundles.map((bundle) => (
					<GitRepoSection
						key={bundle.repo.workspace.workspace_dir}
						bundle={bundle}
						filter={filter}
						authorLogin={authorLogin || null}
						viewerLogin={viewerLogin}
						sessionWorkspace={sessionWorkspaceByDir.get(bundle.repo.workspace.workspace_dir) ?? null}
						selectedWorkspaceDir={selectedWorkspaceDir}
						selectedPrNumber={selectedPrNumber}
						onSelectPull={onSelectPull}
					/>
				))}
			</div>
		</div>
	);
}

function GitRepoSection({
	bundle,
	filter,
	authorLogin,
	viewerLogin,
	sessionWorkspace,
	selectedWorkspaceDir,
	selectedPrNumber,
	onSelectPull,
}: {
	bundle: WorkspacePullRequestBundle;
	filter: GitPullRequestFilter;
	authorLogin: string | null;
	viewerLogin: string | null;
	sessionWorkspace: SessionWorkspace | null;
	selectedWorkspaceDir: string | null;
	selectedPrNumber: number | null;
	onSelectPull: (workspaceDir: string, number: number) => void;
}) {
	const visiblePulls = filterPullRequests(bundle.pulls, {
		filter,
		viewerLogin,
		authorLogin,
		workspace: sessionWorkspace ?? bundle.repo.workspace,
	});

	return (
		<section className="git-view-repo">
			<header className="git-view-repo-head">
				<div>
					<h2>{bundle.repo.workspace.workspace_dir}</h2>
					<p className="git-view-repo-meta">
						<span>{bundle.repo.label}</span>
					</p>
				</div>
				{bundle.loading ? <span className="git-view-inline-status">Refreshing…</span> : null}
			</header>
			{bundle.error ? (
				<p className="git-view-repo-error" role="alert">
					{bundle.error}
				</p>
			) : null}
			{visiblePulls.length === 0 ? (
				<p className="muted git-view-empty-repo">
					{bundle.loading ? "Loading pull requests…" : "No matching open pull requests."}
				</p>
			) : (
				<ul className="git-view-pr-list">
					{visiblePulls.map((pull) => (
						<GitPrRow
							key={pull.number}
							pull={pull}
							repo={bundle.repo}
							selected={
								selectedWorkspaceDir === bundle.repo.workspace.workspace_dir &&
								selectedPrNumber === pull.number
							}
							sessionRelevant={
								!!sessionWorkspace && pullRequestInvolvesSessionBranch(pull, sessionWorkspace)
							}
							onSelect={() => onSelectPull(bundle.repo.workspace.workspace_dir, pull.number)}
						/>
					))}
				</ul>
			)}
		</section>
	);
}

function GitPrRow({
	pull,
	selected,
	sessionRelevant,
	onSelect,
}: {
	pull: GitHubPullRequest;
	repo: GitWorkspaceRepo;
	selected: boolean;
	sessionRelevant: boolean;
	onSelect: () => void;
}) {
	return (
		<li>
			<button
				className={`git-view-pr-row ${selected ? "selected" : ""} ${sessionRelevant ? "session-relevant" : ""}`}
				type="button"
				aria-pressed={selected}
				onClick={onSelect}
			>
				<span className="git-view-pr-number">#{pull.number}</span>
				<span className="git-view-pr-title">{pull.title}</span>
				<span className="git-view-pr-meta">
					{pull.user.login} · {pull.head.ref} → {pull.base.ref}
					{sessionRelevant ? " · session" : ""}
				</span>
			</button>
		</li>
	);
}

function FilterButton({
	active,
	onClick,
	children,
}: {
	active: boolean;
	onClick: () => void;
	children: ReactNode;
}) {
	return (
		<button className={`git-view-filter ${active ? "active" : ""}`} type="button" aria-pressed={active} onClick={onClick}>
			{children}
		</button>
	);
}
