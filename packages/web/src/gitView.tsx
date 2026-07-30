import { useMemo, useState, type ReactNode } from "react";
import type { SessionSnapshot } from "./types.ts";
import {
	filterPullRequests,
	type GitPullRequestFilter,
	uniqueAuthorLogins,
} from "./github/pullRequestFilters.ts";
import {
	gitWorkspaceRepos,
	useGitHubViewer,
	useWorkspacePullRequests,
	workspacePullRequestBundles,
} from "./github/useGitHubPullRequests.ts";

export function GitView({ snapshot }: { snapshot: SessionSnapshot | null }) {
	const workspaces = snapshot?.workspaces ?? [];
	const repos = useMemo(() => gitWorkspaceRepos(workspaces), [workspaces]);
	const viewerQuery = useGitHubViewer(repos.length > 0);
	const pullQueries = useWorkspacePullRequests(repos, repos.length > 0);
	const bundles = useMemo(
		() => workspacePullRequestBundles(repos, pullQueries),
		[repos, pullQueries],
	);
	const [filter, setFilter] = useState<GitPullRequestFilter>("all");
	const [authorLogin, setAuthorLogin] = useState<string>("");

	const allPulls = useMemo(() => bundles.flatMap((bundle) => bundle.pulls), [bundles]);
	const authors = useMemo(() => uniqueAuthorLogins(allPulls), [allPulls]);
	const viewerLogin = viewerQuery.data?.login ?? null;

	if (!snapshot) {
		return (
			<div className="git-view git-view-empty">
				<p className="muted">Open a session to inspect GitHub pull requests for its workspaces.</p>
			</div>
		);
	}

	if (repos.length === 0) {
		return (
			<div className="git-view git-view-empty">
				<p className="muted">This session has no GitHub-backed git workspaces.</p>
			</div>
		);
	}

	const authError = viewerQuery.error instanceof Error ? viewerQuery.error.message : null;

	return (
		<div className="git-view" data-slot="git-view">
			<div className="git-view-toolbar">
				<div className="git-view-filters" role="group" aria-label="Pull request filters">
					<FilterButton active={filter === "all"} onClick={() => setFilter("all")}>
						All open
					</FilterButton>
					<FilterButton active={filter === "mine"} onClick={() => setFilter("mine")}>
						Mine
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
			</div>
			{authError ? (
				<div className="git-view-banner" role="alert">
					<strong>GitHub unavailable</strong>
					<span>{authError}</span>
					<span className="muted">Ensure `gh auth login` works where the GitHub proxy runs.</span>
				</div>
			) : null}
			<div className="git-view-repos">
				{bundles.map((bundle) => {
					const visiblePulls = filterPullRequests(bundle.pulls, {
						filter,
						viewerLogin,
						authorLogin: authorLogin || null,
						workspace: bundle.repo.workspace,
					});
					return (
						<section className="git-view-repo" key={bundle.repo.workspace.workspace_dir}>
							<header className="git-view-repo-head">
								<div>
									<h2>{bundle.repo.workspace.workspace_dir}</h2>
									<p className="git-view-repo-meta">
										<span>{bundle.repo.label}</span>
										{bundle.repo.workspace.local_branch ? (
											<code>{bundle.repo.workspace.local_branch}</code>
										) : null}
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
										<li key={pull.number}>
											<a className="git-view-pr-row" href={pull.html_url} target="_blank" rel="noreferrer">
												<span className="git-view-pr-number">#{pull.number}</span>
												<span className="git-view-pr-title">{pull.title}</span>
												<span className="git-view-pr-meta">
													{pull.user.login} · {pull.head.ref} → {pull.base.ref}
												</span>
											</a>
										</li>
									))}
								</ul>
							)}
						</section>
					);
				})}
			</div>
		</div>
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
