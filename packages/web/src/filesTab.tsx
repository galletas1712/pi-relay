import {
	asyncDataLoaderFeature,
	hotkeysCoreFeature,
	selectionFeature,
} from "@headless-tree/core";
import { useTree } from "@headless-tree/react";
import { useQueryClient } from "@tanstack/react-query";
import { ChevronRight, File, Folder, FolderOpen, RefreshCw } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef } from "react";
import type { AgentApi } from "./agentApi.ts";
import { fetchWorkspaceDir, workspaceDirQueryKey } from "./fileBrowser.ts";
import { joinBrowsePath } from "./filePath.ts";
import type { WorkspaceDirEntry, WorkspaceDirListing } from "./types.ts";

const ROOT_ID = "__root__";

type TreeItemData = {
	path: string;
	name: string;
	kind: WorkspaceDirEntry["kind"] | "root";
	size?: number | null;
};

function pathFromItemId(itemId: string): string {
	return itemId === ROOT_ID ? "" : itemId;
}

export interface FilesTabProps {
	api: AgentApi;
	sessionId: string | null;
	selectedPath: string | null;
	remoteReadBlockedReason?: string | null;
	activity?: string | null;
	onSelectFile: (path: string) => void;
}

export function FilesTab({
	api,
	sessionId,
	selectedPath,
	remoteReadBlockedReason,
	activity,
	onSelectFile,
}: FilesTabProps) {
	const queryClient = useQueryClient();
	const sessionIdRef = useRef(sessionId);
	sessionIdRef.current = sessionId;
	const prevActivity = useRef(activity);

	const loadChildren = useCallback(
		async (itemId: string): Promise<{ id: string; data: TreeItemData }[]> => {
			const sid = sessionIdRef.current;
			if (!sid || remoteReadBlockedReason) return [];
			const path = pathFromItemId(itemId);
			const listing = await queryClient.fetchQuery({
				queryKey: workspaceDirQueryKey(sid, path, null),
				queryFn: () => fetchWorkspaceDir(api, sid, path),
				staleTime: 5_000,
			});
			return listing.entries.map((entry) => {
				const childPath = joinBrowsePath(path, entry.name);
				return {
					id: childPath,
					data: {
						path: childPath,
						name: entry.name,
						kind: entry.kind,
						size: entry.size,
					},
				};
			});
		},
		[api, queryClient, remoteReadBlockedReason],
	);

	const tree = useTree<TreeItemData>({
		rootItemId: ROOT_ID,
		getItemName: (item) => item.getItemData()?.name ?? "",
		isItemFolder: (item) => {
			const data = item.getItemData();
			return data?.kind === "directory" || data?.kind === "root";
		},
		createLoadingItemData: () => ({ path: "", name: "…", kind: "other" }),
		dataLoader: {
			getItem: async (itemId) => {
				if (itemId === ROOT_ID) {
					return { path: "", name: "cwd", kind: "root" };
				}
				const path = pathFromItemId(itemId);
				const parent = path.includes("/") ? path.slice(0, path.lastIndexOf("/")) : "";
				const name = path.includes("/") ? path.slice(path.lastIndexOf("/") + 1) : path;
				const sid = sessionIdRef.current;
				if (!sid) {
					return { path, name, kind: "other" };
				}
				const cached = queryClient.getQueryData<WorkspaceDirListing>(
					workspaceDirQueryKey(sid, parent, null),
				);
				const hit = cached?.entries.find((entry) => entry.name === name);
				return {
					path,
					name,
					kind: hit?.kind ?? "file",
					size: hit?.size,
				};
			},
			getChildrenWithData: loadChildren,
		},
		features: [asyncDataLoaderFeature, selectionFeature, hotkeysCoreFeature],
	});

	useEffect(() => {
		if (!sessionId) return;
		if (prevActivity.current === "running" && activity === "idle") {
			void queryClient.invalidateQueries({ queryKey: ["workspace-dir", sessionId] });
			void queryClient.invalidateQueries({ queryKey: ["workspace-file", sessionId] });
			tree.rebuildTree();
		}
		prevActivity.current = activity;
	}, [activity, queryClient, sessionId, tree]);

	useEffect(() => {
		tree.rebuildTree();
	}, [sessionId, tree]);

	const items = tree.getItems();
	const canBrowse = Boolean(sessionId) && !remoteReadBlockedReason;
	const selectedSet = useMemo(() => new Set(selectedPath ? [selectedPath] : []), [selectedPath]);

	return (
		<div className="files-tab">
			<div className="files-tab-toolbar">
				<span className="files-tab-label">Session cwd</span>
				<button
					className="icon-button"
					type="button"
					aria-label="Refresh files"
					title="Refresh files"
					disabled={!canBrowse}
					onClick={() => {
						if (!sessionId) return;
						void queryClient.invalidateQueries({ queryKey: ["workspace-dir", sessionId] });
						tree.rebuildTree();
					}}
				>
					<RefreshCw size={14} />
				</button>
			</div>
			{!sessionId ? (
				<p className="muted files-tab-empty">Select a session to browse its files.</p>
			) : remoteReadBlockedReason ? (
				<p className="muted files-tab-empty">{remoteReadBlockedReason}</p>
			) : (
				<div {...tree.getContainerProps("Files")} className="files-tree">
					{items.map((item) => {
						const data = item.getItemData();
						if (!data || data.kind === "root") return null;
						const meta = item.getItemMeta();
						const expanded = item.isExpanded();
						const selected = selectedSet.has(data.path);
						const isFolder = data.kind === "directory";
						const disabled = data.kind === "other";
						const itemProps = item.getProps();
						return (
							<button
								key={item.getId()}
								{...itemProps}
								type="button"
								className={`files-tree-item${selected ? " selected" : ""}${disabled ? " disabled" : ""}`}
								style={{ paddingLeft: `${10 + meta.level * 12}px` }}
								disabled={disabled}
								onClick={(event) => {
									itemProps.onClick?.(event);
									if (isFolder) {
										if (expanded) item.collapse();
										else void item.expand();
										return;
									}
									if (data.kind === "file") onSelectFile(data.path);
								}}
							>
								<span className="files-tree-twist" aria-hidden>
									{isFolder ? (
										<ChevronRight
											size={12}
											className={expanded ? "files-tree-twist-open" : undefined}
										/>
									) : (
										<span className="files-tree-twist-spacer" />
									)}
								</span>
								<span className="files-tree-icon" aria-hidden>
									{isFolder ? (
										expanded ? <FolderOpen size={14} /> : <Folder size={14} />
									) : (
										<File size={14} />
									)}
								</span>
								<span className="files-tree-name">{data.name}</span>
							</button>
						);
					})}
				</div>
			)}
		</div>
	);
}
