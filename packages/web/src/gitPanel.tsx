import {
	AlertTriangle,
	ExternalLink,
	GitBranch,
	GitCommitHorizontal,
	GitMerge,
	Loader2,
	RefreshCw,
} from "lucide-react";
import type {
	GitCommit,
	GitStatusResponse,
	GitWorkspaceStatus,
} from "./types.ts";

export interface GitPanelProps {
	status: GitStatusResponse | null;
	loading: boolean;
	fetching?: boolean;
	error: string | null;
	unavailableReason?: string | null;
	onRetry?: () => void;
	onLoadMore?: () => void;
}

function safePullRequestUrl(value: string): string | undefined {
	try {
		const url = new URL(value);
		return url.protocol === "https:" ? url.href : undefined;
	} catch {
		return undefined;
	}
}

export function GitPanel({
	status,
	loading,
	fetching = false,
	error,
	unavailableReason = null,
	onRetry,
	onLoadMore,
}: GitPanelProps) {
	const hasMore = status?.workspaces.some((workspace) => workspace.has_more) ?? false;
	return (
		<section className="git-panel" aria-label="Git repositories">
			<div className="git-panel-toolbar">
				<span>Workspace repositories</span>
				<button
					className="plain-close-button git-refresh"
					type="button"
					onClick={onRetry}
					disabled={fetching || !onRetry}
					aria-label="Refresh Git status"
					title="Refresh Git status"
				>
					<RefreshCw className={fetching ? "spin" : undefined} size={14} aria-hidden />
				</button>
			</div>
			{loading && !status ? (
				<div className="git-panel-state" role="status">
					<Loader2 className="spin" size={16} aria-hidden />
					<span>Loading repositories…</span>
				</div>
			) : null}
			{error && !status ? (
				<div className="git-panel-state error" role="alert">
					<AlertTriangle size={16} aria-hidden />
					<span>Couldn’t load Git status. {error}</span>
					{onRetry ? (
						<button className="secondary-button compact-button" type="button" onClick={onRetry}>
							Retry
						</button>
					) : null}
				</div>
			) : null}
			{error && status ? (
				<div className="git-panel-state stale" role="status">
					<AlertTriangle size={16} aria-hidden />
					<span>Showing saved Git data. Refresh failed: {error}</span>
				</div>
			) : null}
			{!loading && !error && !status ? (
				<p className="git-panel-empty">
					{unavailableReason ?? "Git status is unavailable."}
				</p>
			) : null}
			{status && status.workspaces.length === 0 ? (
				<p className="git-panel-empty">This session has no workspace directories.</p>
			) : null}
			{status?.workspaces_truncated ? (
				<p className="git-panel-state stale" role="status">
					Only the first repositories are shown because this session has too many workspaces.
				</p>
			) : null}
			{status?.workspaces.map((workspace) => (
				<article className="git-repository" key={workspace.workspace_dir}>
					<header className="git-repository-header">
						<div className="git-repository-title">
							<GitBranch size={16} aria-hidden />
							<strong>{workspace.workspace_dir}</strong>
						</div>
						<span className={`git-repository-status ${workspace.status}`}>
							{repositoryStatusLabel(workspace.status)}
						</span>
					</header>
					{workspace.status === "ready" ? (
						<>
							<div className="git-repository-meta">
								<div className="kv">
									<span>branch</span>
									<strong title={workspace.branch ?? workspace.head_sha}>
										{workspace.detached
											? `detached @ ${shortSha(workspace.head_sha)}`
											: workspace.unborn
												? `unborn ${workspace.branch ?? "branch"}`
												: workspace.branch ?? "unknown"}
									</strong>
								</div>
								<div className="kv">
									<span>origin</span>
									<strong title={workspace.remote_url}>
										{workspace.remote_url ? compactRemote(workspace.remote_url) : "none"}
									</strong>
								</div>
							</div>
							<PullRequestSummary workspace={workspace} />
							<div className="git-history-heading">Recent commits</div>
							{workspace.commits.length ? (
								<ol className="git-commit-list">
									{workspace.commits.map((commit) => (
										<GitCommitRow commit={commit} key={commit.sha} />
									))}
								</ol>
							) : (
								<p className="git-panel-empty compact">
									{workspace.error ?? "No commits yet."}
								</p>
							)}
						</>
					) : (
						<p className="git-panel-empty compact">
							{workspace.status === "not_git"
								? "This workspace is not a Git repository."
								: workspace.error ?? "This workspace is unavailable."}
						</p>
					)}
				</article>
			))}
			{status && hasMore ? (
				<button
					className="secondary-button git-load-more"
					type="button"
					onClick={onLoadMore}
					disabled={fetching || status.limit >= 100}
				>
					{fetching ? "Loading history…" : status.limit >= 100 ? "History limit reached" : "Load more history"}
				</button>
			) : null}
		</section>
	);
}

function PullRequestSummary({
	workspace,
}: {
	workspace: GitStatusResponse["workspaces"][number];
}) {
	const pullRequest = workspace.pull_request;
	if (pullRequest) {
		const href = safePullRequestUrl(pullRequest.url);
		if (!href) {
			return <p className="git-pr-note">Pull request link is unavailable.</p>;
		}
		return (
			<a
				className="git-pull-request"
				href={href}
				target="_blank"
				rel="noreferrer"
				title={`Open pull request #${pullRequest.number}`}
			>
				<span className="git-pull-request-number">PR #{pullRequest.number}</span>
				<span className="git-pull-request-title">{pullRequest.title}</span>
				<span className="git-pull-request-state">
					{pullRequest.is_draft ? "draft" : pullRequest.state}
				</span>
				<ExternalLink size={13} aria-hidden />
			</a>
		);
	}
	if (workspace.pull_request_lookup === "unavailable") {
		return <p className="git-pr-note">Pull request lookup unavailable.</p>;
	}
	if (workspace.pull_request_lookup === "none") {
		return <p className="git-pr-note">No pull request found for this branch.</p>;
	}
	if (!workspace.remote_url) {
		return <p className="git-pr-note">No origin remote configured.</p>;
	}
	return null;
}

function GitCommitRow({ commit }: { commit: GitCommit }) {
	const CommitIcon = commit.parents.length > 1 ? GitMerge : GitCommitHorizontal;
	const authoredAt = formatAuthoredAt(commit.authored_at);
	return (
		<li className="git-commit-row">
			<span className="git-graph" aria-hidden>
				<CommitIcon size={15} />
			</span>
			<details className="git-commit-details">
				<summary>
					<span className="git-commit-summary">{commit.summary || "(no commit message)"}</span>
					<span className="git-commit-caption">
						<code>{shortSha(commit.sha)}</code>
						<span>{commit.author_name}</span>
						<time dateTime={commit.authored_at}>{authoredAt}</time>
					</span>
				</summary>
				<dl className="git-lineage">
					<div>
						<dt>commit</dt>
						<dd><code>{commit.sha}</code></dd>
					</div>
					<div>
						<dt>{commit.parents.length === 1 ? "parent" : "parents"}</dt>
						<dd>
							{commit.parents.length
								? commit.parents.map((parent) => <code key={parent}>{parent}</code>)
								: <span>root commit</span>}
						</dd>
					</div>
					<div>
						<dt>author</dt>
						<dd>{commit.author_name}</dd>
					</div>
					<div>
						<dt>authored</dt>
						<dd><time dateTime={commit.authored_at}>{authoredAt}</time></dd>
					</div>
				</dl>
			</details>
		</li>
	);
}

function repositoryStatusLabel(status: GitWorkspaceStatus): string {
	switch (status) {
		case "ready": return "Git";
		case "not_git": return "Not Git";
		case "unavailable": return "Unavailable";
	}
}

function shortSha(sha: string | null | undefined): string {
	return sha?.slice(0, 8) ?? "unknown";
}

function compactRemote(remote: string): string {
	const scpMatch = remote.match(/^[^@]+@([^:]+):(.+?)(?:\.git)?$/);
	if (scpMatch) return `${scpMatch[1]}/${scpMatch[2].replace(/\.git$/, "")}`;
	try {
		const url = new URL(remote);
		return `${url.host}${url.pathname.replace(/\.git$/, "")}`;
	} catch {
		return remote;
	}
}

function formatAuthoredAt(value: string): string {
	const timestamp = new Date(value);
	if (!Number.isFinite(timestamp.getTime())) return value;
	return timestamp.toLocaleString([], {
		year: "numeric",
		month: "short",
		day: "numeric",
		hour: "numeric",
		minute: "2-digit",
	});
}
