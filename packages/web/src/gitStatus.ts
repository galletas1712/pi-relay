import type { AgentApi } from "./agentApi.ts";
import { queryKeys } from "./queryKeys.ts";
import type { GitAgainst, GitFileStatus, WorkspaceGitStatus } from "./types.ts";

export function workspaceGitStatusQueryKey(sessionId: string, against: GitAgainst) {
	return queryKeys.workspaceGitStatus(sessionId, against);
}

export function workspaceGitDiffQueryKey(sessionId: string, path: string, against: GitAgainst) {
	return queryKeys.workspaceGitDiff(sessionId, path, against);
}

export async function fetchWorkspaceGitStatus(
	api: AgentApi,
	sessionId: string,
	against: GitAgainst,
): Promise<WorkspaceGitStatus> {
	return api.gitStatus({ sessionId, against });
}

/** Priority for bubbling a folder glyph from descendant file statuses. */
const STATUS_RANK: Record<GitFileStatus, number> = {
	conflict: 5,
	modified: 4,
	added: 3,
	deleted: 2,
	untracked: 1,
};

export function statusLetter(status: GitFileStatus): string {
	switch (status) {
		case "modified":
			return "M";
		case "added":
			return "A";
		case "deleted":
			return "D";
		case "untracked":
			return "?";
		case "conflict":
			return "U";
	}
}

/**
 * Index of cwd-relative changed paths for O(log n) ancestor lookups.
 * Folder bubbling does not require the tree to have loaded children.
 */
export class GitStatusIndex {
	private readonly paths: string[];
	private readonly byPath: Map<string, GitFileStatus>;

	constructor(report: WorkspaceGitStatus | null | undefined) {
		this.byPath = new Map();
		for (const root of report?.roots ?? []) {
			for (const entry of root.entries) {
				this.byPath.set(entry.path, entry.status);
			}
		}
		this.paths = [...this.byPath.keys()].sort();
	}

	get(path: string): GitFileStatus | null {
		return this.byPath.get(path) ?? null;
	}

	/** Own status, or strongest status among descendants (path/). */
	statusFor(path: string): GitFileStatus | null {
		const own = this.byPath.get(path);
		let best: GitFileStatus | null = own ?? null;
		let bestRank = own ? STATUS_RANK[own] : 0;
		if (path === "") {
			for (const status of this.byPath.values()) {
				const rank = STATUS_RANK[status];
				if (rank > bestRank) {
					best = status;
					bestRank = rank;
				}
			}
			return best;
		}
		const prefix = `${path}/`;
		const start = lowerBound(this.paths, prefix);
		for (let i = start; i < this.paths.length; i += 1) {
			const candidate = this.paths[i]!;
			if (!candidate.startsWith(prefix)) break;
			const status = this.byPath.get(candidate);
			if (!status) continue;
			const rank = STATUS_RANK[status];
			if (rank > bestRank) {
				best = status;
				bestRank = rank;
			}
		}
		return best;
	}

	get size(): number {
		return this.byPath.size;
	}
}

function lowerBound(sorted: string[], value: string): number {
	let lo = 0;
	let hi = sorted.length;
	while (lo < hi) {
		const mid = (lo + hi) >> 1;
		if (sorted[mid]! < value) lo = mid + 1;
		else hi = mid;
	}
	return lo;
}
