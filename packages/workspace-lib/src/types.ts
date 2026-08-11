// Public types — mirror agent-runtime-protocol's ProjectWorkspace /
// WorkspaceKind / MaterializedSessionWorkspace and the browse report structs.

export type WorkspaceKind = "git" | "local";

/** Declaration of one project workspace (session.create / project default). */
export interface WorkspaceDecl {
	kind: WorkspaceKind;
	/** direct-child directory name under the session cwd (validated) */
	workspaceDir: string;
	/** git only */
	remoteUrl?: string;
	remoteBranch?: string;
	/** local only */
	sourcePath?: string;
	/** per-session git branch override (pi-relay SelectedWorkspace.branch_override) */
	branchOverride?: string;
}

export interface SessionWorkspace {
	workspaceDir: string;
	kind: WorkspaceKind;
	/** git: resolved commit oid at materialize time */
	commitOid?: string;
	/** git: the remote/session branch materialized (remote_branch or the override) */
	branch?: string;
	/** git: the session-local branch `pi/session/<sid>/<wdir>` */
	localBranch?: string;
}

export interface MaterializedSession {
	sessionId: string;
	/** <root>/sessions/<id> */
	sessionRoot: string;
	/** <root>/sessions/<id>/cwd — the subvolume, host cwd */
	cwd: string;
	/** btrfs subvolume, or plain directory in non-btrfs fallback mode */
	subvolume: boolean;
	workspaces: SessionWorkspace[];
}

export type DirEntryKind = "file" | "directory" | "other";

export interface DirEntry {
	name: string;
	kind: DirEntryKind;
	size?: number;
	mtimeMs?: number;
}

export interface DirListing {
	path: string;
	entries: DirEntry[];
	nextAfterName?: string;
}

export interface FilePrefix {
	path: string;
	contentBase64: string;
	byteLen: number;
	totalSize: number;
	eof: boolean;
	mtimeMs?: number;
}

export type GitFileStatus = "added" | "modified" | "deleted" | "renamed" | "untracked" | "conflict";
export type GitAgainst = "working_tree" | "branch";

export interface GitComparisonRef {
	baseBranch: string;
	mergeBaseOid: string;
	headOid: string;
}

export interface GitStatusEntry {
	/** cwd-relative path (workspace_dir-prefixed) */
	path: string;
	status: GitFileStatus;
	staged: boolean;
}

export interface GitStatusRoot {
	workspaceDir: string;
	comparison: GitComparisonRef | null;
	error: string | null;
	entries: GitStatusEntry[];
}

export interface GitStatusReport {
	against: GitAgainst;
	roots: GitStatusRoot[];
}

export interface GitDiffReport {
	path: string | null;
	against: GitAgainst;
	comparison: GitComparisonRef | null;
	unified: string;
	binary: boolean;
	truncated: boolean;
}

export interface SearchMatch {
	/** cwd-relative path */
	path: string;
	line: number;
	text: string;
}

export interface SearchReport {
	matches: SearchMatch[];
	truncated: boolean;
}
