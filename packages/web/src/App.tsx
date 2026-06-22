import { useQuery, useQueryClient } from "@tanstack/react-query";
import { memo, useCallback, useEffect, useMemo, useRef, useState, type RefObject } from "react";
import { Folder, FolderGit2, Menu, PanelRightOpen, X } from "lucide-react";
import ReactMarkdown from "react-markdown";
import rehypeRaw from "rehype-raw";
import remarkGfm from "remark-gfm";
import { createAgentApi } from "./agentApi.ts";
import { ChatPane } from "./chatPane.tsx";
import { Composer, type ComposerHandle } from "./composer.tsx";
import { CompactHistoryPickerDialog } from "./historyPickerCompact.tsx";
import { type HistoryTargetOption } from "./historyTargets.ts";
import { ExportDialog } from "./exportDialog.tsx";
import { randomId } from "./ids.ts";
import { Inspector, NoticeStack, Sidebar } from "./panels.tsx";
import { approximateJsonSize, perfEnabled, perfLog, perfNow } from "./perf.ts";
import { queryKeys } from "./queryKeys.ts";
import { isStageRunning, reRunParamsForStage } from "./runBoard.ts";
import type { ConnectionStatus } from "./rpc.ts";
import { COMMANDS, findCommand, parseSlash, type ParsedSlash } from "./slash.ts";
import { refreshPlanForEvent } from "./sessionEvents.ts";
import { markdownComponents } from "./transcript.tsx";
import {
	mergeSnapshotIntoSessionList,
	patchSessionListEventSummary,
	patchSessionListMetadata,
	patchSessionListProvider,
} from "./sessionQueryCache.ts";
import {
	applyEntryBodies,
	applyEventHighWater,
	applyQueueProjection,
	applySelectedSnapshot,
	applySwitchResultToCache,
	applyTranscriptAppendedEvent,
	applyTreeIndex,
	applyTranscriptTurns,
	applyTurnDetail,
	branchFromTree,
	emptySelectedSessionCache,
	mergeSessionActivityEvent,
	queueProjectionFromEvent,
	selectedEntries,
	treeNodesInOrder,
	turnCardsInOrder,
	turnDetailEntries,
	type SelectedSessionCache,
} from "./selectedSessionCache.ts";
import { useSelectedSessionStore } from "./selectedSessionStore.ts";
import {
	DEFAULT_PROVIDER,
	MODEL_OPTIONS,
	providerFromModelKey,
	providerModelKey,
	providerReasoningEffort,
	reasoningEffortsForProvider,
	textContent,
	withReasoningEffort,
} from "./sessionDefaults.ts";
import {
	projectTitle,
	sessionTitle,
	isArchivedSession,
	displayActivity,
	sortSessionsByLastUserMessage,
	tallyActivities,
	type SessionListItem,
} from "./sessionList.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import {
	loadUiSelection,
	rememberSelectedSession,
	rememberUiSelection,
	selectedSessionForProject,
} from "./uiResume.ts";
import {
	rememberWorkspaceScope,
	startWorkspacesFromScope,
	workspaceScopeForProject,
	type WorkspaceScopeEntry,
} from "./workspaceScope.ts";
import { WorkspaceScopePicker } from "./workspaceScopePicker.tsx";
import type {
	EventFrame,
	Notice,
	Project,
	ProviderConfig,
	ReasoningEffort,
	SessionSnapshot,
	SessionSummary,
	Stage,
	HandoffFileName,
	ToolListing,
	TranscriptEntry,
	TranscriptTreeNode,
	ProjectWorkspace,
} from "./types.ts";

const MAX_NOTICES = 24;
const NOTICE_TTL_MS = 4000;
const SESSION_LIST_REFRESH_DEBOUNCE_MS = 250;
const SELECTED_SESSION_REFRESH_DEBOUNCE_MS = 80;
const FOREGROUND_RECONCILE_THROTTLE_MS = 2000;
const TRANSCRIPT_INDEX_PAGE_SIZE = 5000;
const TRANSCRIPT_TURN_PAGE_SIZE = 50;
const SELECTED_SESSION_DISPLAY_SCOPE = "active_branch" as const;
const SIDEBAR_CLOSE_BEFORE_SELECT_MS = 200;
const MEDIUM_PANEL_QUERY = "(min-width: 900px)";
const WIDE_PANEL_QUERY = "(min-width: 1280px)";

type PanelMode = "compact" | "medium" | "wide";

function panelModeForViewport(): PanelMode {
	if (typeof window === "undefined" || typeof window.matchMedia !== "function") return "wide";
	if (window.matchMedia(WIDE_PANEL_QUERY).matches) return "wide";
	if (window.matchMedia(MEDIUM_PANEL_QUERY).matches) return "medium";
	return "compact";
}

function defaultPanelState(mode: PanelMode): { sidebarOpen: boolean; rightOpen: boolean } {
	return {
		sidebarOpen: mode === "wide",
		rightOpen: mode !== "compact",
	};
}

type ExportDialogState = {
	entries: TranscriptEntry[];
};

type HistoryDialogState = {
	sessionId: string;
	nodes: TranscriptTreeNode[];
	activeLeafId: string | null;
	loading?: boolean;
	error?: string | null;
};

type DeleteDialogState = {
	session: SessionListItem;
	deleting: boolean;
};

type WorkspaceDraft =
	| {
			kind: "git";
			workspace_dir: string;
			remote_url: string;
			remote_branch: string;
	  }
	| {
			kind: "local";
			workspace_dir: string;
			source_path: string;
	  };

type WorkspaceDraftPatch = {
	kind?: "git" | "local";
	workspace_dir?: string;
	remote_url?: string;
	remote_branch?: string;
	source_path?: string;
};

type ProjectDialogState = {
	mode: "create" | "edit";
	projectId?: string;
	name: string;
	workspaces: WorkspaceDraft[];
	saving: boolean;
};

type PromptDialogState = {
	loading: boolean;
	template: string;
	rendered: string | null;
	view: "rendered" | "template";
	error: string | null;
};

function workspaceDraftFromProject(workspace: ProjectWorkspace): WorkspaceDraft {
	const kind = workspace.kind ?? "git";
	if (kind === "local") {
		return {
			kind,
			workspace_dir: workspace.workspace_dir,
			source_path: workspace.source_path ?? ""
		};
	}
	return {
		kind: "git",
		workspace_dir: workspace.workspace_dir,
		remote_url: workspace.remote_url ?? "",
		remote_branch: workspace.remote_branch ?? ""
	};
}

function newWorkspaceDraft(kind: "git" | "local" = "git"): WorkspaceDraft {
	return kind === "local"
		? { kind: "local", workspace_dir: "", source_path: "" }
		: { kind: "git", workspace_dir: "", remote_url: "", remote_branch: "main" };
}

function updateWorkspaceDraft(current: WorkspaceDraft, patch: WorkspaceDraftPatch): WorkspaceDraft {
	const nextKind = patch.kind ?? current.kind;
	if (nextKind === "local") {
		return {
			kind: "local",
			workspace_dir: patch.workspace_dir ?? current.workspace_dir,
			source_path: patch.source_path ?? (current.kind === "local" ? current.source_path : "")
		};
	}
	return {
		kind: "git",
		workspace_dir: patch.workspace_dir ?? current.workspace_dir,
		remote_url: patch.remote_url ?? (current.kind === "git" ? current.remote_url : ""),
		remote_branch: patch.remote_branch ?? (current.kind === "git" ? current.remote_branch : "main")
	};
}

function projectWorkspacesFromDrafts(workspaces: WorkspaceDraft[]): ProjectWorkspace[] {
	return workspaces.map((workspace, index) => {
		if (!workspace.workspace_dir.trim()) throw new Error(`workspace ${index + 1}: name is required`);
		if (workspace.kind === "local") {
			if (!workspace.source_path.trim()) throw new Error(`workspace ${index + 1}: source path is required`);
			return {
				kind: "local",
				workspace_dir: workspace.workspace_dir.trim(),
				source_path: workspace.source_path.trim()
			};
		}
		if (!workspace.remote_url.trim()) throw new Error(`workspace ${index + 1}: remote URL is required`);
		if (!workspace.remote_branch.trim()) throw new Error(`workspace ${index + 1}: branch is required`);
		return {
			kind: "git",
			workspace_dir: workspace.workspace_dir.trim(),
			remote_url: workspace.remote_url.trim(),
			remote_branch: workspace.remote_branch.trim()
		};
	});
}

export function App() {
	const api = useMemo(() => createAgentApi(), []);
	const queryClient = useQueryClient();
	const initialUiSelection = useMemo(() => loadUiSelection(), []);
	const [connection, setConnection] = useState<ConnectionStatus>("connecting");
	const [selectedProjectId, setSelectedProjectId] = useState<string | null>(initialUiSelection.projectId);
	const [selectedId, setSelectedId] = useState<string | null>(initialUiSelection.sessionId);
	const selectedRef = useRef<string | null>(initialUiSelection.sessionId);
	const [notices, setNotices] = useState<Notice[]>([]);
	const [query, setQuery] = useState("");
	const [newSessionProvider, setNewSessionProvider] = useState<ProviderConfig>(DEFAULT_PROVIDER);
	const [sending, setSending] = useState(false);
	const [stopping, setStopping] = useState(false);
	const [resumingTurnId, setResumingTurnId] = useState<string | null>(null);
	const [historySwitchingSessionId, setHistorySwitchingSessionId] = useState<string | null>(null);
	const [sidebarOpen, setSidebarOpen] = useState(() => defaultPanelState(panelModeForViewport()).sidebarOpen);
	const [rightOpen, setRightOpen] = useState(() => defaultPanelState(panelModeForViewport()).rightOpen);
	const [panelMode, setPanelMode] = useState<PanelMode>(() => panelModeForViewport());
	const [showArchived, setShowArchived] = useState(false);
	const [historyDialog, setHistoryDialog] = useState<HistoryDialogState | null>(null);
	const [exportDialog, setExportDialog] = useState<ExportDialogState | null>(null);
	const [renameSessionId, setRenameSessionId] = useState<string | null>(null);
	const [renameValue, setRenameValue] = useState("");
	const [deleteDialog, setDeleteDialog] = useState<DeleteDialogState | null>(null);
	const [projectDialog, setProjectDialog] = useState<ProjectDialogState | null>(null);
	const [promptDialog, setPromptDialog] = useState<PromptDialogState | null>(null);
	const {
		cache: selectedCache,
		cacheRef: selectedCacheRef,
		drop: dropSelectedCache,
		replace: replaceSelectedCache,
		reset: resetSelectedCache,
		update: updateSelectedCache,
	} = useSelectedSessionStore(initialUiSelection.sessionId);
	const [selectedFetchState, setSelectedFetchState] = useState<{ sessionId: string | null; loading: boolean }>({
		sessionId: initialUiSelection.sessionId,
		loading: !!initialUiSelection.sessionId,
	});

	const selectedSyncTimer = useRef<number | null>(null);
	const sessionListRefreshTimer = useRef<number | null>(null);
	const composerHandleRef = useRef<ComposerHandle | null>(null);
	const nextSessionTitleRef = useRef<string | null>(null);
	const selectedProjectRef = useRef<string | null>(initialUiSelection.projectId);
	const lastEventIds = useRef(new Map<string, number>());
	const subscribedEventSessionIds = useRef(new Set<string>());
	const panelModeRef = useRef<PanelMode>(panelModeForViewport());
	const sidebarSelectTimer = useRef<number | null>(null);
	const selectedLoadVersion = useRef(0);
	const selectedRefreshInFlight = useRef(new Map<string, Promise<{ snapshot: SessionSnapshot; entries: TranscriptEntry[] } | null>>());
	const autoLoadedTurnDetailRef = useRef<string | null>(null);
	const lastForegroundReconcileAt = useRef(Date.now());
	const handleSessionEventRef = useRef<(event: EventFrame) => void>(() => undefined);

	const pushNotice = useCallback((tone: Notice["tone"], text: string) => {
		setNotices((current) => [...current.slice(Math.max(0, current.length - MAX_NOTICES + 1)), { id: randomId("notice"), tone, text }]);
	}, []);

	useEffect(() => {
		if (selectedRef.current !== selectedId) selectedRef.current = selectedId;
	}, [selectedId]);

	useEffect(() => {
		if (selectedProjectRef.current !== selectedProjectId) selectedProjectRef.current = selectedProjectId;
	}, [selectedProjectId]);

	useEffect(() => {
		if (notices.length === 0) return;
		const timer = window.setTimeout(() => {
			setNotices((current) => current.slice(1));
		}, NOTICE_TTL_MS);
		return () => window.clearTimeout(timer);
	}, [notices.length]);

	const projectsQuery = useQuery({
		queryKey: queryKeys.projects,
		queryFn: () => api.listProjects(),
		enabled: connection === "open",
	});
	const projects = projectsQuery.data ?? [];

	const sessionsQuery = useQuery({
		queryKey: queryKeys.sessions(selectedProjectId),
		queryFn: () => api.listSessions(100, selectedProjectId),
		enabled: connection === "open",
	});
	const sessions = sessionsQuery.data ?? [];

	const sessionItems = useMemo(() => sortSessionsByLastUserMessage(sessions), [sessions]);
	const selectedProject = useMemo(
		() => projects.find((project) => project.project_id === selectedProjectId) ?? null,
		[projects, selectedProjectId],
	);

	// Editable workspace scope for the *next* new session in the selected project.
	// Reset from persisted scope whenever the project (or its workspace set) changes;
	// projectWorkspaceKey captures the workspace set so renames/edits re-derive scope
	// while a plain projects refetch leaves in-progress edits untouched.
	const [workspaceScope, setWorkspaceScope] = useState<WorkspaceScopeEntry[]>([]);
	const workspaceScopeRef = useRef<WorkspaceScopeEntry[]>(workspaceScope);
	workspaceScopeRef.current = workspaceScope;
	const projectWorkspaces = selectedProject?.workspaces ?? null;
	const projectWorkspacesRef = useRef(projectWorkspaces);
	projectWorkspacesRef.current = projectWorkspaces;
	const projectWorkspaceKey = projectWorkspaces?.map((workspace) => workspace.workspace_dir).join("\n") ?? "";
	useEffect(() => {
		setWorkspaceScope(workspaceScopeForProject(selectedProjectId, projectWorkspacesRef.current ?? []));
	}, [selectedProjectId, projectWorkspaceKey]);
	const handleWorkspaceScopeChange = useCallback((scope: WorkspaceScopeEntry[]) => {
		setWorkspaceScope(scope);
		rememberWorkspaceScope(selectedProjectRef.current, scope);
	}, []);

	const selectedSession = useMemo(
		() => sessionItems.find((session) => session.session_id === selectedId) ?? null,
		[sessionItems, selectedId],
	);

	const loadedSnapshot = selectedCache.sessionId === selectedId ? selectedCache.snapshot : null;
	const historySwitchingSelectedSession = !!selectedId && historySwitchingSessionId === selectedId;
	const selectedLoading = selectedFetchState.sessionId === selectedId && selectedFetchState.loading;
	const transcriptLoading = !!selectedId && ((!loadedSnapshot && selectedLoading) || historySwitchingSelectedSession);
	const loadedEntries = useMemo(
		() => (selectedCache.sessionId === selectedId ? selectedEntries(selectedCache) : []),
		[selectedCache.activeBranchEntryIds, selectedCache.entriesById, selectedCache.sessionId, selectedId],
	);
	const [expandedTurnIds, setExpandedTurnIds] = useState<Set<string>>(() => new Set());
	const [loadingTurnId, setLoadingTurnId] = useState<string | null>(null);
	const [autoLoadingTurnId, setAutoLoadingTurnId] = useState<string | null>(null);
	const [loadingOlderTurns, setLoadingOlderTurns] = useState(false);
	const orderedTurnCards = useMemo(
		() => (selectedCache.sessionId === selectedId ? turnCardsInOrder(selectedCache) : []),
		[selectedCache.sessionId, selectedCache.turnCardsById, selectedCache.turnOrder, selectedId],
	);
	const latestTurnCard = orderedTurnCards.at(-1) ?? null;
	const runningTurnCardId = loadedSnapshot?.activity === "running" && latestTurnCard?.status === "open" ? latestTurnCard.id : null;
	const turnCardViews = useMemo(() => {
		if (orderedTurnCards.length === 0) return null;
		return orderedTurnCards.map((card) => {
			const isCurrent = card.id === runningTurnCardId;
			const expanded = expandedTurnIds.has(card.id) || isCurrent;
			return {
				card,
				entries: expanded ? turnDetailEntries(selectedCache, card.id) : null,
				expanded,
				isCurrent,
			};
		});
	}, [
		expandedTurnIds,
		orderedTurnCards,
		runningTurnCardId,
		selectedCache.entriesById,
		selectedCache.turnDetailsById,
	]);

	const snapshotChatSession = useMemo(() => {
		if (!selectedId || !loadedSnapshot) return null;
		return {
			session_id: selectedId,
			project_id: loadedSnapshot.project_id,
			activity: loadedSnapshot.activity,
			active_leaf_id: loadedSnapshot.active_leaf_id,
			provider: loadedSnapshot.provider,
			metadata: loadedSnapshot.metadata,
		};
	}, [loadedSnapshot, selectedId]);
	const selectedListChatSession = useMemo(() => {
		if (!selectedId) return null;
		return {
			session_id: selectedId,
			project_id: selectedSession?.project_id ?? loadedSnapshot?.project_id ?? selectedProjectId,
			activity: selectedSession?.activity ?? "idle",
			active_leaf_id: selectedSession?.active_leaf_id ?? null,
			provider: selectedSession?.provider ?? newSessionProvider,
			metadata: selectedSession?.metadata ?? {},
		};
	}, [loadedSnapshot?.project_id, newSessionProvider, selectedId, selectedProjectId, selectedSession]);
	const selectedChatSession = snapshotChatSession ?? selectedListChatSession;

	const activeProvider = loadedSnapshot?.provider ?? selectedSession?.provider ?? newSessionProvider;
	const activeProviderKind = activeProvider.kind;
	const toolsQuery = useQuery({
		queryKey: queryKeys.tools(activeProviderKind),
		queryFn: () => api.listTools(activeProviderKind),
		enabled: connection === "open",
	});
	const tools: ToolListing[] = toolsQuery.data ?? [];
	const stagesQuery = useQuery({
		queryKey: queryKeys.stages(loadedSnapshot?.session_id ?? null),
		queryFn: () => {
			if (!loadedSnapshot) throw new Error("select a session first");
			return api.listStages(loadedSnapshot.session_id);
		},
		enabled: connection === "open" && !!loadedSnapshot,
		// The parent PARKS (goes idle) while a stage runs, so gate the poll on
		// whether any stage is actually running — not on the parent's activity —
		// or the missed-event safety net would be off exactly when it's needed.
		refetchInterval: (query) =>
			(query.state.data?.stages ?? []).some(isStageRunning) ? 2_000 : false,
	});
	const stages = stagesQuery.data?.stages ?? [];
	const stageSubagentIds = useMemo(
		() => stages.flatMap((stage) => stage.subagents.map((subagent) => subagent.id)),
		[stages],
	);
	const reasoningEfforts = reasoningEffortsForProvider(activeProvider);
	const hasTranscriptEntries =
		loadedSnapshot?.has_transcript_entries ??
		selectedSession?.has_transcript_entries ??
		(loadedSnapshot ? loadedEntries.length > 0 || loadedSnapshot.active_leaf_id !== null : false);
	const modelLocked = !!selectedId && !!loadedSnapshot && hasTranscriptEntries;
	const modelControlsDisabled = !!selectedId && (!loadedSnapshot || loadedSnapshot.activity !== "idle");

	const selectSession = useCallback((sessionId: string | null) => {
		const previousSessionId = selectedRef.current;
		if (sessionId === previousSessionId) {
			if (sessionId === null) nextSessionTitleRef.current = null;
			return;
		}
		if (sessionId === null) nextSessionTitleRef.current = null;
		selectedRef.current = sessionId;
		setSelectedId(sessionId);
		selectedLoadVersion.current += 1;
		const nextCache = resetSelectedCache(sessionId);
		setSelectedFetchState({
			sessionId,
			loading: !!sessionId && !nextCache.snapshot,
		});
		rememberSelectedSession(selectedProjectRef.current, sessionId);
	}, [resetSelectedCache]);

	const selectProjectSession = useCallback((projectId: string | null, sessionId: string | null) => {
		selectedProjectRef.current = projectId;
		selectedRef.current = sessionId;
		setSelectedProjectId(projectId);
		setSelectedId(sessionId);
		selectedLoadVersion.current += 1;
		const nextCache = resetSelectedCache(sessionId);
		setSelectedFetchState({
			sessionId,
			loading: !!sessionId && !nextCache.snapshot,
		});
		rememberUiSelection(projectId, sessionId);
	}, [resetSelectedCache]);

	const invalidateSessionList = useCallback(
		(projectId = selectedProjectRef.current) => {
			void queryClient.invalidateQueries({
				queryKey: queryKeys.sessions(projectId),
			});
		},
		[queryClient],
	);

	const scheduleSessionListRefresh = useCallback(
		(delayMs = SESSION_LIST_REFRESH_DEBOUNCE_MS) => {
			if (sessionListRefreshTimer.current !== null) return;
			sessionListRefreshTimer.current = window.setTimeout(() => {
				sessionListRefreshTimer.current = null;
				invalidateSessionList();
			}, delayMs);
		},
		[invalidateSessionList],
	);

	const fetchSessionSnapshot = useCallback(
		async (sessionId: string, includeEntries: boolean, source: string) => {
			const shouldLogPerf = perfEnabled();
			const startedAt = perfNow();
			if (shouldLogPerf) perfLog("session.get start", { sessionId, source, includeEntries });
			const nextSnapshot = await api.getSession(sessionId, {
				includeEntries,
				entryScope: includeEntries ? SELECTED_SESSION_DISPLAY_SCOPE : undefined,
			});
			if (shouldLogPerf) {
				const rpcMs = perfNow() - startedAt;
				perfLog("session.get end", {
					sessionId,
					entries: nextSnapshot.entries?.length ?? 0,
					approxBytes: approximateJsonSize(nextSnapshot),
					rpcMs: Math.round(rpcMs),
					entryScope: includeEntries ? SELECTED_SESSION_DISPLAY_SCOPE : "none",
				});
			}
			return nextSnapshot;
		},
		[api],
	);

	const commitSelectedSnapshot = useCallback(
		(snapshot: SessionSnapshot) => {
			const observedEventId = lastEventIds.current.get(snapshot.session_id) ?? 0;
			lastEventIds.current.set(snapshot.session_id, Math.max(observedEventId, snapshot.last_event_id));
			if (snapshot.session_id === selectedRef.current) {
				updateSelectedCache((current) =>
					applySelectedSnapshot(current.sessionId === snapshot.session_id ? current : emptySelectedSessionCache(snapshot.session_id), snapshot),
				);
			}
			queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(snapshot.project_id), (current) =>
				mergeSnapshotIntoSessionList(current, snapshot),
			);
		},
		[queryClient],
	);

	const refreshTranscriptTurns = useCallback(
		async (sessionId: string) => {
			const result = await api.getTranscriptTurns(sessionId, { limit: TRANSCRIPT_TURN_PAGE_SIZE });
			if (selectedRef.current !== sessionId) return null;
			updateSelectedCache((current) => applyTranscriptTurns(current.sessionId === sessionId ? current : selectedCacheRef.current, result));
			return result;
		},
		[api, updateSelectedCache],
	);

	const loadOlderTranscriptTurns = useCallback(
		async () => {
			const sessionId = selectedRef.current;
			if (!sessionId || loadingOlderTurns) return;
			const cache = selectedCacheRef.current;
			if (cache.sessionId !== sessionId || !cache.turnHasMoreBefore || !cache.turnBeforeEntryId) return;
			const beforeEntryId = cache.turnBeforeEntryId;
			setLoadingOlderTurns(true);
			try {
				const result = await api.getTranscriptTurns(sessionId, {
					beforeEntryId,
					limit: TRANSCRIPT_TURN_PAGE_SIZE,
				});
				if (selectedRef.current !== sessionId) return;
				updateSelectedCache((current) =>
					applyTranscriptTurns(current.sessionId === sessionId ? current : selectedCacheRef.current, result, { mode: "prepend" }),
				);
			} catch (error) {
				if (selectedRef.current === sessionId) pushNotice("error", errorMessage(error));
			} finally {
				setLoadingOlderTurns(false);
			}
		},
		[api, loadingOlderTurns, pushNotice, updateSelectedCache],
	);

	const getFreshSession = useCallback(
		async (sessionId: string) => {
			const snapshot = await fetchSessionSnapshot(sessionId, false, "fetch");
			commitSelectedSnapshot(snapshot);
			await refreshTranscriptTurns(sessionId);
			if (selectedRef.current === sessionId && snapshot.project_id !== selectedProjectRef.current) {
				selectedProjectRef.current = snapshot.project_id;
				setSelectedProjectId(snapshot.project_id);
				rememberUiSelection(snapshot.project_id, sessionId);
			}
			const cache = selectedCacheRef.current;
			return {
				snapshot: cache.sessionId === sessionId && cache.snapshot ? cache.snapshot : snapshot,
				entries: cache.sessionId === sessionId ? selectedEntries(cache) : [],
			};
		},
		[commitSelectedSnapshot, fetchSessionSnapshot, refreshTranscriptTurns],
	);

	const patchSelectedSnapshot = useCallback(
		(
			sessionId: string,
			patcher: (snapshot: SessionSnapshot) => SessionSnapshot,
		) => {
			if (sessionId !== selectedRef.current) return;
			updateSelectedCache((current) => {
				if (current.sessionId !== sessionId || !current.snapshot) return current;
				const nextSnapshot = patcher({
					...current.snapshot,
					entries: selectedEntries(current),
				});
				return applySelectedSnapshot(current, nextSnapshot);
			});
		},
		[updateSelectedCache],
	);

	const refreshSelectedSessionState = useCallback(
		async (sessionId: string) => {
			if (sessionId !== selectedRef.current) return null;
			const inFlight = selectedRefreshInFlight.current.get(sessionId);
			if (inFlight) return inFlight;
			const currentSnapshot = selectedCacheRef.current.sessionId === sessionId ? selectedCacheRef.current.snapshot : null;
			setSelectedFetchState({
				sessionId,
				loading: !currentSnapshot,
			});
			const request = (async () => {
				let result: { snapshot: SessionSnapshot; entries: TranscriptEntry[] } | null;
				if (!currentSnapshot) {
					result = await getFreshSession(sessionId);
				} else {
					const snapshot = await fetchSessionSnapshot(sessionId, false, "refresh");
					if (selectedRef.current !== sessionId) return null;
					commitSelectedSnapshot(snapshot);
					if (selectedRef.current === sessionId && snapshot.project_id !== selectedProjectRef.current) {
						selectedProjectRef.current = snapshot.project_id;
						setSelectedProjectId(snapshot.project_id);
						rememberUiSelection(snapshot.project_id, sessionId);
					}
					const cacheAfterSnapshot = selectedCacheRef.current;
					const needsTurns =
						cacheAfterSnapshot.sessionId !== sessionId ||
						cacheAfterSnapshot.turnTranscriptRevision !== (snapshot.transcript_revision ?? null) ||
						cacheAfterSnapshot.turnActiveLeafId !== (snapshot.active_leaf_id ?? null) ||
						(snapshot.has_transcript_entries && cacheAfterSnapshot.turnOrder.length === 0);
					if (needsTurns) await refreshTranscriptTurns(sessionId);
					if (selectedRef.current !== sessionId) return null;
					const cache = selectedCacheRef.current;
					result = {
						snapshot: cache.sessionId === sessionId && cache.snapshot ? cache.snapshot : snapshot,
						entries: cache.sessionId === sessionId ? selectedEntries(cache) : [],
					};
				}
				if (selectedRef.current === sessionId) {
					setSelectedFetchState({
						sessionId,
						loading: false,
					});
				}
				return result;
			})().catch((error) => {
				if (selectedRef.current === sessionId) {
					setSelectedFetchState({
						sessionId,
						loading: false,
					});
				}
				throw error;
			}).finally(() => {
				if (selectedRefreshInFlight.current.get(sessionId) === request) {
					selectedRefreshInFlight.current.delete(sessionId);
				}
			});
			selectedRefreshInFlight.current.set(sessionId, request);
			return request;
		},
		[commitSelectedSnapshot, fetchSessionSnapshot, getFreshSession, refreshTranscriptTurns],
	);

	const syncActiveBranchNow = useCallback(
		(sessionId: string) => {
			if (selectedSyncTimer.current !== null) {
				window.clearTimeout(selectedSyncTimer.current);
				selectedSyncTimer.current = null;
			}
			return refreshSelectedSessionState(sessionId);
		},
		[refreshSelectedSessionState],
	);

	const loadTurnDetail = useCallback(
		async (cardId: string, options: { mode: "manual" | "auto" }) => {
			const sessionId = selectedRef.current;
			if (!sessionId) throw new Error("select a session first");
			const cache = selectedCacheRef.current;
			const card = cache.turnCardsById.get(cardId);
			if (!card) throw new Error("turn card is not loaded");
			if (turnDetailEntries(cache, cardId)) {
				if (options.mode === "manual") setExpandedTurnIds((current) => new Set(current).add(cardId));
				return;
			}
			if (options.mode === "manual") setLoadingTurnId(cardId);
			else setAutoLoadingTurnId(cardId);
			try {
				const result = await api.getTranscriptTurnDetail(sessionId, {
					cardId: card.id,
					leafId: card.active_leaf_id,
					startSequence: card.start_sequence,
					endSequence: card.end_sequence,
				});
				if (selectedRef.current !== sessionId) return;
				let applied = false;
				updateSelectedCache((current) => {
					const detail = applyTurnDetail(current.sessionId === sessionId ? current : selectedCacheRef.current, sessionId, result.card_id, result.entries);
					applied = detail.applied;
					return detail.cache;
				});
				if (applied && options.mode === "manual") setExpandedTurnIds((current) => new Set(current).add(result.card_id));
			} catch (error) {
				if (selectedRef.current === sessionId) pushNotice("error", errorMessage(error));
			} finally {
				if (options.mode === "manual") setLoadingTurnId((current) => (current === cardId ? null : current));
				else setAutoLoadingTurnId((current) => (current === cardId ? null : current));
			}
		},
		[api, pushNotice, updateSelectedCache],
	);
	const expandTurn = useCallback(
		(cardId: string) => {
			void loadTurnDetail(cardId, { mode: "manual" });
		},
		[loadTurnDetail],
	);
	const collapseTurn = useCallback((cardId: string) => {
		setExpandedTurnIds((current) => {
			if (!current.has(cardId)) return current;
			const next = new Set(current);
			next.delete(cardId);
			return next;
		});
	}, []);

	useEffect(() => {
		if (!runningTurnCardId || autoLoadingTurnId === runningTurnCardId) return;
		const cache = selectedCacheRef.current;
		const card = cache.turnCardsById.get(runningTurnCardId);
		const autoLoadKey = card ? `${runningTurnCardId}:${card.active_leaf_id}` : runningTurnCardId;
		if (autoLoadedTurnDetailRef.current === autoLoadKey) return;
		if (turnDetailEntries(cache, runningTurnCardId)) return;
		autoLoadedTurnDetailRef.current = autoLoadKey;
		void loadTurnDetail(runningTurnCardId, { mode: "auto" });
	}, [autoLoadingTurnId, loadTurnDetail, runningTurnCardId, selectedCache.turnCardsById, selectedCache.turnDetailsById]);

	useEffect(() => {
		autoLoadedTurnDetailRef.current = null;
	}, [selectedId]);

	const reconcileAfterForeground = useCallback(
		() => {
			if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
			const now = Date.now();
			if (now - lastForegroundReconcileAt.current < FOREGROUND_RECONCILE_THROTTLE_MS) return;
			lastForegroundReconcileAt.current = now;
			const sessionId = selectedRef.current;
			invalidateSessionList();
			if (!sessionId) return;
			void syncActiveBranchNow(sessionId).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[invalidateSessionList, pushNotice, syncActiveBranchNow],
	);

	useEffect(() => {
		const onVisibilityChange = () => {
			if (document.visibilityState === "visible") reconcileAfterForeground();
		};
		const onFocus = () => reconcileAfterForeground();
		const onPageShow = (event: PageTransitionEvent) => {
			if (event.persisted) reconcileAfterForeground();
		};
		document.addEventListener("visibilitychange", onVisibilityChange);
		window.addEventListener("focus", onFocus);
		window.addEventListener("pageshow", onPageShow);
		return () => {
			document.removeEventListener("visibilitychange", onVisibilityChange);
			window.removeEventListener("focus", onFocus);
			window.removeEventListener("pageshow", onPageShow);
		};
	}, [reconcileAfterForeground]);

	const scheduleActiveBranchSync = useCallback(
		(sessionId = selectedRef.current, delayMs = SELECTED_SESSION_REFRESH_DEBOUNCE_MS) => {
			if (!sessionId || sessionId !== selectedRef.current) return;
			if (selectedSyncTimer.current !== null) window.clearTimeout(selectedSyncTimer.current);
			selectedSyncTimer.current = window.setTimeout(() => {
				selectedSyncTimer.current = null;
				void refreshSelectedSessionState(sessionId).catch((error) => pushNotice("error", errorMessage(error)));
			}, delayMs);
		},
		[pushNotice, refreshSelectedSessionState],
	);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current) => {
			if (!sessionId) return null;
			return refreshSelectedSessionState(sessionId);
		},
		[refreshSelectedSessionState],
	);

	useEffect(() => {
		if (connection !== "open") return;
		if (!selectedId) {
			resetSelectedCache(null);
			setSelectedFetchState({ sessionId: null, loading: false });
			return;
		}
		const version = ++selectedLoadVersion.current;
		const currentSnapshot = selectedCacheRef.current.sessionId === selectedId ? selectedCacheRef.current.snapshot : null;
		setSelectedFetchState({
			sessionId: selectedId,
			loading: !currentSnapshot,
		});
		void refreshSelectedSessionState(selectedId)
			.then(() => {
				if (selectedLoadVersion.current !== version || selectedRef.current !== selectedId) return;
				setSelectedFetchState({
					sessionId: selectedId,
					loading: false,
				});
			})
			.catch((error) => {
				if (selectedLoadVersion.current !== version || selectedRef.current !== selectedId) return;
				setSelectedFetchState({
					sessionId: selectedId,
					loading: false,
				});
				pushNotice("error", errorMessage(error));
			});
	}, [connection, pushNotice, refreshSelectedSessionState, resetSelectedCache, selectedId]);

	const handleSessionEvent = useCallback(
		(event: EventFrame) => {
			const currentSessions = queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(selectedProjectRef.current));
			const eventSession = currentSessions?.find((session) => session.session_id === event.session_id);
			if (eventSession && eventSession.project_id !== selectedProjectRef.current) return;
			const previousEventId = lastEventIds.current.get(event.session_id) ?? 0;
			if (event.event_id <= previousEventId) return;

			const refreshPlan = refreshPlanForEvent(event);
			lastEventIds.current.set(event.session_id, event.event_id);
			let shouldSyncSelected = refreshPlan.syncSelected && event.session_id === selectedRef.current;
			if (event.session_id === selectedRef.current) {
				const queue = queueProjectionFromEvent(event);
				if (queue) {
					const next = replaceSelectedCache(
						applyEventHighWater(
							applyQueueProjection(selectedCacheRef.current, event.session_id, queue),
							event.session_id,
							event.event_id,
						),
					);
					shouldSyncSelected = false;
				}
				if (event.event === "transcript.appended") {
					const applied = applyTranscriptAppendedEvent(selectedCacheRef.current, event);
					replaceSelectedCache(applied.cache);
					shouldSyncSelected = applied.result === "refresh";
				} else if (isTranscriptSideChannelEvent(event)) {
					const entryId = eventEntryId(event);
					if (entryId && selectedCacheRef.current.entriesById.has(entryId)) {
						replaceSelectedCache(applyEventHighWater(selectedCacheRef.current, event.session_id, event.event_id));
					}
				}
				const activity = activityFromEvent(event);
				if (activity) {
					replaceSelectedCache(mergeSessionActivityEvent(selectedCacheRef.current, event.session_id, event.event_id, activity));
				}
			}
			if (shouldSyncSelected) scheduleActiveBranchSync(event.session_id);
			const activity = activityFromEvent(event);
			patchSessionListEventSummary(queryClient, selectedProjectRef.current, event, activity);
			if (refreshPlan.refreshList) {
				scheduleSessionListRefresh();
				if (loadedSnapshot?.session_id) {
					// The run board reads the stage.* surface; the backend emits no
					// dedicated stage events, so the subagent lifecycle events (and the
					// completion steer landing as a normal parent message) are the signal
					// to refresh the board. The 2s poll covers any missed event.
					void queryClient.invalidateQueries({ queryKey: queryKeys.stages(loadedSnapshot.session_id) });
				}
			}

			if (event.session_id === selectedRef.current) {
				if (event.event === "model.error") pushNotice("error", modelErrorNotice(event.data));
				if (event.event === "compaction.requested") pushNotice("info", compactionRequestedNotice(event.data));
				if (event.event === "compaction.completed") pushNotice("success", compactionCompletedNotice(event.data));
				if (event.event === "compaction.error") pushNotice("error", compactionErrorNotice(event.data));
				if (event.event === "subagent.running") pushNotice("info", subagentRunningNotice(event.data));
				if (event.event === "subagent.idle") {
					const outcome = typeof event.data.outcome === "string" ? event.data.outcome : null;
					const level = outcome === "Crashed" ? "error" : outcome === "Interrupted" ? "info" : "success";
					pushNotice(level, subagentIdleNotice(event.data));
				}
				if (event.event === "turn.finished") {
					const outcome = typeof event.data.outcome === "string" ? event.data.outcome : null;
					if (outcome === "Interrupted") pushNotice("info", "turn interrupted");
					if (outcome === "Crashed") pushNotice("error", "turn crashed");
				}
			}
		},
		[
			pushNotice,
			queryClient,
			replaceSelectedCache,
			scheduleActiveBranchSync,
			scheduleSessionListRefresh,
			loadedSnapshot?.session_id,
		],
	);

	useEffect(() => {
		handleSessionEventRef.current = handleSessionEvent;
	}, [handleSessionEvent]);

	useEffect(() => {
		const offStatus = api.onStatus((status) => {
			setConnection(status);
			subscribedEventSessionIds.current.clear();
			if (status !== "open") return;
			void Promise.all([
				queryClient.invalidateQueries({ queryKey: queryKeys.projects }),
				queryClient.invalidateQueries({ queryKey: queryKeys.systemPromptRoot }),
				queryClient.invalidateQueries({
					queryKey: queryKeys.sessions(selectedProjectRef.current),
				}),
			]).catch((error) => pushNotice("error", errorMessage(error)));
		});
		const offEvent = api.onEvent((event) => handleSessionEventRef.current(event));
		void api.connect().catch((error) => pushNotice("error", errorMessage(error)));
		return () => {
			offStatus();
			offEvent();
			if (selectedSyncTimer.current !== null) window.clearTimeout(selectedSyncTimer.current);
			if (sessionListRefreshTimer.current !== null) window.clearTimeout(sessionListRefreshTimer.current);
			api.close();
		};
	}, [api, pushNotice, queryClient]);

	useEffect(() => {
		if (projectsQuery.error) pushNotice("error", errorMessage(projectsQuery.error));
	}, [projectsQuery.error, pushNotice]);
	useEffect(() => {
		if (sessionsQuery.error) pushNotice("error", errorMessage(sessionsQuery.error));
	}, [sessionsQuery.error, pushNotice]);
	useEffect(() => {
		if (toolsQuery.error) pushNotice("error", errorMessage(toolsQuery.error));
	}, [toolsQuery.error, pushNotice]);

	useEffect(() => {
		if (projectsQuery.status !== "success") return;
		const currentProjectId = selectedProjectRef.current;
		if (currentProjectId === null || projects.some((project) => project.project_id === currentProjectId)) return;
		selectProjectSession(null, null);
		setQuery("");
		composerHandleRef.current?.setValue("");
	}, [projects, projectsQuery.status, selectProjectSession]);

	useEffect(() => {
		if (!selectedId) return;
		if (sessionItems.some((session) => session.session_id === selectedId)) return;
		if (selectedFetchState.sessionId === selectedId && selectedFetchState.loading) return;
		if (loadedSnapshot?.session_id === selectedId) return;
		selectSession(null);
	}, [
		loadedSnapshot?.session_id,
		selectSession,
		selectedFetchState.loading,
		selectedFetchState.sessionId,
		selectedId,
		sessionItems,
	]);

	useEffect(() => {
		if (!loadedSnapshot) return;
		const observedEventId = lastEventIds.current.get(loadedSnapshot.session_id) ?? 0;
		lastEventIds.current.set(loadedSnapshot.session_id, Math.max(observedEventId, loadedSnapshot.last_event_id));
		queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(loadedSnapshot.project_id), (current) =>
			mergeSnapshotIntoSessionList(current, loadedSnapshot),
		);
		// `last_event_id` is a transient replay cursor for the daemon's in-memory-ish
		// event buffer. The daemon may clear old event rows after a session becomes
		// idle, so a fresh `session.get` can legitimately report a smaller cursor
		// than this tab has already observed. Revisions and explicit
		// foreground/reconnect reconciliation drive freshness; never use the event
		// cursor mismatch as a durable selected-session refresh trigger.
	}, [loadedSnapshot, queryClient]);

	const ensureTreeIndex = useCallback(
		async (
			sessionId: string,
			options: {
				forceRestart?: boolean;
				onPage?: (nodes: TranscriptTreeNode[], complete: boolean) => void;
			} = {},
		): Promise<TranscriptTreeNode[]> => {
			const forceRestart = options.forceRestart ?? false;
			const initialCache = selectedCacheRef.current.sessionId === sessionId ? selectedCacheRef.current : emptySelectedSessionCache(sessionId);
			const snapshotRevision = initialCache.snapshot?.transcript_revision ?? null;
			const initialNodes = treeNodesInOrder(initialCache);
			if (
				!forceRestart &&
				initialCache.treeComplete &&
				(snapshotRevision === null || initialCache.treeTranscriptRevision === null || initialCache.treeTranscriptRevision === snapshotRevision)
			) {
				return initialNodes;
			}
			let afterSequence =
				!forceRestart && (initialCache.treeTranscriptRevision === snapshotRevision || snapshotRevision === null)
					? initialCache.treeLoadedPrefixSequence
					: 0;
			let complete = false;
			let nodes = afterSequence > 0 ? initialNodes : [];
			if (nodes.length > 0) options.onPage?.(nodes, false);
			while (!complete && selectedRef.current === sessionId) {
				const shouldLogPerf = perfEnabled();
				const startedAt = perfNow();
				if (shouldLogPerf) perfLog("transcript.index start", { sessionId, afterSequence });
				const index = await api.getTranscriptIndex(sessionId, {
					afterSequence,
					limit: TRANSCRIPT_INDEX_PAGE_SIZE,
				});
				if (shouldLogPerf) {
					perfLog("transcript.index end", {
						sessionId,
						nodes: index.nodes.length,
						approxBytes: approximateJsonSize(index),
						rpcMs: Math.round(perfNow() - startedAt),
						complete: index.complete,
					});
				}
				if (selectedRef.current !== sessionId) break;
				const nextCache = updateSelectedCache((current) => {
					const base = current.sessionId === sessionId ? current : emptySelectedSessionCache(sessionId);
					return applyTreeIndex(base, index);
				});
				nodes = treeNodesInOrder(nextCache);
				afterSequence = nextCache.treeLoadedPrefixSequence;
				complete = nextCache.treeComplete;
				options.onPage?.(nodes, complete);
			}
			return nodes;
		},
		[api],
	);

	useEffect(() => {
		if (connection !== "open") return;
		const selectedHasEventCursor =
			!!selectedId && (lastEventIds.current.has(selectedId) || loadedSnapshot?.session_id === selectedId);
		const desiredSessionIds = new Set<string>();
		for (const session of sessions) {
			desiredSessionIds.add(session.session_id);
		}
		for (const stageSubagentId of stageSubagentIds) {
			desiredSessionIds.add(stageSubagentId);
		}
		if (selectedId && selectedHasEventCursor) desiredSessionIds.add(selectedId);
		for (const sessionId of Array.from(subscribedEventSessionIds.current)) {
			if (desiredSessionIds.has(sessionId)) continue;
			subscribedEventSessionIds.current.delete(sessionId);
			if (api.isOpen()) {
				void api.unsubscribeEvents(sessionId).catch(() => undefined);
			}
		}
		for (const sessionId of desiredSessionIds) {
			if (subscribedEventSessionIds.current.has(sessionId)) continue;
			subscribedEventSessionIds.current.add(sessionId);
			const afterEventId =
				lastEventIds.current.get(sessionId) ?? (loadedSnapshot?.session_id === sessionId ? loadedSnapshot.last_event_id : null);
			void api
				.subscribeEvents(sessionId, afterEventId)
				.then((replayed) => {
					if (!subscribedEventSessionIds.current.has(sessionId)) return undefined;
					for (const event of replayed) handleSessionEvent(event);
					return undefined;
				})
				.catch((error) => {
					subscribedEventSessionIds.current.delete(sessionId);
					pushNotice("error", errorMessage(error));
				});
		}
	}, [
		api,
		connection,
		handleSessionEvent,
		loadedSnapshot?.last_event_id,
		loadedSnapshot?.session_id,
		pushNotice,
		selectedId,
		sessions,
		stageSubagentIds,
	]);

	const configureProvider = useCallback(
		async (provider: ProviderConfig) => {
			const sessionId = selectedRef.current;
			if (!sessionId) {
				setNewSessionProvider(provider);
				return;
			}
			const result = await api.configureSession({ sessionId, provider });
			patchSessionListProvider(queryClient, selectedProjectRef.current, sessionId, provider);
			patchSelectedSnapshot(sessionId, (snapshot) => ({
				...snapshot,
				provider,
				metadata: result.metadata ?? snapshot.metadata,
				activity: result.activity,
			}));
			invalidateSessionList();
		},
		[api, invalidateSessionList, patchSelectedSnapshot, queryClient],
	);

	const changeModel = useCallback(
		async (modelKey: string) => {
			if (modelLocked) {
				pushNotice("info", "model is locked after the first transcript entry");
				return;
			}
			await configureProvider(providerFromModelKey(modelKey, activeProvider));
		},
		[activeProvider, configureProvider, modelLocked, pushNotice],
	);

	const changeReasoningEffort = useCallback(
		async (effort: ReasoningEffort) => {
			await configureProvider(withReasoningEffort(activeProvider, effort));
		},
		[activeProvider, configureProvider],
	);

	const filteredSessions = useMemo(() => {
		const q = query.trim().toLowerCase();
		const visibleSessions = showArchived ? sessionItems : sessionItems.filter((session) => !isArchivedSession(session));
		if (!q) return visibleSessions;
		return visibleSessions.filter((session) => {
			const title = sessionTitle(session).toLowerCase();
			return title.includes(q) || session.session_id.toLowerCase().includes(q);
		});
	}, [query, sessionItems, showArchived]);

	const activeSessionItems = useMemo(() => sessionItems.filter((session) => !isArchivedSession(session)), [sessionItems]);
	const counts = useMemo(() => tallyActivities(activeSessionItems), [activeSessionItems]);
	const archivedCount = sessionItems.length - activeSessionItems.length;

	const openRenameDialog = useCallback((session: SessionListItem) => {
		setRenameSessionId(session.session_id);
		setRenameValue(sessionTitle(session));
	}, []);

	const closeRenameDialog = useCallback(() => {
		setRenameSessionId(null);
		setRenameValue("");
	}, []);

	const renameSession = useCallback(async () => {
		if (!renameSessionId) return;
		const title = renameValue.trim();
		if (!title) throw new Error("session title is required");
		const result = await api.renameSession(renameSessionId, title);
		patchSessionListMetadata(queryClient, selectedProjectRef.current, renameSessionId, { title });
		patchSelectedSnapshot(renameSessionId, (snapshot) => ({
			...snapshot,
			metadata: result.metadata ?? { ...snapshot.metadata, title },
			activity: result.activity,
		}));
		invalidateSessionList();
		pushNotice("success", `renamed session to “${truncate(title, 80)}”`);
		closeRenameDialog();
	}, [api, closeRenameDialog, invalidateSessionList, patchSelectedSnapshot, pushNotice, queryClient, renameSessionId, renameValue]);

	const setSessionArchived = useCallback(
		async (session: SessionListItem, archived: boolean) => {
			const sessionId = session.session_id;
			const currentSnapshot = loadedSnapshot?.session_id === sessionId ? loadedSnapshot : null;
			const activity = currentSnapshot?.activity ?? session.activity;
			if (activity !== "idle") throw new Error("only idle sessions can be archived");
			const metadata = { ...(currentSnapshot?.metadata ?? session.metadata) };
			if (archived) metadata.archived = true;
			else delete metadata.archived;
			const result = await api.configureSession({
				sessionId,
				provider: currentSnapshot?.provider ?? session.provider,
				metadata,
			});
			patchSessionListMetadata(
				queryClient,
				selectedProjectRef.current,
				sessionId,
				archived ? { archived: true } : {},
				archived ? [] : ["archived"],
			);
			patchSelectedSnapshot(sessionId, (snapshot) => ({
				...snapshot,
				metadata: result.metadata ?? metadata,
				activity: result.activity,
			}));
			invalidateSessionList();
			pushNotice(
				"success",
				archived ? `archived “${truncate(sessionTitle(session), 80)}”` : `unarchived “${truncate(sessionTitle(session), 80)}”`,
			);
		},
		[api, invalidateSessionList, loadedSnapshot, patchSelectedSnapshot, pushNotice, queryClient],
	);

	const closeDeleteDialog = useCallback(() => {
		setDeleteDialog(null);
	}, []);

	const deleteSession = useCallback(async () => {
		if (!deleteDialog || deleteDialog.deleting) return;
		setDeleteDialog((current) => (current ? { ...current, deleting: true } : current));
		const session = deleteDialog.session;
		const sessionId = session.session_id;
		const title = sessionTitle(session);
		try {
			const current = sessionId === selectedRef.current ? await refreshSelected(sessionId) : null;
			const activity = current?.snapshot.activity ?? session.activity;
			if (activity !== "idle") throw new Error("only idle sessions can be deleted");

			await api.deleteSession(sessionId);
			if (selectedSyncTimer.current !== null) {
				window.clearTimeout(selectedSyncTimer.current);
				selectedSyncTimer.current = null;
			}
			lastEventIds.current.delete(sessionId);
			dropSelectedCache(sessionId);
			queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(selectedProjectRef.current), (current) =>
				current?.filter((candidate) => candidate.session_id !== sessionId),
			);
			composerHandleRef.current?.clearSession(sessionId);

			if (selectedRef.current === sessionId) {
				selectSession(null);
				composerHandleRef.current?.setValue("");
			}

			closeDeleteDialog();
			invalidateSessionList();
			pushNotice("success", `deleted “${truncate(title, 80)}”`);
		} catch (error) {
			setDeleteDialog((current) => (current?.session.session_id === sessionId ? { ...current, deleting: false } : current));
			throw error;
		}
	}, [api, closeDeleteDialog, deleteDialog, dropSelectedCache, invalidateSessionList, pushNotice, queryClient, refreshSelected, selectSession]);

	const createSession = useCallback(
		(title?: string) => {
			nextSessionTitleRef.current = title?.trim() || null;
			selectSession(null);
			composerHandleRef.current?.setValue("");
			requestAnimationFrame(() => composerHandleRef.current?.focus());
			return null;
		},
		[selectSession],
	);

	const requireSelected = useCallback(() => {
		if (!selectedRef.current) throw new Error("select a session first");
		return selectedRef.current;
	}, []);

	const queueUserInput = useCallback(
		async (text: string) => {
			const sessionId = requireSelected();
			if (!loadedSnapshot && selectedRef.current === sessionId) {
				throw new Error("session is still loading");
			}
			if (selectedSession && isArchivedSession(selectedSession)) {
				const current = loadedSnapshot?.session_id === sessionId ? loadedSnapshot : (await refreshSelected(sessionId))?.snapshot;
				if ((current?.activity ?? selectedSession.activity) !== "idle") {
					throw new Error("only idle archived sessions can be resumed");
				}
				const metadata = { ...(current?.metadata ?? selectedSession.metadata) };
				delete metadata.archived;
				const result = await api.configureSession({
					sessionId,
					provider: current?.provider ?? selectedSession.provider,
					metadata,
				});
				patchSessionListMetadata(queryClient, selectedProjectRef.current, sessionId, {}, ["archived"]);
				patchSelectedSnapshot(sessionId, (snapshot) => ({
					...snapshot,
					metadata: result.metadata ?? metadata,
					activity: result.activity,
				}));
				invalidateSessionList();
			}
			const clientInputId = randomId("web_input");
			const content = textContent(text);
			try {
				const result = await api.queueFollowUp({
					sessionId,
					clientInputId,
					expectedActiveLeafId: loadedSnapshot?.activity === "idle" ? (loadedSnapshot.active_leaf_id ?? null) : undefined,
					baseLeafId: selectedBaseLeafId(selectedCacheRef.current, sessionId, loadedSnapshot?.active_leaf_id ?? null),
					content,
				});
				if (result.queue) {
					updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue!));
				}
				if (result.queued) {
					invalidateSessionList();
				} else {
					if (result.active_branch_sync) {
						const overview = {
							...result.active_branch_sync.overview,
							active_leaf_id: result.active_branch_sync.active_leaf_id,
						};
						commitSelectedSnapshot(overview);
						lastEventIds.current.set(sessionId, Math.max(lastEventIds.current.get(sessionId) ?? 0, overview.last_event_id));
					} else if (result.active_branch) {
						updateSelectedCache((current) =>
							applySwitchResultToCache(
								current.sessionId === sessionId ? current : selectedCacheRef.current,
								result.active_branch!,
							),
						);
						if (result.active_branch.last_event_id !== undefined) {
							lastEventIds.current.set(sessionId, result.active_branch.last_event_id);
						}
					} else {
						try {
							await syncActiveBranchNow(sessionId);
						} catch (error) {
							pushNotice("error", errorMessage(error));
						}
					}
				}
				void refreshTranscriptTurns(sessionId).catch((error) => pushNotice("error", errorMessage(error)));
			} catch (error) {
				composerHandleRef.current?.restoreSubmittedDraft(sessionId, text);
				throw error;
			}
		},
		[
			api,
			commitSelectedSnapshot,
			invalidateSessionList,
			loadedSnapshot,
			patchSelectedSnapshot,
			pushNotice,
			refreshTranscriptTurns,
			refreshSelected,
			requireSelected,
			selectedSession,
			updateSelectedCache,
		],
	);

	const startNewSession = useCallback(
		async (text: string) => {
			const projectId = selectedProjectRef.current;
			const sessionId = randomId("session");
			const title = nextSessionTitleRef.current || titleFromText(text);
			nextSessionTitleRef.current = null;
			const result = await api.startSession({
				sessionId,
				projectId,
				provider: newSessionProvider,
				metadata: {
					title,
					created_by: "web",
					compaction: {
						config: {
							auto_enabled: true,
							remote_mode: "auto",
							max_consecutive_failures: 3,
						},
					},
				},
				clientInputId: randomId("web_start"),
				priority: "follow_up",
				content: textContent(text),
				workspaces: projectId ? startWorkspacesFromScope(workspaceScopeRef.current) : undefined,
			});
			await queryClient.invalidateQueries({
				queryKey: queryKeys.sessions(projectId),
			});
			selectSession(result.session_id);
			return result.session_id;
		},
		[api, newSessionProvider, queryClient, selectSession],
	);

	const switchToTarget = useCallback(
		async (target: HistoryTargetOption) => {
			const sessionId = requireSelected();
			if (!loadedSnapshot || loadedSnapshot.session_id !== sessionId) {
				throw new Error("session is still loading");
			}
			if (loadedSnapshot.activity !== "idle") {
				throw new Error("stop the active turn before switching history");
			}
			const targetBranchIds = branchFromTree(selectedCacheRef.current, target.actionLeafId).map((node) => node.id);
			if (target.actionLeafId && !targetBranchIds.includes(target.actionLeafId)) {
				throw new Error("history index is still loading; please wait for the switch list to finish");
			}
			const restoreText = await restoreTextForTarget(api, sessionId, target, selectedCacheRef, updateSelectedCache);
			let result;
			try {
				result = await api.switchHistory({
					sessionId,
					leafId: target.actionLeafId,
					expectedActiveLeafId: target.expectedActiveLeafId ?? loadedSnapshot.active_leaf_id ?? null,
					expectedTranscriptRevision: selectedCacheRef.current.treeTranscriptRevision ?? loadedSnapshot.transcript_revision ?? null,
					activeBranchEntryIds: targetBranchIds,
					missingBodyIds: [],
				});
			} catch (error) {
				if (isHistoryChangedError(error)) {
					await ensureTreeIndex(sessionId, { forceRestart: true });
					throw new Error("history changed; refreshed the switch list, please choose again");
				}
				throw error;
			}
			if (restoreText !== null) composerHandleRef.current?.setValue(restoreText);
			updateSelectedCache((current) => applySwitchResultToCache(current.sessionId === sessionId ? current : selectedCacheRef.current, result));
			await refreshTranscriptTurns(sessionId);
			if (result.last_event_id !== undefined) lastEventIds.current.set(sessionId, result.last_event_id);
			invalidateSessionList();
			pushNotice("success", restoreText !== null ? "message restored for editing" : "switched to selected history point");
		},
		[
			api,
			ensureTreeIndex,
			invalidateSessionList,
			loadedSnapshot,
			pushNotice,
			refreshTranscriptTurns,
			requireSelected,
			updateSelectedCache,
		],
	);

	const handleSwitchHistoryTarget = useCallback(
		(target: HistoryTargetOption) => {
			const sessionId = selectedRef.current;
			setHistoryDialog(null);
			if (sessionId) setHistorySwitchingSessionId(sessionId);
			void switchToTarget(target)
				.catch((error) => pushNotice("error", errorMessage(error)))
				.finally(() => {
					if (sessionId) {
						setHistorySwitchingSessionId((current) => (current === sessionId ? null : current));
					}
				});
		},
		[pushNotice, switchToTarget],
	);

	const promoteQueuedInput = useCallback(
		async (inputId: string) => {
			const sessionId = requireSelected();
			const result = await api.promoteQueuedInput(sessionId, inputId);
			if (result.queue) {
				updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue!));
			}
			await queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectRef.current) });
			if (!result.promoted && result.status !== "queued") {
				pushNotice("info", "message is already being processed");
			}
		},
		[api, pushNotice, queryClient, requireSelected, updateSelectedCache],
	);

	const updateQueuedInput = useCallback(
		async (inputId: string, text: string) => {
			const sessionId = requireSelected();
			const queueRevision = selectedCacheRef.current.sessionId === sessionId ? selectedCacheRef.current.snapshot?.queue_revision : undefined;
			const result = await api.updateQueuedInput(sessionId, inputId, textContent(text), queueRevision);
			updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue));
			if (!result.updated && result.reason === "queue_changed") pushNotice("info", "queue changed; refreshed");
			if (!result.updated && result.reason === "not_editable") pushNotice("info", "message is no longer editable");
			invalidateSessionList();
		},
		[api, invalidateSessionList, pushNotice, requireSelected, updateSelectedCache],
	);

	const cancelQueuedInput = useCallback(
		async (inputId: string) => {
			const sessionId = requireSelected();
			const queueRevision = selectedCacheRef.current.sessionId === sessionId ? selectedCacheRef.current.snapshot?.queue_revision : undefined;
			const result = await api.cancelQueuedInput(sessionId, inputId, queueRevision);
			updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue));
			if (!result.cancelled && result.reason === "queue_changed") pushNotice("info", "queue changed; refreshed");
			if (!result.cancelled && result.reason === "not_editable") pushNotice("info", "message is no longer cancellable");
			invalidateSessionList();
		},
		[api, invalidateSessionList, pushNotice, requireSelected, updateSelectedCache],
	);

	const reorderQueuedInput = useCallback(
		async (inputId: string, direction: "up" | "down") => {
			const sessionId = requireSelected();
			const cache = selectedCacheRef.current;
			const followUps = (cache.sessionId === sessionId ? cache.snapshot?.queued_inputs : loadedSnapshot?.queued_inputs ?? [])
				?.filter((input) => input.priority === "follow_up" && input.status === "queued") ?? [];
			const currentIndex = followUps.findIndex((input) => input.input_id === inputId);
			const targetIndex = direction === "up" ? currentIndex - 1 : currentIndex + 1;
			if (currentIndex < 0 || targetIndex < 0 || targetIndex >= followUps.length) return;
			const nextOrder = followUps.map((input) => input.input_id);
			[nextOrder[currentIndex], nextOrder[targetIndex]] = [nextOrder[targetIndex], nextOrder[currentIndex]];
			const queueRevision = cache.sessionId === sessionId ? cache.snapshot?.queue_revision : undefined;
			const result = await api.reorderQueuedFollowUps(sessionId, nextOrder, queueRevision);
			updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue));
			if (!result.reordered && result.reason === "queue_changed") pushNotice("info", "queue changed; refreshed");
			invalidateSessionList();
		},
		[api, invalidateSessionList, loadedSnapshot?.queued_inputs, pushNotice, requireSelected, updateSelectedCache],
	);

	const stopActiveTurn = useCallback(async () => {
		const sessionId = requireSelected();
		setStopping(true);
		try {
			await api.interrupt(sessionId);
			await Promise.all([
				syncActiveBranchNow(sessionId),
				queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectRef.current) }),
			]);
		} catch (error) {
			pushNotice("error", errorMessage(error));
		} finally {
			setStopping(false);
		}
	}, [api, pushNotice, queryClient, requireSelected, syncActiveBranchNow]);

	const invalidateStages = useCallback(() => {
		if (loadedSnapshot?.session_id) {
			void queryClient.invalidateQueries({ queryKey: queryKeys.stages(loadedSnapshot.session_id) });
		}
	}, [loadedSnapshot?.session_id, queryClient]);

	const cancelStage = useCallback(
		(stageId: string) => {
			const parentSessionId = loadedSnapshot?.session_id;
			if (!parentSessionId) return;
			void api
				.cancelStage(parentSessionId, stageId)
				.then(() => invalidateStages())
				.catch((error) => pushNotice("error", errorMessage(error)));
		},
		[api, invalidateStages, loadedSnapshot?.session_id, pushNotice],
	);

	// Steer the full subagent: a steer-priority message into the subagent's own
	// session (the composer only ever sends follow_up). The daemon rejects
	// steering a read-only subagent, so the board only offers this for the full.
	const steerSubagent = useCallback(
		(subagentSessionId: string) => {
			const text = window.prompt("Steer the full subagent with:");
			if (!text || !text.trim()) return;
			void api
				.steerSubagent({
					subagentSessionId,
					clientInputId: randomId("web_steer"),
					content: [{ type: "text", text: text.trim() }],
				})
				.then(() => pushNotice("success", "steered the subagent"))
				.catch((error) => pushNotice("error", errorMessage(error)));
		},
		[api, pushNotice],
	);

	const reRunStage = useCallback(
		(stage: Stage) => {
			const parentSessionId = loadedSnapshot?.session_id;
			if (!parentSessionId) return;
			const reRun = reRunParamsForStage(stage, parentSessionId);
			if (!reRun) {
				pushNotice("error", "cannot re-run: original prompts are unavailable");
				return;
			}
			const start =
				reRun.kind === "full" ? api.startFullStage(reRun.params) : api.startReadonlyFanout(reRun.params);
			void start
				.then(() => invalidateStages())
				.catch((error) => pushNotice("error", errorMessage(error)));
		},
		[api, invalidateStages, loadedSnapshot?.session_id, pushNotice],
	);

	const readHandoffFile = useCallback(
		async (stageId: string, subagentId: string | null, file: HandoffFileName): Promise<string> => {
			const parentSessionId = loadedSnapshot?.session_id;
			if (!parentSessionId) throw new Error("select a session first");
			const result = await api.readHandoffFile({ parentSessionId, stageId, subagentId, file });
			return result.content;
		},
		[api, loadedSnapshot?.session_id],
	);

	const resumeTerminalTurn = useCallback(
		async (leafId?: string | null) => {
			const sessionId = requireSelected();
			const current = await refreshSelected(sessionId);
			const activeLeafId = leafId ?? current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null;
			if (!activeLeafId) throw new Error("no terminal turn to resume");
			if ((current?.snapshot.activity ?? loadedSnapshot?.activity) !== "idle") {
				throw new Error("stop the active turn before retrying");
			}
			setResumingTurnId(activeLeafId);
			try {
				const result = await api.resumeTurn({
					sessionId,
					leafId: activeLeafId,
					expectedActiveLeafId: current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null,
				});
				await Promise.all([
					syncActiveBranchNow(sessionId),
					queryClient.invalidateQueries({ queryKey: queryKeys.sessions(selectedProjectRef.current) }),
				]);
				pushNotice("success", result.outcome === "Interrupted" ? "continued turn" : "retry started");
			} finally {
				setResumingTurnId(null);
			}
		},
		[api, loadedSnapshot?.active_leaf_id, loadedSnapshot?.activity, pushNotice, queryClient, refreshSelected, requireSelected, syncActiveBranchNow],
	);

	const openHistoryDialog = useCallback(
		() => {
			if (!loadedSnapshot) throw new Error("session is still loading");
			if (loadedSnapshot.activity !== "idle") {
				throw new Error("stop the active turn before switching history");
			}
			const sessionId = loadedSnapshot.session_id;
			const cache = selectedCacheRef.current;
			const treeRevisionMatches =
				loadedSnapshot.transcript_revision === undefined ||
				cache.treeTranscriptRevision === null ||
				cache.treeTranscriptRevision === loadedSnapshot.transcript_revision;
			const cachedNodes = cache.sessionId === sessionId && treeRevisionMatches ? treeNodesInOrder(cache) : [];
			const treeComplete = cache.sessionId === sessionId && treeRevisionMatches && cache.treeComplete;
			setHistoryDialog({
				sessionId,
				nodes: treeComplete ? cachedNodes : [],
				activeLeafId: loadedSnapshot.active_leaf_id,
				loading: !treeComplete,
				error: null,
			});
			void ensureTreeIndex(sessionId, {
				onPage: (nodes, complete) => {
					setHistoryDialog((current) => {
						if (!current || current.sessionId !== sessionId) return current;
						return {
							...current,
							nodes: complete ? nodes : [],
							activeLeafId: selectedCacheRef.current.treeActiveLeafId ?? loadedSnapshot.active_leaf_id,
							loading: !complete,
							error: null,
						};
					});
				},
			})
				.then((nodes) => {
					setHistoryDialog((current) => {
						if (!current || current.sessionId !== sessionId) return current;
						return {
							...current,
							nodes,
							activeLeafId: selectedCacheRef.current.treeActiveLeafId ?? current.activeLeafId,
							loading: false,
							error: null,
						};
					});
				})
				.catch((error) => {
					setHistoryDialog((current) => {
						if (!current || current.sessionId !== sessionId) return current;
						return {
							...current,
							loading: false,
							error: errorMessage(error),
						};
					});
				});
		},
		[ensureTreeIndex, loadedSnapshot],
	);

	const executeSlash = useCallback(
		async (parsed: ParsedSlash) => {
			const name = parsed.name;
			const args = parsed.args;
			const pushActionNotice = (tone: Notice["tone"], text: string) => {
				pushNotice(tone, `/${name}: ${text}`);
			};
			if (!name || name === "help") {
				pushActionNotice("info", `commands: ${COMMANDS.map((command) => `/${command.name}`).join(", ")}`);
				return;
			}
			if (!findCommand(name)) {
				throw new Error(`unknown command: /${name}`);
			}
			if (name === "system") {
				if (args) {
					pushActionNotice("info", "/system is read-only; edit PI.md in the repo to change the prompt");
					return;
				}
				if (!loadedSnapshot) {
					throw new Error("/system requires a selected session");
				}
				setPromptDialog({ loading: true, template: "", rendered: null, view: "rendered", error: null });
				try {
					const next = await queryClient.fetchQuery({
						queryKey: queryKeys.systemPrompt(loadedSnapshot.session_id),
						queryFn: () => api.getSystemPrompt(loadedSnapshot.session_id),
						staleTime: 0,
					});
					setPromptDialog({ loading: false, template: next.template, rendered: next.rendered, view: next.rendered ? "rendered" : "template", error: null });
				} catch (error) {
					setPromptDialog({ loading: false, template: "", rendered: null, view: "template", error: errorMessage(error) });
				}
				return;
			}

			const sessionId = requireSelected();
			if (!loadedSnapshot) throw new Error("session is still loading");
			if (name === "switch") {
				openHistoryDialog();
				return;
			}
			if (name === "export") {
				const current = await api.getSession(sessionId, { includeEntries: true, entryScope: SELECTED_SESSION_DISPLAY_SCOPE });
				if (selectedRef.current === sessionId) commitSelectedSnapshot(current);
				setExportDialog({
					entries: current.entries ?? [],
				});
				return;
			}
			if (name === "compact") {
				const result = await api.requestCompaction(sessionId);
				pushActionNotice("success", `compaction requested ${result.action_row_id ?? ""}`.trim());
				return;
			}
			throw new Error(`unknown command: /${name}`);
		},
		[api, commitSelectedSnapshot, loadedSnapshot, openHistoryDialog, pushNotice, queryClient, requireSelected],
	);

	const submitComposer = useCallback(
		async (text: string) => {
			if (!text.trim() || sending) return false;
			text = text.trim();
			const slash = parseSlash(text);
			setSending(true);
			try {
				if (slash) {
					await executeSlash(slash);
				} else {
					if (selectedRef.current) {
						await queueUserInput(text);
					} else {
						await startNewSession(text);
					}
				}
				return true;
			} catch (error) {
				pushNotice("error", errorMessage(error));
				return false;
			} finally {
				setSending(false);
			}
		},
		[executeSlash, pushNotice, queueUserInput, sending, startNewSession],
	);

	const canStop = !!selectedId && loadedSnapshot?.activity === "running";
	const queuedInputs = loadedSnapshot?.queued_inputs ?? [];
	const handleToggleArchived = useCallback(() => {
		setShowArchived((show) => !show);
	}, []);
	const handleSelectProject = useCallback(
		(projectId: string | null) => {
			if (projectId === selectedProjectRef.current) return;
			const nextSessionId = selectedSessionForProject(projectId);
			selectProjectSession(projectId, nextSessionId);
			setQuery("");
			if (!nextSessionId) composerHandleRef.current?.setValue("");
		},
		[selectProjectSession],
	);
	const openCreateProjectDialog = useCallback(() => {
		setProjectDialog({
			mode: "create",
			name: "",
			workspaces: selectedProject ? selectedProject.workspaces.map(workspaceDraftFromProject) : [newWorkspaceDraft()],
			saving: false,
		});
	}, [selectedProject]);
	const openEditProjectDialog = useCallback((project: Project) => {
		setProjectDialog({
			mode: "edit",
			projectId: project.project_id,
			name: projectTitle(project),
			workspaces: project.workspaces.map(workspaceDraftFromProject),
			saving: false,
		});
	}, []);
	const closeProjectDialog = useCallback(() => {
		setProjectDialog(null);
	}, []);
	const saveProjectDialog = useCallback(async () => {
		if (!projectDialog || projectDialog.saving) return;
		const name = projectDialog.name.trim();
		const workspaces = projectWorkspacesFromDrafts(projectDialog.workspaces);
		if (!name) throw new Error("project name is required");
		if (!workspaces.length) throw new Error("at least one workspace is required");
		setProjectDialog((current) => (current ? { ...current, saving: true } : current));
		try {
			const saved =
				projectDialog.mode === "create"
					? await api.createProject({
							name,
							workspaces,
							metadata: { created_by: "web" },
						})
					: await api.updateProject({
							projectId: projectDialog.projectId ?? "",
							name,
							workspaces,
						});
			await queryClient.invalidateQueries({ queryKey: queryKeys.projects });
			selectProjectSession(saved.project_id, null);
			pushNotice("success", `${projectDialog.mode === "create" ? "created" : "updated"} project “${truncate(saved.name, 80)}”`);
			closeProjectDialog();
		} catch (error) {
			setProjectDialog((current) => (current ? { ...current, saving: false } : current));
			throw error;
		}
	}, [api, closeProjectDialog, projectDialog, pushNotice, queryClient, selectProjectSession]);
	const handleSidebarNew = useCallback(() => {
		void createSession();
		if (panelModeRef.current !== "wide") setSidebarOpen(false);
	}, [createSession]);
	const handleArchiveToggle = useCallback(
		(session: SessionListItem) => {
			void setSessionArchived(session, !isArchivedSession(session)).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[pushNotice, setSessionArchived],
	);
	const handleSidebarDelete = useCallback((session: SessionListItem) => {
		setDeleteDialog({ session, deleting: false });
	}, []);
	const handleModelChange = useCallback(
		(value: string) => {
			void changeModel(value).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[changeModel, pushNotice],
	);
	const handleReasoningEffortChange = useCallback(
		(value: ReasoningEffort) => {
			void changeReasoningEffort(value).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[changeReasoningEffort, pushNotice],
	);
	const handleToggleRight = useCallback(() => {
		setRightOpen((open) => !open);
	}, []);
	const handleToggleSidebar = useCallback(() => {
		setSidebarOpen((open) => !open);
	}, []);
	const handleCloseDrawers = useCallback(() => {
		if (panelModeRef.current !== "wide") setSidebarOpen(false);
		if (panelModeRef.current === "compact") setRightOpen(false);
	}, []);
	const closeSidebarIfOverlay = useCallback(() => {
		if (panelModeRef.current !== "wide") setSidebarOpen(false);
	}, []);
	const handleSidebarSelectSession = useCallback((sessionId: string) => {
		if (panelModeRef.current === "wide") {
			selectSession(sessionId);
			return;
		}
		setSidebarOpen(false);
		if (sidebarSelectTimer.current !== null) window.clearTimeout(sidebarSelectTimer.current);
		sidebarSelectTimer.current = window.setTimeout(() => {
			sidebarSelectTimer.current = null;
			selectSession(sessionId);
		}, SIDEBAR_CLOSE_BEFORE_SELECT_MS);
	}, [selectSession]);
	useEffect(() => {
		const onKeyDown = (event: KeyboardEvent) => {
			if (event.key !== "Escape") return;
			if (panelModeRef.current !== "wide") setSidebarOpen(false);
			if (panelModeRef.current === "compact") setRightOpen(false);
		};
		window.addEventListener("keydown", onKeyDown);
		return () => window.removeEventListener("keydown", onKeyDown);
	}, []);
	useEffect(() => () => {
		if (sidebarSelectTimer.current !== null) window.clearTimeout(sidebarSelectTimer.current);
	}, []);
	useEffect(() => {
		if (typeof window.matchMedia !== "function") return;
		const queries = [window.matchMedia(MEDIUM_PANEL_QUERY), window.matchMedia(WIDE_PANEL_QUERY)];
		const syncPanelsToViewport = () => {
			const nextMode = panelModeForViewport();
			if (nextMode === panelModeRef.current) return;
			panelModeRef.current = nextMode;
			setPanelMode(nextMode);
			const defaults = defaultPanelState(nextMode);
			setSidebarOpen(defaults.sidebarOpen);
			setRightOpen(defaults.rightOpen);
		};
		for (const query of queries) query.addEventListener("change", syncPanelsToViewport);
		syncPanelsToViewport();
		return () => {
			for (const query of queries) query.removeEventListener("change", syncPanelsToViewport);
		};
	}, []);
	const handleResumeTurn = useCallback(
		(entryId: string) => {
			void resumeTerminalTurn(entryId).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[pushNotice, resumeTerminalTurn],
	);
	const handleStop = useCallback(() => {
		void stopActiveTurn();
	}, [stopActiveTurn]);
	const handlePromoteQueued = useCallback(
		(inputId: string) => {
			void promoteQueuedInput(inputId).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[promoteQueuedInput, pushNotice],
	);
	const handleUpdateQueued = useCallback(
		(inputId: string, text: string) => {
			void updateQueuedInput(inputId, text).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[pushNotice, updateQueuedInput],
	);
	const handleCancelQueued = useCallback(
		(inputId: string) => {
			void cancelQueuedInput(inputId).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[cancelQueuedInput, pushNotice],
	);
	const handleMoveQueued = useCallback(
		(inputId: string, direction: "up" | "down") => {
			void reorderQueuedInput(inputId, direction).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[pushNotice, reorderQueuedInput],
	);
	const mobileTitle = selectedChatSession
		? sessionTitle(selectedChatSession)
		: selectedProject
			? projectTitle(selectedProject)
			: "pi relay";
	const mobileActivity = selectedChatSession
		? displayActivity(loadedSnapshot?.activity ?? selectedChatSession.activity)
		: null;
	const mobileArchived = selectedChatSession ? isArchivedSession(selectedChatSession) : false;
	const mobileSessionStatus = selectedChatSession ? (mobileArchived ? "archived" : mobileActivity) : null;
	const sidebarIsOverlay = panelMode !== "wide";
	const inspectorIsOverlay = panelMode === "compact";
	const sidebarOverlayOpen = sidebarIsOverlay && sidebarOpen;
	const inspectorOverlayOpen = inspectorIsOverlay && rightOpen;
	const sidebarInert = sidebarIsOverlay && !sidebarOpen;
	const inspectorInert = inspectorIsOverlay && !rightOpen;
	const appClassName = `app-shell ${sidebarOpen ? "sidebar-open" : ""} ${rightOpen ? "inspector-open" : ""}`;

	return (
		<div className={appClassName}>
			<div className="mobile-topbar">
				<button
					className="icon-button"
					type="button"
					onClick={handleToggleSidebar}
					aria-label={sidebarOpen ? "close projects and sessions" : "open projects and sessions"}
					aria-expanded={sidebarOpen}
				>
					<Menu size={17} />
				</button>
				<div className="mobile-topbar-title">
					<span>{mobileTitle}</span>
					<div className="mobile-topbar-status">
						<span className={`connection-pill ${connection === "open" ? "online" : "offline"}`}>
							{connection === "open" ? "connected" : connection}
						</span>
						{mobileSessionStatus ? (
							<span className={`session-state ${mobileSessionStatus}`}>{mobileSessionStatus}</span>
						) : null}
					</div>
				</div>
				{inspectorIsOverlay && !rightOpen ? (
					<button
						className="icon-button"
						type="button"
						onClick={handleToggleRight}
						aria-label="open inspector"
						aria-expanded={rightOpen}
					>
						<PanelRightOpen size={17} />
					</button>
				) : null}
			</div>

			{sidebarOverlayOpen || inspectorOverlayOpen ? (
				<button className="drawer-scrim" type="button" aria-label="close panel" onClick={handleCloseDrawers} />
			) : null}

			<Sidebar
				counts={counts}
				total={activeSessionItems.length}
				archived={archivedCount}
				connection={connection}
				projects={projects}
				selectedProjectId={selectedProjectId}
				query={query}
				showArchived={showArchived}
				filteredSessions={filteredSessions}
				selectedId={selectedId}
				inert={sidebarInert}
				onQueryChange={setQuery}
				onToggleArchived={handleToggleArchived}
				onNew={handleSidebarNew}
				onClose={() => setSidebarOpen(false)}
				onSelectProject={(projectId) => {
					handleSelectProject(projectId);
				}}
				onNewProject={() => {
					openCreateProjectDialog();
					closeSidebarIfOverlay();
				}}
				onEditProject={(project) => {
					openEditProjectDialog(project);
					closeSidebarIfOverlay();
				}}
				onSelectSession={(sessionId) => {
					handleSidebarSelectSession(sessionId);
				}}
				onRename={(session) => {
					openRenameDialog(session);
					closeSidebarIfOverlay();
				}}
				onArchiveToggle={(session) => {
					handleArchiveToggle(session);
					closeSidebarIfOverlay();
				}}
				onDelete={(session) => {
					handleSidebarDelete(session);
					closeSidebarIfOverlay();
				}}
			/>

			<ChatPane
				session={selectedChatSession}
				snapshot={loadedSnapshot}
				entries={loadedEntries}
				turnCards={turnCardViews}
				transcriptLoading={transcriptLoading}
				modelOptions={MODEL_OPTIONS}
				modelValue={providerModelKey(activeProvider)}
				modelLocked={modelLocked}
				modelControlsDisabled={modelControlsDisabled}
				reasoningEfforts={reasoningEfforts}
				reasoningEffort={providerReasoningEffort(activeProvider)}
				rightOpen={rightOpen}
				selectedId={selectedId}
				resumingTurnId={resumingTurnId}
				onModelChange={handleModelChange}
				onReasoningEffortChange={handleReasoningEffortChange}
				onToggleRight={handleToggleRight}
				onResumeTurn={handleResumeTurn}
				onExpandTurn={expandTurn}
				onCollapseTurn={collapseTurn}
				loadingTurnId={loadingTurnId ?? autoLoadingTurnId}
				hasOlderTurns={selectedCache.sessionId === selectedId && selectedCache.turnHasMoreBefore}
				loadingOlderTurns={loadingOlderTurns}
				onLoadOlderTurns={loadOlderTranscriptTurns}
			/>

			<footer className="chat-dock" data-slot="chat-box">
				{!selectedId && selectedProject && workspaceScope.length ? (
					<WorkspaceScopePicker scope={workspaceScope} onChange={handleWorkspaceScopeChange} disabled={sending} />
				) : null}
				<Composer
					selectedId={selectedId}
					composerHandleRef={composerHandleRef}
					sending={sending}
					canStop={canStop}
					stopping={stopping}
					queuedInputs={queuedInputs}
					onSubmit={submitComposer}
					onStop={handleStop}
					onPromoteQueued={handlePromoteQueued}
					onUpdateQueued={handleUpdateQueued}
					onCancelQueued={handleCancelQueued}
					onMoveQueued={handleMoveQueued}
				/>
			</footer>

			<aside className="inspector" data-slot="inspector" inert={inspectorInert}>
				<Inspector
					snapshot={loadedSnapshot}
					stages={stages}
					stagesLoading={stagesQuery.isLoading}
					stagesError={errorMessageOrNull(stagesQuery.error)}
					runBoard={{
						onCancelStage: cancelStage,
						onSteerSubagent: steerSubagent,
						onReRunStage: reRunStage,
						readHandoffFile,
					}}
					tools={tools}
					onSelectSession={(sessionId) => {
						selectSession(sessionId);
						if (inspectorIsOverlay) setRightOpen(false);
					}}
					onClose={() => setRightOpen(false)}
				/>
			</aside>

			{renameSessionId ? (
				<RenameSessionDialog
					value={renameValue}
					onChange={setRenameValue}
					onClose={closeRenameDialog}
					onSubmit={() => {
						void renameSession().catch((error) => pushNotice("error", errorMessage(error)));
					}}
				/>
			) : null}

			{deleteDialog ? (
				<DeleteSessionDialog
					session={deleteDialog.session}
					deleting={deleteDialog.deleting}
					onClose={closeDeleteDialog}
					onConfirm={() => {
						void deleteSession().catch((error) => pushNotice("error", errorMessage(error)));
					}}
				/>
			) : null}

			{projectDialog ? (
				<ProjectDialog
					state={projectDialog}
					onChange={(patch) => setProjectDialog((current) => (current ? { ...current, ...patch } : current))}
					onClose={closeProjectDialog}
					onSubmit={() => {
						void saveProjectDialog().catch((error) => pushNotice("error", errorMessage(error)));
					}}
				/>
			) : null}

			{promptDialog ? (
				<SystemPromptDialog state={promptDialog} onChangeView={(view) => setPromptDialog((current) => (current ? { ...current, view } : current))} onClose={() => setPromptDialog(null)} />
			) : null}

			{historyDialog ? (
				<CompactHistoryPickerDialog
					nodes={historyDialog.nodes}
					activeLeafId={historyDialog.activeLeafId}
					loading={historyDialog.loading}
					error={historyDialog.error}
					onClose={() => setHistoryDialog(null)}
					onSwitch={handleSwitchHistoryTarget}
				/>
			) : null}
			{exportDialog ? (
				<ExportDialog
					entries={exportDialog.entries}
					onClose={() => setExportDialog(null)}
					onCopied={() => pushNotice("success", "export copied to clipboard")}
					onDownloaded={() => pushNotice("success", "export downloaded")}
					onError={(error) => pushNotice("error", errorMessage(error))}
				/>
			) : null}
			<NoticeStack notices={notices} rightOpen={rightOpen} />
		</div>
	);
}

function titleFromText(text: string): string {
	return truncate(firstLine(text).trim() || "New session", 64);
}

function errorMessageOrNull(error: unknown): string | null {
	return error === null || error === undefined ? null : errorMessage(error);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function isHistoryChangedError(error: unknown): boolean {
	return errorMessage(error).startsWith("history_changed:");
}

function modelErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "model request failed";
	return `model error: ${truncate(error, 420)}`;
}

function compactionRequestedNotice(data: Record<string, unknown>): string {
	const trigger = typeof data.trigger === "string" ? data.trigger : null;
	return trigger === "auto" ? "auto-compaction started" : "compaction started";
}

function compactionCompletedNotice(data: Record<string, unknown>): string {
	const trigger = typeof data.trigger === "string" ? data.trigger : null;
	const provider = typeof data.provider === "string" ? data.provider : null;
	const remote = data.remote === true;
	const prefix = trigger === "auto" ? "auto-compaction" : "compaction";
	if (provider === "openai" && remote) return `${prefix} completed with OpenAI provider-native compaction`;
	if (provider === "claude") return `${prefix} completed with Claude summary`;
	return `${prefix} completed`;
}

function compactionErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "compaction failed";
	const label = data.trigger === "auto" ? "auto-compaction error" : "compaction error";
	return `${label}: ${truncate(error, 420)}`;
}

function subagentLabel(data: Record<string, unknown>): string {
	const label =
		typeof data.role === "string" && data.role.trim() ? data.role.trim() : "subagent";
	const child = typeof data.child_session_id === "string" ? data.child_session_id.slice(0, 13) : "";
	return child ? `${label} ${child}` : label;
}

function subagentRunningNotice(data: Record<string, unknown>): string {
	return `${subagentLabel(data)} started`;
}

function subagentIdleNotice(data: Record<string, unknown>): string {
	const outcome = typeof data.outcome === "string" && data.outcome.trim() ? data.outcome.trim() : "completed";
	const preview =
		typeof data.summary_preview === "string" && data.summary_preview.trim()
			? `: ${truncate(data.summary_preview.trim(), 180)}`
			: "";
	return `${subagentLabel(data)} idle (${outcome})${preview}`;
}

function activityFromEvent(event: EventFrame): SessionSummary["activity"] | null {
	const activity = event.data.activity;
	if (activity === "idle" || activity === "queued" || activity === "running") return activity;
	if (event.event === "session.idle") return "idle";
	if (event.event === "input.queued") return "queued";
	if (
		event.event === "input.consumed" ||
		event.event === "input.accepted" ||
		event.event === "action.requested" ||
		event.event === "model.requested" ||
		event.event === "tool.requested" ||
		event.event === "tool.started" ||
		event.event === "compaction.requested" ||
		event.event === "compaction.completed" ||
		event.event === "compaction.error"
	) {
		return "running";
	}
	return null;
}

function isTranscriptSideChannelEvent(event: EventFrame): boolean {
	return event.event === "turn.started" || event.event === "turn.finished" || event.event === "assistant.message";
}

function eventEntryId(event: EventFrame): string | null {
	const entryId = event.data.entry_id;
	return typeof entryId === "string" ? entryId : null;
}

function selectedBaseLeafId(cache: SelectedSessionCache, sessionId: string, fallback: string | null): string | null {
	if (cache.sessionId !== sessionId) return fallback;
	return cache.activeBranchEntryIds.at(-1) ?? cache.snapshot?.active_leaf_id ?? fallback;
}

async function restoreTextForTarget(
	api: ReturnType<typeof createAgentApi>,
	sessionId: string,
	target: HistoryTargetOption,
	selectedCacheRef: RefObject<SelectedSessionCache>,
	updateSelectedCache: (updater: (current: SelectedSessionCache) => SelectedSessionCache) => SelectedSessionCache,
): Promise<string | null> {
	if (!target.restoreEntryId) return target.restoreText ?? null;
	const cached = selectedCacheRef.current.entriesById.get(target.restoreEntryId);
	if (cached?.item.type === "user_message") return contentBlocksToText(cached.item.content);
	const result = await api.getTranscriptEntries(sessionId, [target.restoreEntryId]);
	updateSelectedCache((current) => applyEntryBodies(current.sessionId === sessionId ? current : selectedCacheRef.current, sessionId, result.entries));
	const entry = result.entries.find((candidate) => candidate.id === target.restoreEntryId);
	if (entry?.item.type === "user_message") return contentBlocksToText(entry.item.content);
	throw new Error("could not load the full user message for editing");
}

function SystemPromptDialog({
	state,
	onChangeView,
	onClose,
}: {
	state: PromptDialogState;
	onChangeView: (view: "rendered" | "template") => void;
	onClose: () => void;
}) {
	const text = state.view === "rendered" ? (state.rendered ?? "") : state.template;
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={onClose}>
			<div
				className="rename-dialog system-prompt-dialog"
				role="dialog"
				aria-modal="true"
				aria-labelledby="system-prompt-dialog-title"
				onMouseDown={(event) => event.stopPropagation()}
			>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="system-prompt-dialog-title">PI.md</h2>
						<p>Rendered prompt and source template.</p>
					</div>
					<button className="plain-close-button" type="button" onClick={onClose} aria-label="close system prompt dialog">
						<X size={16} />
					</button>
				</div>
				<div className="system-prompt-tabs" role="tablist" aria-label="PI.md view">
					<button type="button" className={state.view === "rendered" ? "selected" : ""} onClick={() => onChangeView("rendered")} disabled={!state.rendered}>
						Rendered
					</button>
					<button type="button" className={state.view === "template" ? "selected" : ""} onClick={() => onChangeView("template")}>
						Template
					</button>
				</div>
				<div className="system-prompt-body">
					{state.loading ? <p className="muted">Loading PI.md…</p> : null}
					{state.error ? <p className="error-text">{state.error}</p> : null}
					{!state.loading && !state.error ? (
						state.view === "rendered" ? <MarkdownView text={text} /> : <pre>{text}</pre>
					) : null}
				</div>
			</div>
		</div>
	);
}


const MarkdownView = memo(function MarkdownView({ text }: { text: string }) {
	return (
		<div className="assistant-markdown system-prompt-markdown">
			<ReactMarkdown
				rehypePlugins={[rehypeRaw]}
				remarkPlugins={[remarkGfm]}
				components={markdownComponents}
			>
				{text}
			</ReactMarkdown>
		</div>
	);
});

function RenameSessionDialog({
	value,
	onChange,
	onClose,
	onSubmit,
}: {
	value: string;
	onChange: (value: string) => void;
	onClose: () => void;
	onSubmit: () => void;
}) {
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={onClose}>
			<div
				className="rename-dialog"
				role="dialog"
				aria-modal="true"
				aria-labelledby="rename-dialog-title"
				onMouseDown={(event) => event.stopPropagation()}
			>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="rename-dialog-title">Rename session</h2>
					</div>
					<button className="plain-close-button" type="button" onClick={onClose} aria-label="close rename dialog">
						<X size={16} />
					</button>
				</div>
				<form
					onSubmit={(event) => {
						event.preventDefault();
						onSubmit();
					}}
				>
					<label className="rename-field">
						<span>Session title</span>
						<input value={value} onChange={(event) => onChange(event.target.value)} autoFocus placeholder="Session title" required />
					</label>
					<div className="rename-actions">
						<button type="button" className="secondary-button" onClick={onClose}>
							Cancel
						</button>
						<button type="submit" className="primary-button">
							Save
						</button>
					</div>
				</form>
			</div>
		</div>
	);
}

function DeleteSessionDialog({
	session,
	deleting,
	onClose,
	onConfirm,
}: {
	session: SessionListItem;
	deleting: boolean;
	onClose: () => void;
	onConfirm: () => void;
}) {
	const title = sessionTitle(session);
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={deleting ? undefined : onClose}>
			<div
				className="rename-dialog"
				role="dialog"
				aria-modal="true"
				aria-labelledby="delete-dialog-title"
				onMouseDown={(event) => event.stopPropagation()}
			>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="delete-dialog-title">Delete session</h2>
					</div>
					<button className="plain-close-button" type="button" onClick={onClose} aria-label="close delete dialog" disabled={deleting}>
						<X size={16} />
					</button>
				</div>
				<div className="delete-dialog-body">
					<p>
						Delete <strong>{title}</strong> permanently?
					</p>
					<p className="muted">This removes the transcript, queued inputs, actions, and events for this session. This cannot be undone.</p>
				</div>
				<div className="rename-actions">
					<button type="button" className="secondary-button" onClick={onClose} disabled={deleting}>
						Cancel
					</button>
					<button type="button" className="primary-button destructive" onClick={onConfirm} disabled={deleting}>
						{deleting ? "Deleting..." : "Delete"}
					</button>
				</div>
			</div>
		</div>
	);
}

function ProjectDialog({
	state,
	onChange,
	onClose,
	onSubmit,
}: {
	state: ProjectDialogState;
	onChange: (patch: Partial<ProjectDialogState>) => void;
	onClose: () => void;
	onSubmit: () => void;
}) {
	const title = state.mode === "create" ? "New project" : "Project settings";
	const updateWorkspace = (index: number, patch: WorkspaceDraftPatch) => {
		onChange({
			workspaces: state.workspaces.map((workspace, workspaceIndex) =>
				workspaceIndex === index ? updateWorkspaceDraft(workspace, patch) : workspace,
			),
		});
	};
	const removeWorkspace = (index: number) => {
		onChange({ workspaces: state.workspaces.filter((_, workspaceIndex) => workspaceIndex !== index) });
	};
	const addWorkspace = () => {
		onChange({ workspaces: [...state.workspaces, newWorkspaceDraft()] });
	};
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={state.saving ? undefined : onClose}>
			<div
				className="rename-dialog project-dialog"
				role="dialog"
				aria-modal="true"
				aria-labelledby="project-dialog-title"
				onMouseDown={(event) => event.stopPropagation()}
			>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="project-dialog-title">{title}</h2>
					</div>
					<button className="plain-close-button" type="button" onClick={onClose} aria-label="close project dialog" disabled={state.saving}>
						<X size={16} />
					</button>
				</div>
				<form
					onSubmit={(event) => {
						event.preventDefault();
						onSubmit();
					}}
				>
					<label className="rename-field">
						<span>Project name</span>
						<input
							value={state.name}
							onChange={(event) => onChange({ name: event.target.value })}
							autoFocus
							placeholder="Project name"
							required
							disabled={state.saving}
						/>
					</label>
					<div className="workspace-editor">
						<div className="workspace-editor-head">
							<span>Workspaces</span>
							<button type="button" className="secondary-button" onClick={addWorkspace} disabled={state.saving}>
								Add workspace
							</button>
						</div>
						<div className="workspace-editor-list">
							{state.workspaces.map((workspace, index) => {
								return (
									<div className="workspace-card" key={index}>
										<div className="workspace-card-head">
											{workspace.kind === "git" ? <FolderGit2 size={14} /> : <Folder size={14} />}
											<span>{workspace.kind === "git" ? "Git repo" : "Local folder"}</span>
										</div>
										<div className="workspace-row">
											<label>
												<span>Type</span>
												<select
													value={workspace.kind}
													onChange={(event) => updateWorkspace(index, { kind: event.target.value as "git" | "local" })}
													disabled={state.saving}
												>
													<option value="git">Git repo</option>
													<option value="local">Local folder</option>
												</select>
											</label>
											<label>
												<span>Name</span>
												<input
													value={workspace.workspace_dir}
													onChange={(event) => updateWorkspace(index, { workspace_dir: event.target.value })}
													placeholder={workspace.kind === "local" ? "docs" : "pi-relay"}
													required
													disabled={state.saving}
												/>
											</label>
											<button
												type="button"
												className="secondary-button workspace-remove"
												onClick={() => removeWorkspace(index)}
												disabled={state.saving || state.workspaces.length <= 1}
											>
												Remove
											</button>
										</div>
										{workspace.kind === "local" ? (
											<label className="workspace-full-field">
												<span>Source path</span>
												<input
													value={workspace.source_path}
													onChange={(event) => updateWorkspace(index, { source_path: event.target.value })}
													placeholder="/Users/me/reference-docs"
													required
													disabled={state.saving}
												/>
											</label>
										) : (
											<div className="workspace-row git-fields">
												<label>
													<span>Remote URL</span>
													<input
														value={workspace.remote_url}
														onChange={(event) => updateWorkspace(index, { remote_url: event.target.value })}
														placeholder="https://github.com/me/pi-relay.git"
														required
														disabled={state.saving}
													/>
												</label>
												<label>
													<span>Branch</span>
													<input
														value={workspace.remote_branch}
														onChange={(event) => updateWorkspace(index, { remote_branch: event.target.value })}
														placeholder="main"
														required
														disabled={state.saving}
													/>
												</label>
											</div>
										)}
									</div>
								);
							})}
						</div>
					</div>
					<div className="rename-actions">
						<button type="button" className="secondary-button" onClick={onClose} disabled={state.saving}>
							Cancel
						</button>
						<button type="submit" className="primary-button" disabled={state.saving}>
							{state.saving ? "Saving..." : "Save"}
						</button>
					</div>
				</form>
			</div>
		</div>
	);
}
