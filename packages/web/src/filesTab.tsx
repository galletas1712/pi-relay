import {
	asyncDataLoaderFeature,
	hotkeysCoreFeature,
	selectionFeature,
} from "@headless-tree/core";
import { useTree } from "@headless-tree/react";
import { useQuery, useQueryClient } from "@tanstack/react-query";
import { ChevronRight, File, Folder, FolderOpen } from "lucide-react";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { AgentApi } from "./agentApi.ts";
import {
	fetchWorkspaceDir,
	mergeWorkspaceDirPage,
	workspaceDirQueryKey,
} from "./fileBrowser.ts";
import { joinBrowsePath } from "./filePath.ts";
import { GitComparisonList } from "./gitComparison.tsx";
import {
	fetchWorkspaceGitStatus,
	GitStatusIndex,
	statusLetter,
	workspaceGitStatusQueryKey,
} from "./gitStatus.ts";
import type { GitAgainst, GitFileStatus, WorkspaceDirEntry, WorkspaceDirListing } from "./types.ts";
import { workspaceFileCache } from "./workspaceFileCache.ts";

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

function itemIdForDirPath(path: string): string {
	return path === "" ? ROOT_ID : path;
}

/** Directories whose listings are currently visible (cwd root + expanded folders). */
export function visibleBrowseDirectories(expandedItemIds: readonly string[]): string[] {
	const dirs = new Set<string>([""]);
	for (const itemId of expandedItemIds) {
		if (itemId === ROOT_ID) continue;
		dirs.add(pathFromItemId(itemId));
	}
	return [...dirs].sort();
}

export interface FilesTabProps {
	api: AgentApi;
	sessionId: string | null;
	selectedPath: string | null;
	remoteReadBlockedReason?: string | null;
	activity?: string | null;
	/** Bumped when interest-scoped dir listings should reload from disk. */
	treeEpoch?: number;
	/** When false, hide git comparison controls (no git workspace roots). */
	hasGitWorkspaces?: boolean;
	onSelectFile: (path: string, diffAgainst?: GitAgainst) => void;
	onVisibleDirectoriesChange?: (directories: string[]) => void;
}

export function FilesTab({
	api,
	sessionId,
	selectedPath,
	remoteReadBlockedReason,
	activity,
	treeEpoch = 0,
	hasGitWorkspaces = false,
	onSelectFile,
	onVisibleDirectoriesChange,
}: FilesTabProps) {
	const queryClient = useQueryClient();
	const sessionIdRef = useRef(sessionId);
	sessionIdRef.current = sessionId;
	const prevActivity = useRef(activity);
	const [expandedItems, setExpandedItems] = useState<string[]>([]);
	const expandedItemsRef = useRef(expandedItems);
	expandedItemsRef.current = expandedItems;
	const [loadingMore, setLoadingMore] = useState<string | null>(null);
	const [listingEpoch, setListingEpoch] = useState(0);
	const [gitAgainst, setGitAgainst] = useState<GitAgainst>("working_tree");

	const gitStatusQuery = useQuery({
		queryKey: sessionId
			? workspaceGitStatusQueryKey(sessionId, gitAgainst)
			: ["workspace-git-status", "none"],
		queryFn: () => fetchWorkspaceGitStatus(api, sessionId!, gitAgainst),
		enabled: !!sessionId && hasGitWorkspaces && !remoteReadBlockedReason,
		staleTime: 0,
	});

	const statusIndex = useMemo(
		() => new GitStatusIndex(gitStatusQuery.data),
		[gitStatusQuery.data],
	);

	const loadChildren = useCallback(
		async (itemId: string): Promise<{ id: string; data: TreeItemData }[]> => {
			const sid = sessionIdRef.current;
			if (!sid || remoteReadBlockedReason) return [];
			const path = pathFromItemId(itemId);
			const listing = await queryClient.fetchQuery({
				queryKey: workspaceDirQueryKey(sid, path, null),
				queryFn: async () => {
					const page = await fetchWorkspaceDir(api, sid, path);
					return mergeWorkspaceDirPage(undefined, page);
				},
				staleTime: 0,
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
		state: { expandedItems },
		setExpandedItems,
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

	const reloadVisibleDirectories = useCallback(() => {
		const itemIds = [ROOT_ID, ...expandedItemsRef.current];
		for (const itemId of itemIds) {
			try {
				void tree.getItemInstance(itemId).invalidateChildrenIds(true);
			} catch {
				// Item may not be mounted yet.
			}
		}
	}, [tree]);

	useEffect(() => {
		onVisibleDirectoriesChange?.(visibleBrowseDirectories(expandedItems));
	}, [expandedItems, onVisibleDirectoriesChange]);

	useEffect(() => {
		return () => onVisibleDirectoriesChange?.([]);
	}, [onVisibleDirectoriesChange]);

	useEffect(() => {
		if (!sessionId) return;
		if (prevActivity.current === "running" && activity === "idle") {
			void queryClient.resetQueries({ queryKey: ["workspace-dir", sessionId] }).then(() => {
				reloadVisibleDirectories();
			});
			workspaceFileCache.clearSession(sessionId);
			void queryClient.resetQueries({ queryKey: ["workspace-file", sessionId] });
			void queryClient.resetQueries({ queryKey: ["workspace-git-status", sessionId] });
			void queryClient.resetQueries({ queryKey: ["workspace-git-diff", sessionId] });
		}
		prevActivity.current = activity;
	}, [activity, queryClient, reloadVisibleDirectories, sessionId]);

	useEffect(() => {
		if (treeEpoch === 0) return;
		reloadVisibleDirectories();
		if (sessionId && hasGitWorkspaces) {
			void queryClient.invalidateQueries({ queryKey: ["workspace-git-status", sessionId] });
			void queryClient.invalidateQueries({ queryKey: ["workspace-git-diff", sessionId] });
		}
	}, [treeEpoch, reloadVisibleDirectories, queryClient, sessionId, hasGitWorkspaces]);

	useEffect(() => {
		if (listingEpoch === 0) return;
		reloadVisibleDirectories();
	}, [listingEpoch, reloadVisibleDirectories]);

	const loadMore = useCallback(
		async (dirPath: string) => {
			if (!sessionId || remoteReadBlockedReason) return;
			const key = workspaceDirQueryKey(sessionId, dirPath, null);
			const current = queryClient.getQueryData<WorkspaceDirListing>(key);
			const afterName = current?.next_after_name;
			if (!afterName) return;
			setLoadingMore(dirPath);
			try {
				const page = await fetchWorkspaceDir(api, sessionId, dirPath, afterName);
				queryClient.setQueryData<WorkspaceDirListing>(key, (previous) =>
					mergeWorkspaceDirPage(previous, page),
				);
				try {
					void tree.getItemInstance(itemIdForDirPath(dirPath)).invalidateChildrenIds(true);
				} catch {
					setListingEpoch((epoch) => epoch + 1);
				}
			} finally {
				setLoadingMore(null);
			}
		},
		[api, queryClient, remoteReadBlockedReason, sessionId, tree],
	);

	const items = tree.getItems();
	const selectedSet = useMemo(() => new Set(selectedPath ? [selectedPath] : []), [selectedPath]);

	const nextAfterByDir = useMemo(() => {
		const map = new Map<string, string>();
		if (!sessionId) return map;
		const dirs = new Set<string>([""]);
		for (const itemId of expandedItems) {
			if (itemId !== ROOT_ID) dirs.add(pathFromItemId(itemId));
		}
		for (const dir of dirs) {
			const listing = queryClient.getQueryData<WorkspaceDirListing>(
				workspaceDirQueryKey(sessionId, dir, null),
			);
			if (listing?.next_after_name) map.set(dir, listing.next_after_name);
		}
		return map;
	}, [expandedItems, listingEpoch, queryClient, sessionId, treeEpoch]);

	const rootErrors = (gitStatusQuery.data?.roots ?? [])
		.map((root) => root.error)
		.filter((error): error is string => !!error);
	const queryError =
		gitStatusQuery.error instanceof Error
			? gitStatusQuery.error.message
			: gitStatusQuery.error
				? "Git status request failed"
				: null;
	const deletedPaths = statusIndex.pathsWithStatus("deleted");

	return (
		<div className="files-tab">
			{!sessionId ? (
				<p className="muted files-tab-empty">Select a session to browse its files.</p>
			) : remoteReadBlockedReason ? (
				<p className="muted files-tab-empty">{remoteReadBlockedReason}</p>
			) : (
				<>
					{hasGitWorkspaces ? (
						<div className="files-git-toolbar" role="group" aria-label="Git status view">
							<button
								type="button"
								className={`files-git-mode${gitAgainst === "working_tree" ? " active" : ""}`}
								aria-pressed={gitAgainst === "working_tree"}
								title="Changes not included in HEAD"
								onClick={() => setGitAgainst("working_tree")}
							>
								Working tree
							</button>
							<button
								type="button"
								className={`files-git-mode${gitAgainst === "branch" ? " active" : ""}`}
								aria-pressed={gitAgainst === "branch"}
								title="All changes on the current branch or PR"
								onClick={() => setGitAgainst("branch")}
							>
								Branch changes
							</button>
							{gitStatusQuery.isFetching ? (
								<span className="muted files-git-toolbar-hint">updating…</span>
							) : null}
						</div>
					) : null}
					{gitAgainst === "branch" ? (
						<GitComparisonList roots={gitStatusQuery.data?.roots ?? []} />
					) : null}
					{queryError || rootErrors.length > 0 ? (
						<p
							className="error-text files-git-error"
							title={queryError ?? rootErrors.join("\n")}
						>
							{queryError ? "Git status unavailable" : "Git status unavailable for some roots"}
						</p>
					) : null}
					{deletedPaths.length > 0 ? (
						<details className="files-git-deleted">
							<summary>
								{deletedPaths.length} deleted {deletedPaths.length === 1 ? "file" : "files"}
							</summary>
							<div className="files-git-deleted-list">
								{deletedPaths.map((path) => (
									<button
										type="button"
										key={path}
										aria-label={`Open diff for deleted file ${path}`}
										title={`Open ${gitAgainst === "working_tree" ? "working tree" : "branch"} diff for ${path}`}
										onClick={() => onSelectFile(path, gitAgainst)}
									>
										<span aria-hidden>D</span>
										<span>{path}</span>
									</button>
								))}
							</div>
						</details>
					) : null}
					<div {...tree.getContainerProps("Files")} className="files-tree">
						{items.map((item, index) => {
							const data = item.getItemData();
							if (!data || data.kind === "root") return null;
							const meta = item.getItemMeta();
							const expanded = item.isExpanded();
							const selected = selectedSet.has(data.path);
							const isFolder = data.kind === "directory";
							const disabled = data.kind === "other";
							const itemProps = item.getProps();
							const parentPath = pathFromItemId(meta.parentId ?? ROOT_ID);
							const nextSibling = items[index + 1];
							const nextParent = nextSibling
								? pathFromItemId(nextSibling.getItemMeta().parentId ?? ROOT_ID)
								: null;
							const showLoadMore =
								nextAfterByDir.has(parentPath) && nextParent !== parentPath;
							const status = hasGitWorkspaces ? statusIndex.statusFor(data.path) : null;
							return (
								<div key={item.getId()}>
									<button
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
										{status ? <GitStatusGlyph status={status} /> : null}
									</button>
									{showLoadMore ? (
										<button
											type="button"
											className="files-tree-item files-tree-load-more"
											style={{ paddingLeft: `${10 + meta.level * 12}px` }}
											disabled={loadingMore === parentPath}
											onClick={() => void loadMore(parentPath)}
										>
											{loadingMore === parentPath ? "Loading…" : "Load more"}
										</button>
									) : null}
								</div>
							);
						})}
					</div>
				</>
			)}
		</div>
	);
}

function GitStatusGlyph({ status }: { status: GitFileStatus }) {
	return (
		<span
			className={`files-tree-git-status files-tree-git-status-${status}`}
			title={status}
			aria-label={status}
		>
			{statusLetter(status)}
		</span>
	);
}
