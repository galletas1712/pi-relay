import { ArrowLeft, RefreshCw, X } from "lucide-react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { fetchWorkspaceFile, workspaceFileQueryKey } from "./fileBrowser.ts";
import { FileView } from "./fileView.tsx";
import type { AgentApi } from "./agentApi.ts";
import { browsePathBasename } from "./filePath.ts";

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
		queryFn: () => fetchWorkspaceFile(api, sessionId, path),
		enabled: !remoteReadBlockedReason,
		staleTime: 5_000,
	});

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
							{query.data.byte_len.toLocaleString()}
							{query.data.eof ? "" : "+"} / {query.data.total_size.toLocaleString()} bytes
							{query.data.mtime_ms ? ` · ${new Date(query.data.mtime_ms).toLocaleString()}` : ""}
						</div>
						<FileView file={query.data} onNavigate={onNavigate} />
					</>
				) : null}
			</div>
		</section>
	);
}
