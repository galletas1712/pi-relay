import { File, PanelLeftOpen, X } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useCallback, useEffect, useRef, useState } from "react";
import { loadCachedWorkspaceFile, workspaceFileQueryKey } from "./fileBrowser.ts";
import { FileView } from "./fileView.tsx";
import type { AgentApi } from "./agentApi.ts";
import { browsePathBasename } from "./filePath.ts";
import { GitComparisonSummary } from "./gitComparison.tsx";
import { workspaceGitDiffQueryKey } from "./gitStatus.ts";
import { useParkedScrollPreservation } from "./parkedScroll.ts";
import type { GitAgainst, GitComparison, SessionWorkspace } from "./types.ts";
import { diffMarker, parseUnifiedDiff } from "./unifiedDiff.ts";
import { workspaceFileCache } from "./workspaceFileCache.ts";

export type FilePaneViewMode = "contents" | "diff_working_tree" | "diff_branch";

export interface FilePaneProps {
	api: AgentApi;
	sessionId: string;
	path: string;
	workspaces?: SessionWorkspace[];
	initialViewMode?: FilePaneViewMode;
	remoteReadBlockedReason?: string | null;
	parked?: boolean;
	onClose: () => void;
	/** Restore chat beside the file (wide files-only → split). */
	onShowChat?: () => void;
	onNavigate: (path: string) => void;
}

function pathUnderGitRoot(path: string, workspaces: SessionWorkspace[] | undefined): boolean {
	return (workspaces ?? []).some(
		(workspace) =>
			(workspace.kind ?? "git") === "git" &&
			(path === workspace.workspace_dir || path.startsWith(`${workspace.workspace_dir}/`)),
	);
}

export function FilePane({
	api,
	sessionId,
	path,
	workspaces,
	initialViewMode = "contents",
	remoteReadBlockedReason,
	parked = false,
	onClose,
	onShowChat,
	onNavigate,
}: FilePaneProps) {
	const bodyRef = useRef<HTMLDivElement>(null);
	const getBodyScroller = useCallback(() => bodyRef.current, []);
	useParkedScrollPreservation(getBodyScroller, parked);
	const gitCapable = pathUnderGitRoot(path, workspaces);
	const [viewMode, setViewMode] = useState<FilePaneViewMode>(initialViewMode);

	useEffect(() => {
		setViewMode(initialViewMode);
	}, [initialViewMode, path]);

	useEffect(() => {
		if (!gitCapable && viewMode !== "contents") setViewMode("contents");
	}, [gitCapable, viewMode]);

	const contentsQuery = useQuery({
		queryKey: workspaceFileQueryKey(sessionId, path),
		queryFn: () => loadCachedWorkspaceFile(api, sessionId, path),
		enabled: !remoteReadBlockedReason && viewMode === "contents",
		staleTime: Infinity,
		gcTime: 0,
	});

	const diffAgainst: GitAgainst | null =
		viewMode === "diff_working_tree"
			? "working_tree"
			: viewMode === "diff_branch"
				? "branch"
				: null;

	const diffQuery = useQuery({
		queryKey: diffAgainst
			? workspaceGitDiffQueryKey(sessionId, path, diffAgainst)
			: ["workspace-git-diff", "none"],
		queryFn: () => api.gitDiff({ sessionId, path, against: diffAgainst! }),
		enabled: !remoteReadBlockedReason && !!diffAgainst && gitCapable,
		staleTime: 0,
	});

	useEffect(() => {
		workspaceFileCache.pin(sessionId, path);
		return () => {
			workspaceFileCache.unpin(sessionId, path);
		};
	}, [sessionId, path]);

	return (
		<section
			className="file-pane"
			data-slot="file-pane"
			aria-label="File preview"
			aria-hidden={parked || undefined}
			inert={parked}
		>
			<header className="file-pane-header">
				{onShowChat ? (
					<button
						className="icon-button"
						type="button"
						aria-label="Show chat"
						title="Show chat"
						onClick={onShowChat}
					>
						<PanelLeftOpen size={14} />
					</button>
				) : null}
				<span className="session-status-icon" aria-hidden>
					<File size={14} />
				</span>
				<div className="file-pane-title" title={path}>
					<span className="file-pane-name">{browsePathBasename(path)}</span>
					<span className="muted file-pane-path">{path}</span>
				</div>
				{gitCapable ? (
					<label className="file-pane-view-mode">
						<span className="sr-only">File view</span>
						<select
							value={viewMode}
							onChange={(event) => setViewMode(event.target.value as FilePaneViewMode)}
							aria-label="File view mode"
						>
							<option value="contents">Contents</option>
							<option value="diff_working_tree">Working tree diff</option>
							<option value="diff_branch">Branch diff</option>
						</select>
					</label>
				) : null}
				<div className="file-pane-actions">
					<button className="icon-button" type="button" aria-label="Close file" title="Close file" onClick={onClose}>
						<X size={14} />
					</button>
				</div>
			</header>
			<div className="file-pane-body" ref={bodyRef}>
				{remoteReadBlockedReason ? (
					<p className="muted">{remoteReadBlockedReason}</p>
				) : viewMode === "contents" ? (
					contentsQuery.isLoading || (contentsQuery.isFetching && !contentsQuery.data) ? (
						<p className="muted">Loading file…</p>
					) : contentsQuery.error ? (
						<p className="error-text">
							{contentsQuery.error instanceof Error
								? contentsQuery.error.message
								: "Failed to load file"}
						</p>
					) : contentsQuery.data ? (
						<>
							<div className="file-pane-meta muted">
								{contentsQuery.data.bytes.byteLength.toLocaleString()} /{" "}
								{contentsQuery.data.totalSize.toLocaleString()} bytes
								{contentsQuery.data.mtimeMs
									? ` · ${new Date(contentsQuery.data.mtimeMs).toLocaleString()}`
									: ""}
								{contentsQuery.isFetching ? " · updating…" : ""}
							</div>
							<FileView file={contentsQuery.data} onNavigate={onNavigate} />
						</>
					) : null
				) : diffQuery.isLoading || (diffQuery.isFetching && !diffQuery.data) ? (
					<p className="muted">Loading diff…</p>
				) : diffQuery.error ? (
					<p className="error-text">
						{diffQuery.error instanceof Error ? diffQuery.error.message : "Failed to load diff"}
					</p>
				) : diffQuery.data ? (
					<FileDiffView
						unified={diffQuery.data.unified}
						binary={diffQuery.data.binary}
						truncated={diffQuery.data.truncated}
						status={diffQuery.data.status ?? null}
						comparison={diffQuery.data.comparison ?? null}
						against={diffQuery.data.against}
						updating={diffQuery.isFetching}
					/>
				) : null}
			</div>
		</section>
	);
}

function FileDiffView({
	unified,
	binary,
	truncated,
	status,
	comparison,
	against,
	updating,
}: {
	unified: string;
	binary: boolean;
	truncated: boolean;
	status: string | null;
	comparison: GitComparison | null;
	against: GitAgainst;
	updating: boolean;
}) {
	const rows = parseUnifiedDiff(unified);
	return (
		<>
			<div className="file-pane-meta muted">
				{against === "branch" ? (
					comparison ? (
						<GitComparisonSummary comparison={comparison} />
					) : (
						"Branch comparison"
					)
				) : (
					"HEAD → working tree"
				)}
				{comparison ? ` · merge base ${comparison.merge_base_oid.slice(0, 8)}` : ""}
				{status ? ` · ${status}` : ""}
				{updating ? " · updating…" : ""}
			</div>
			{binary ? (
				<p className="muted">Binary file differs</p>
			) : rows.length === 0 ? (
				<p className="muted">No differences</p>
			) : (
				<div className="edit-diff file-pane-diff">
					{rows.map((row, index) => (
						<div className={`edit-diff-row ${row.kind}`} key={`${row.kind}-${index}`}>
							<span className="edit-diff-marker">{diffMarker(row.kind)}</span>
							<span className="edit-diff-text">{row.text || " "}</span>
						</div>
					))}
				</div>
			)}
			{truncated ? <p className="muted file-truncated-note">Diff truncated</p> : null}
		</>
	);
}
