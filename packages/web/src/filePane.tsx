import { ArrowLeft, RefreshCw, X } from "lucide-react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useEffect } from "react";
import {
	invalidateCachedWorkspaceFile,
	loadCachedWorkspaceFile,
	workspaceFileQueryKey,
} from "./fileBrowser.ts";
import { FileView } from "./fileView.tsx";
import type { AgentApi } from "./agentApi.ts";
import { browsePathBasename } from "./filePath.ts";
import { workspaceFileCache } from "./workspaceFileCache.ts";

export interface FilePaneProps {
	api: AgentApi;
	sessionId: string;
	path: string;
	replacementMode: boolean;
	remoteReadBlockedReason?: string | null;
	onClose: () => void;
	onNavigate: (path: string) => void;
}

export function FilePane({
	api,
	sessionId,
	path,
	replacementMode,
	remoteReadBlockedReason,
	onClose,
	onNavigate,
}: FilePaneProps) {
	const queryClient = useQueryClient();
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
		<section className="file-pane" data-slot="file-pane" aria-label="File preview">
			<header className="file-pane-header">
				{replacementMode ? (
					<button className="secondary-button file-pane-back" type="button" onClick={onClose}>
						<ArrowLeft size={14} aria-hidden />
						Back to chat
					</button>
				) : null}
				<div className="file-pane-title" title={path}>
					<span className="file-pane-name">{browsePathBasename(path)}</span>
					<span className="muted file-pane-path">{path}</span>
				</div>
				<div className="file-pane-actions">
					<button
						className="icon-button"
						type="button"
						aria-label="Refresh file"
						title="Refresh file"
						disabled={Boolean(remoteReadBlockedReason) || query.isFetching}
						onClick={() => {
							invalidateCachedWorkspaceFile(sessionId, path);
							void queryClient.invalidateQueries({ queryKey: workspaceFileQueryKey(sessionId, path) });
						}}
					>
						<RefreshCw size={14} />
					</button>
					{!replacementMode ? (
						<button className="icon-button" type="button" aria-label="Close file" title="Close file" onClick={onClose}>
							<X size={14} />
						</button>
					) : null}
				</div>
			</header>
			<div className="file-pane-body">
				{remoteReadBlockedReason ? (
					<p className="muted">{remoteReadBlockedReason}</p>
				) : query.isLoading ? (
					<p className="muted">Loading file…</p>
				) : query.error ? (
					<p className="error-text">{query.error instanceof Error ? query.error.message : "Failed to load file"}</p>
				) : query.data ? (
					<>
						<div className="file-pane-meta muted">
							{query.data.bytes.byteLength.toLocaleString()} / {query.data.totalSize.toLocaleString()}{" "}
							bytes
							{query.data.mtimeMs ? ` · ${new Date(query.data.mtimeMs).toLocaleString()}` : ""}
							{query.isFetching ? " · refreshing…" : ""}
						</div>
						<FileView file={query.data} onNavigate={onNavigate} />
					</>
				) : null}
			</div>
		</section>
	);
}
