import { File, PanelLeftOpen, X } from "lucide-react";
import { useQuery } from "@tanstack/react-query";
import { useEffect } from "react";
import { loadCachedWorkspaceFile, workspaceFileQueryKey } from "./fileBrowser.ts";
import { FileView } from "./fileView.tsx";
import type { AgentApi } from "./agentApi.ts";
import { browsePathBasename } from "./filePath.ts";
import { workspaceFileCache } from "./workspaceFileCache.ts";

export interface FilePaneProps {
	api: AgentApi;
	sessionId: string;
	path: string;
	remoteReadBlockedReason?: string | null;
	parked?: boolean;
	onClose: () => void;
	/** Restore chat beside the file (wide files-only → split). */
	onShowChat?: () => void;
	onNavigate: (path: string) => void;
}

export function FilePane({
	api,
	sessionId,
	path,
	remoteReadBlockedReason,
	parked = false,
	onClose,
	onShowChat,
	onNavigate,
}: FilePaneProps) {
	const query = useQuery({
		queryKey: workspaceFileQueryKey(sessionId, path),
		queryFn: () => loadCachedWorkspaceFile(api, sessionId, path),
		enabled: !remoteReadBlockedReason,
		staleTime: Infinity,
		gcTime: 0,
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
				<div className="file-pane-actions">
					<button className="icon-button" type="button" aria-label="Close file" title="Close file" onClick={onClose}>
						<X size={14} />
					</button>
				</div>
			</header>
			<div className="file-pane-body">
				{remoteReadBlockedReason ? (
					<p className="muted">{remoteReadBlockedReason}</p>
				) : query.isLoading || (query.isFetching && !query.data) ? (
					<p className="muted">Loading file…</p>
				) : query.error ? (
					<p className="error-text">{query.error instanceof Error ? query.error.message : "Failed to load file"}</p>
				) : query.data ? (
					<>
						<div className="file-pane-meta muted">
							{query.data.bytes.byteLength.toLocaleString()} / {query.data.totalSize.toLocaleString()}{" "}
							bytes
							{query.data.mtimeMs ? ` · ${new Date(query.data.mtimeMs).toLocaleString()}` : ""}
							{query.isFetching ? " · updating…" : ""}
						</div>
						<FileView file={query.data} onNavigate={onNavigate} />
					</>
				) : null}
			</div>
		</section>
	);
}
