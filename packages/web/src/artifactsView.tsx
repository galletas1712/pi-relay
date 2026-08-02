import { useQuery } from "@tanstack/react-query";
import { useEffect, useState } from "react";
import { RefreshCw, File, GitBranch, FolderTree } from "lucide-react";
import type { AgentApi } from "./agentApi.ts";
import type { ConnectionStatus } from "./rpc.ts";
import { queryKeys } from "./queryKeys.ts";
import type { ArtifactsFile, ArtifactsSnapshot, SessionSnapshot } from "./types.ts";

type ArtifactsViewProps = {
	api: AgentApi;
	session: SessionSnapshot;
	connection: ConnectionStatus;
};

export function ArtifactsView({ api, session, connection }: ArtifactsViewProps) {
	const [workspaceDir, setWorkspaceDir] = useState(session.workspaces[0]?.workspace_dir ?? "");
	useEffect(() => {
		if (!session.workspaces.some((workspace) => workspace.workspace_dir === workspaceDir)) {
			setWorkspaceDir(session.workspaces[0]?.workspace_dir ?? "");
		}
	}, [session.workspaces, workspaceDir]);
	const workspace = session.workspaces.find((candidate) => candidate.workspace_dir === workspaceDir);
	const [section, setSection] = useStateSection();
	const snapshotQuery = useQuery({
		queryKey: queryKeys.artifacts(session.session_id, workspaceDir),
		queryFn: () => api.getArtifactsSnapshot(session.session_id, workspaceDir),
		enabled: connection === "open" && !!workspace,
		refetchInterval: 5000,
		refetchIntervalInBackground: false,
	});
	const handoffsQuery = useQuery({
		queryKey: ["artifacts-handoffs", session.session_id],
		queryFn: () => api.listDelegations(session.session_id),
		enabled: connection === "open" && section === "handoffs",
	});
	const [selectedPath, setSelectedPath] = useState<string | null>(null);
	const mobileDetail = selectedPath !== null && (section === "files" || section === "changes");
	const fileQuery = useQuery({
		queryKey: ["artifact-file", session.session_id, workspace?.workspace_dir, selectedPath],
		queryFn: () => api.readArtifactFile(session.session_id, workspace!.workspace_dir, selectedPath!),
		enabled: connection === "open" && !!workspace && section === "files" && !!selectedPath,
	});
	const diffQuery = useQuery({
		queryKey: ["artifact-diff", session.session_id, workspace?.workspace_dir, selectedPath],
		queryFn: () => api.getArtifactDiff(session.session_id, workspace!.workspace_dir, selectedPath ?? undefined),
		enabled: connection === "open" && workspace?.kind !== "local" && section === "changes" && !!selectedPath,
	});

	if (!workspace) {
		return <main className="artifacts-view"><p>No session workspace is configured.</p></main>;
	}
	const snapshot = snapshotQuery.data;
	const visibleTree = snapshot?.tree.filter((entry) => !isHandoffPath(entry.path));
	const visibleChanges = snapshot?.git?.changes.filter(
		(change) => !isHandoffPath(change.path) && (!change.old_path || !isHandoffPath(change.old_path)),
	);
	return (
		<main className="artifacts-view" data-slot="artifacts-view" data-mobile-detail={mobileDetail ? "true" : "false"}>
			<header className="artifacts-header">
				<div>
					<p className="workspace-route-eyebrow">Workspace artifacts</p>
					<p className="artifacts-mobile-status" aria-live="polite">
						{snapshot?.git ? `${snapshot.git.changes.length} changes` : "Local workspace · Git unavailable"}
					</p>
					<label>
						<span className="artifacts-muted">Workspace</span>
						<select value={workspaceDir} onChange={(event) => {
							setWorkspaceDir(event.target.value);
							setSelectedPath(null);
						}} disabled={connection !== "open"}>
							{session.workspaces.map((candidate) => (
								<option key={candidate.workspace_dir} value={candidate.workspace_dir}>{candidate.workspace_dir}</option>
							))}
						</select>
					</label>
					<p className="artifacts-summary">
						{snapshot?.git?.branch ?? "local workspace"} · {snapshot?.git?.changes.length ?? 0} changes
					</p>
				</div>
				<button className="secondary-button" type="button" onClick={() => void snapshotQuery.refetch()} disabled={connection !== "open" || snapshotQuery.isFetching}>
					<RefreshCw size={15} aria-hidden /> Refresh
				</button>
			</header>
			<nav className="artifacts-nav" aria-label="Workspace artifacts sections">
				{(["files", "changes", "handoffs"] as const).map((item) => (
					<button key={item} type="button" className={section === item ? "artifacts-nav-active" : ""} onClick={() => setSection(item)}>
						{item === "files" ? <FolderTree size={15} /> : item === "changes" ? <GitBranch size={15} /> : <File size={15} />}
						{item[0].toUpperCase() + item.slice(1)}
					</button>
				))}
			</nav>
			<div className="artifacts-body">
				<section className="artifacts-list" aria-label={section}>
					{connection !== "open" ? <p role="alert">Workspace inspection is waiting for a connection.</p> : null}
					{snapshotQuery.isLoading ? <p>Loading workspace…</p> : snapshotQuery.error ? <p role="alert">Couldn’t inspect this workspace: {errorMessage(snapshotQuery.error)}</p> : null}
					{section === "files" ? visibleTree?.filter((entry) => entry.kind === "file").map((entry) => (
						<button key={entry.path} type="button" className={selectedPath === entry.path ? "artifact-row selected" : "artifact-row"} onClick={() => setSelectedPath(entry.path)}>
							<span>{entry.path}</span><small>{formatBytes(entry.size)}</small>
						</button>
					)) : null}
					{section === "changes" && snapshot?.git ? visibleChanges?.map((change) => (
						<button key={change.path} type="button" className={selectedPath === change.path ? "artifact-row selected" : "artifact-row"} onClick={() => setSelectedPath(change.path)}>
							<span>{change.status} {change.path}</span>
						</button>
					)) : section === "changes" ? <p className="artifacts-muted">Changes are unavailable because this is a local, non-Git workspace.</p> : null}
					{section === "handoffs" ? (
						handoffsQuery.isLoading ? <p>Loading handoffs…</p> :
							handoffsQuery.error ? <p role="alert">Couldn’t load handoffs: {errorMessage(handoffsQuery.error)}</p> :
							<p>{handoffsQuery.data?.delegations.length ?? 0} delegation handoffs. Open a delegation in the inspector to read its allowlisted files.</p>
					) : null}
					{section === "changes" && snapshot?.git?.baseline == null ? <p className="artifacts-muted">Git baseline unavailable for this older session; showing HEAD comparison.</p> : null}
				</section>
				<section className="artifacts-preview" aria-live="polite">
					{mobileDetail ? (
						<button
							type="button"
							className="artifacts-mobile-back"
							onClick={() => setSelectedPath(null)}
						>
							Back to {section === "changes" ? "Changes" : "Files"}
						</button>
					) : null}
					{section === "files" ? <Preview query={fileQuery} empty="Select a file to preview." /> : null}
					{section === "changes" ? <Preview query={diffQuery} error={diffQuery.error} empty={snapshot?.git ? "Select a change to preview." : "Git changes are unavailable for this local workspace."} /> : null}
					{section === "handoffs" ? <p className="artifacts-muted">Read-only workspace inspection does not expose .pi-handoff files.</p> : null}
				</section>
			</div>
		</main>
	);
}

function Preview({ query, empty, error }: { query: { data?: ArtifactsFile | { contents: string; truncated: boolean }; isFetching: boolean }; empty: string; error?: unknown }) {
	if (error) return <p role="alert">Couldn’t load preview: {errorMessage(error)}</p>;
	if (!query.data) return <p className="artifacts-muted">{query.isFetching ? "Loading preview…" : empty}</p>;
	return <pre className="artifacts-code">{query.data.contents}{query.data.truncated ? "\n\n… preview truncated …" : ""}</pre>;
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : "request failed";
}

function isHandoffPath(path: string): boolean {
	return path === ".pi-handoff" || path.startsWith(".pi-handoff/");
}

function formatBytes(size: number) {
	return size < 1024 ? `${size} B` : `${Math.round(size / 1024)} KB`;
}

function useStateSection(): ["files" | "changes" | "handoffs", (value: "files" | "changes" | "handoffs") => void] {
	// Kept local to avoid making the route URL carry transient preview state.
	const [section, setSection] = useState<"files" | "changes" | "handoffs">("files");
	return [section, setSection];
}

