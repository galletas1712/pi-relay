import type { GitHubPullRequest } from "../github/githubApi.ts";
import type { GitWorkspaceRepo } from "../github/useGitHubPullRequests.ts";
import type { SessionWorkspace } from "../types.ts";
import { pullRequestInvolvesSessionBranch } from "../github/pullRequestFilters.ts";

export function PullRequestPanel({
	repo,
	pull,
	sessionWorkspace,
	loading,
	onOpenGraph,
}: {
	repo: GitWorkspaceRepo | null;
	pull: GitHubPullRequest | null;
	sessionWorkspace: SessionWorkspace | null;
	loading?: boolean;
	onOpenGraph?: () => void;
}) {
	if (loading) {
		return (
			<div className="git-pr-panel git-pr-panel-empty">
				<p className="muted">Loading pull request…</p>
			</div>
		);
	}

	if (!repo || !pull) {
		return (
			<div className="git-pr-panel git-pr-panel-empty">
				<p className="muted">Select a pull request to inspect checks, reviewers, and conversation.</p>
			</div>
		);
	}

	const sessionRelevant =
		sessionWorkspace && pullRequestInvolvesSessionBranch(pull, sessionWorkspace);

	return (
		<div className="git-pr-panel" data-slot="git-pr-panel">
			<header className="git-pr-panel-head">
				<div>
					<p className="git-pr-panel-repo">
						<span>{repo.workspace.workspace_dir}</span>
						<span className="muted">{repo.label}</span>
					</p>
					<h1 className="git-pr-panel-title">
						<span className="git-view-pr-number">#{pull.number}</span> {pull.title}
					</h1>
					<p className="git-pr-panel-meta">
						{pull.user.login} · {pull.head.ref} → {pull.base.ref}
						{sessionRelevant ? <span className="git-pr-session-tag">Session branch</span> : null}
					</p>
				</div>
				<div className="git-pr-panel-actions">
					{onOpenGraph ? (
						<button className="secondary-button" type="button" onClick={onOpenGraph}>
							See Git Graph
						</button>
					) : null}
					<a className="secondary-button" href={pull.html_url} target="_blank" rel="noreferrer">
						Open on GitHub
					</a>
				</div>
			</header>
			<section className="git-pr-panel-section">
				<h2>Checks</h2>
				<p className="muted">Status checks will appear here.</p>
			</section>
			<section className="git-pr-panel-section">
				<h2>Reviewers</h2>
				<p className="muted">Requested reviewers will appear here.</p>
			</section>
		</div>
	);
}
