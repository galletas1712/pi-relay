import type { GitComparison, GitComparisonRef, GitStatusRoot } from "./types.ts";

export function GitComparisonList({ roots }: { roots: GitStatusRoot[] }) {
	const comparisons = roots.filter(
		(root): root is GitStatusRoot & { comparison: GitComparison } => !!root.comparison,
	);
	if (comparisons.length === 0) return null;
	return (
		<div className="files-git-comparisons" aria-label="Git branch comparisons">
			{comparisons.map((root) => (
				<div className="files-git-comparison" key={root.workspace_dir}>
					<span className="files-git-comparison-root">{root.workspace_dir}</span>
					<GitComparisonSummary comparison={root.comparison} />
				</div>
			))}
		</div>
	);
}

export function GitComparisonSummary({ comparison }: { comparison: GitComparison }) {
	return (
		<span className="git-comparison-summary" title={comparisonTitle(comparison)}>
			<ComparisonRef value={comparison.base} />
			<span className="git-comparison-arrow" aria-hidden>
				→
			</span>
			<ComparisonRef value={comparison.tip} />
		</span>
	);
}

function ComparisonRef({ value }: { value: GitComparisonRef }) {
	return (
		<span className="git-comparison-ref">
			<span className="git-comparison-branch">{value.branch}</span>
			{value.pull_request ? (
				<a href={value.pull_request.url} target="_blank" rel="noreferrer">
					#{value.pull_request.number} {value.pull_request.title}
				</a>
			) : null}
		</span>
	);
}

function comparisonTitle(comparison: GitComparison): string {
	return `${refTitle(comparison.base)} → ${refTitle(comparison.tip)}`;
}

function refTitle(value: GitComparisonRef): string {
	return value.pull_request
		? `${value.branch} · #${value.pull_request.number} ${value.pull_request.title}`
		: value.branch;
}
