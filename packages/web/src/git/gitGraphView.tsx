import type { GitHubPullRequest } from "../github/githubApi.ts";
import type { GitWorkspaceRepo } from "../github/useGitHubPullRequests.ts";

export function GitGraphView({
	repos,
	activeRepo,
	pulls,
	selectedPull,
	onSelectRepo,
}: {
	repos: GitWorkspaceRepo[];
	activeRepo: GitWorkspaceRepo | null;
	pulls: GitHubPullRequest[];
	selectedPull: GitHubPullRequest | null;
	onSelectRepo: (workspaceDir: string) => void;
}) {
	if (repos.length === 0) {
		return (
			<div className="git-graph git-graph-empty">
				<p className="muted">No git repositories available for graph view.</p>
			</div>
		);
	}

	return (
		<div className="git-graph" data-slot="git-graph">
			<div className="git-graph-toolbar">
				<div className="git-graph-repo-tabs" role="tablist" aria-label="Repository graph scope">
					{repos.map((repo) => (
						<button
							key={repo.workspace.workspace_dir}
							className={`git-graph-repo-tab ${activeRepo?.workspace.workspace_dir === repo.workspace.workspace_dir ? "active" : ""}`}
							type="button"
							role="tab"
							aria-selected={activeRepo?.workspace.workspace_dir === repo.workspace.workspace_dir}
							onClick={() => onSelectRepo(repo.workspace.workspace_dir)}
						>
							{repo.workspace.workspace_dir}
						</button>
					))}
				</div>
			</div>
			<div className="git-graph-canvas" aria-label="Commit graph">
				{selectedPull ? (
					<div className="git-graph-selection">
						<p>
							Highlighting lineage for <strong>#{selectedPull.number}</strong>{" "}
							<span className="muted">{selectedPull.head.ref}</span>
						</p>
						<div className="git-graph-lineage">
							<div className="git-graph-node highlighted">merge-base</div>
							<div className="git-graph-edge highlighted" />
							<div className="git-graph-node highlighted">{selectedPull.head.ref}</div>
						</div>
						<p className="muted git-graph-placeholder">
							Full commit graph rendering will use materialized workspace git history.
						</p>
					</div>
				) : (
					<p className="muted git-graph-placeholder">Select a pull request to highlight its lineage on the graph.</p>
				)}
				{pulls.length > 0 ? (
					<ul className="git-graph-pr-index muted">
						{pulls.slice(0, 8).map((pull) => (
							<li key={pull.number} className={selectedPull?.number === pull.number ? "highlighted" : "dimmed"}>
								#{pull.number} {pull.head.ref}
							</li>
						))}
					</ul>
				) : null}
			</div>
		</div>
	);
}
