import { useQueries, useQuery, useQueryClient, type QueryClient } from "@tanstack/react-query";
import {
	useCallback,
	useEffect,
	useMemo,
	useRef,
	useState,
	useSyncExternalStore,
	type CSSProperties,
	type KeyboardEvent as ReactKeyboardEvent,
	type PointerEvent as ReactPointerEvent,
	type RefObject,
} from "react";
import { ArrowUp, Bot, Menu, PanelRightOpen } from "lucide-react";
import { createAgentApi, type AgentApi } from "./agentApi.ts";
import { ChatPane } from "./chatPane.tsx";
import { clearAcknowledgedTranscriptDestination } from "./transcript.tsx";
import type {
	OlderTurnsLoadRequest,
	OlderTurnsLoadResult,
	TranscriptDestination,
	TranscriptTurnPageIdentity,
} from "./transcript.tsx";
import { Composer, type ComposerHandle } from "./composer.tsx";
import { routeComposerSubmission, type ComposerSubmission } from "./composerRouting.ts";
import {
	assertRemoteActionAllowed,
	composerTextNeedsConnection,
	ConnectionRecoveryBanner,
	ConnectionRetryController,
	remoteActionBlockedReason,
} from "./connectionRecovery.tsx";
import { CompactHistoryPickerDialog } from "./historyPickerCompact.tsx";
import { type HistoryTargetOption } from "./historyTargets.ts";
import { ExportDialog } from "./exportDialog.tsx";
import { buildCachedExportBlocks, type ExportBlock } from "./exportTranscript.ts";
import {
	DeleteSessionDialog,
	newWorkspaceDraft,
	ProjectDialog,
	projectWorkspacesFromDrafts,
	RenameSessionDialog,
	workspaceDraftFromProject,
	type ProjectDialogState,
} from "./entityDialogs.tsx";
import { randomId } from "./ids.ts";
import {
	Inspector,
	NoticeStack,
	RUN_BOARD_DEFAULT_DELEGATION_COUNT,
	RUN_BOARD_EXPANDED_DELEGATION_COUNT,
	Sidebar,
} from "./panels.tsx";
import { approximateJsonSize, perfEnabled, perfLog, perfNow } from "./perf.ts";
import { queryKeys } from "./queryKeys.ts";
import { isDelegationRunning } from "./delegationBoard.ts";
import { DelegationListRetryController } from "./delegationListRetryController.ts";
import {
	ProviderConfigurationController,
	type ProviderConfigurationTarget,
} from "./providerConfigurationController.ts";
import type { ConnectionStatus } from "./rpc.ts";
import { findCommand, type ParsedSlash } from "./slash.ts";
import { refreshPlanForEvent } from "./sessionEvents.ts";
import { stopSession } from "./stopSession.ts";
import {
	SystemPromptDialog,
	type SystemPromptDialogState,
} from "./systemPromptDialog.tsx";
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
	activeBranchEntriesForExport,
	branchFromTree,
	emptySelectedSessionCache,
	hasUsableSelectedSessionCache,
	mergeSessionActivityEvent,
	prependTranscriptTurns,
	queueProjectionFromEvent,
	captureSelectedSessionRefresh,
	commitSelectedSessionRefresh,
	selectedEntries,
	snapshotWithTranscriptTurnsMetadata,
	treeNodesInOrder,
	turnCardsInOrder,
	turnDetailEntries,
	type SelectedSessionCache,
} from "./selectedSessionCache.ts";
import { SessionListRequestCoordinator } from "./sessionListRequestCoordinator.ts";
import { useSelectedSessionStore } from "./selectedSessionStore.ts";
import {
	DEFAULT_PROVIDER,
	MODEL_OPTIONS,
	newSessionCompactionConfig,
	providerFromModelKey,
	providerModelKey,
	providerReasoningEffort,
	reasoningEffortsForProvider,
	textContent,
	withReasoningEffort,
} from "./sessionDefaults.ts";
import {
	IntermediateUiStateError,
	isSelectedSessionFetchError,
	SelectedSessionFetchCoordinator,
	shouldReportActionError,
} from "./selectedSessionFetchState.ts";
import {
	projectTitle,
	sessionTitle,
	isArchivedSession,
	sessionStatusWithDelegations,
	sortSessionsByLastUserMessage,
	type SessionListItem,
} from "./sessionList.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import {
	loadUiSelection,
	rememberUiSelection,
} from "./uiResume.ts";
import {
	rememberWorkspaceScope,
	startWorkspacesFromScope,
	workspaceScopeForProject,
	type WorkspaceScopeEntry,
} from "./workspaceScope.ts";
import {
	NewSessionSetup,
	type WorkspaceConfiguration,
} from "./newSessionSetup.tsx";
import { McpOAuthDialog } from "./mcpOAuthDialog.tsx";
import {
	clearMcpServerSelection,
	mcpSelectionForProviderChange,
	mcpSelectionPayloadForProvider,
	reconcileMcpSelection,
	type McpSelectionState,
} from "./mcpSelection.ts";
import {
	browserWorkspaceRouteHistory,
	fallbackExecutionConversation,
	hostRouteScope,
	legacyWorkspaceResume,
	messageRecipient,
	openAgentConversation,
	parseWorkspaceRoute,
	projectRouteScope,
	rootConversationRoute,
	selectRootRun,
	showConversation,
	unavailableConversationRoute,
	unavailableExecutionDetail,
	WorkspaceRouteHistory,
	type RouteNavigation,
	type WorkspaceRoute,
	type WorkspaceRouteParseResult,
	type WorkspaceRouteUnavailable,
} from "./workspaceRoute.ts";
import type {
	Activity,
	DelegationSubagent,
	EventFrame,
	ErrorNotice,
	McpInventory,
	McpLoginResult,
	Project,
	ProviderConfig,
	ReasoningEffort,
	SessionSnapshot,
	SessionSummary,
	ToolListing,
	TranscriptEntry,
	TranscriptTreeNode,
	TranscriptTurnsResult,
} from "./types.ts";

const MAX_ERROR_NOTICES = 24;
const ERROR_NOTICE_TTL_MS = 4000;
const SESSION_LIST_REFRESH_DEBOUNCE_MS = 250;
const SESSION_LIST_REFETCH_MS = 2000;
const BACKGROUND_SESSION_WARM_CONCURRENCY = 2;
const SELECTED_SESSION_REFRESH_DEBOUNCE_MS = 80;
const FOREGROUND_RECONCILE_THROTTLE_MS = 2000;
const FOREGROUND_RECONNECT_AFTER_MS = 5000;
const AWAKE_HEARTBEAT_MS = 1000;
const TRANSCRIPT_INDEX_PAGE_SIZE = 5000;
const TRANSCRIPT_TURN_PAGE_SIZE = 50;
const SELECTED_SESSION_DISPLAY_SCOPE = "active_branch" as const;
const SIDEBAR_CLOSE_BEFORE_SELECT_MS = 200;
const MEDIUM_PANEL_QUERY = "(min-width: 900px)";
const WIDE_PANEL_QUERY = "(min-width: 1280px)";
const SIDEBAR_WIDTH_STORAGE_KEY = "piRelaySidebarWidth:v1";
const DEFAULT_SIDEBAR_WIDTH = 320;
const MIN_SIDEBAR_WIDTH = 240;
const MAX_SIDEBAR_WIDTH = 480;
const SIDEBAR_KEYBOARD_STEP = 16;

function clampSidebarWidth(width: number): number {
	return Math.min(MAX_SIDEBAR_WIDTH, Math.max(MIN_SIDEBAR_WIDTH, Math.round(width)));
}

function loadSidebarWidth(): number {
	if (typeof window === "undefined") return DEFAULT_SIDEBAR_WIDTH;
	try {
		const stored = Number(window.localStorage.getItem(SIDEBAR_WIDTH_STORAGE_KEY));
		return Number.isFinite(stored) && stored > 0
			? clampSidebarWidth(stored)
			: DEFAULT_SIDEBAR_WIDTH;
	} catch {
		return DEFAULT_SIDEBAR_WIDTH;
	}
}

function saveSidebarWidth(width: number): void {
	try {
		window.localStorage.setItem(SIDEBAR_WIDTH_STORAGE_KEY, String(clampSidebarWidth(width)));
	} catch {
		// localStorage persistence is best-effort.
	}
}

function delegationQueryPrefix(parentSessionId: string) {
	return ["delegations", parentSessionId] as const;
}

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

function routeScope(projectId: string | null) {
	return projectId === null ? hostRouteScope() : projectRouteScope(projectId);
}

type ExportDialogState = {
	entries: TranscriptEntry[];
	blocks?: ExportBlock[];
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

export interface AppProps {
	api?: AgentApi;
	routeHistory?: WorkspaceRouteHistory | null;
}

type RouteValidationState =
	| { kind: "idle" }
	| { kind: "pending" }
	| {
			kind: "valid";
			revision: number;
			canonicalUrl: string;
			projectId: string | null;
			conversationSessionId: string;
		}
	| {
			kind: "unavailable";
			state: WorkspaceRouteUnavailable;
			retryable: boolean;
		};

function routeScopeProjectId(route: WorkspaceRoute): string | null {
	return route.scope.kind === "project" ? route.scope.projectId : null;
}

function routeConversationSessionId(route: WorkspaceRoute): string {
	return messageRecipient(route).sessionId;
}

function routeReadsEnabled(
	result: WorkspaceRouteParseResult,
	validation: RouteValidationState,
	revision: number,
): boolean {
	if (result.kind === "none") return validation.kind === "idle";
	if (result.kind !== "route" || validation.kind !== "valid") return false;
	return (
		validation.revision === revision &&
		validation.canonicalUrl === result.canonicalUrl &&
		validation.projectId === routeScopeProjectId(result.route) &&
		validation.conversationSessionId === routeConversationSessionId(result.route)
	);
}

function initialRouteResult(history: WorkspaceRouteHistory | null): WorkspaceRouteParseResult {
	return history?.current() ?? { kind: "none" };
}

function routeInitialSelection(
	result: WorkspaceRouteParseResult,
	legacy: ReturnType<typeof loadUiSelection>,
): { projectId: string | null; conversationSessionId: string | null } {
	if (result.kind === "route") {
		return {
			projectId: routeScopeProjectId(result.route),
			conversationSessionId: routeConversationSessionId(result.route),
		};
	}
	if (result.kind === "unavailable") {
		return { projectId: null, conversationSessionId: null };
	}
	return {
		projectId: legacy.projectId,
		// Legacy identity is not trusted until the selected session's canonical
		// direct parent/root has been resolved.
		conversationSessionId: null,
	};
}

function projectMismatchUnavailable(route: WorkspaceRoute, actualProjectId: string | null): WorkspaceRouteUnavailable {
	const requestedProject =
		route.scope.kind === "project" ? `project ${route.scope.projectId}` : "Host";
	const actualProject = actualProjectId ? `project ${actualProjectId}` : "Host";
	return {
		kind: "unavailable",
		issue: "project-mismatch",
		message: `This run belongs to ${actualProject}, not ${requestedProject}.`,
		requestedUrl: "",
		backTo: null,
	};
}

function routeRootUnavailable(message: string): WorkspaceRouteUnavailable {
	return {
		kind: "unavailable",
		issue: "invalid-conversation",
		message,
		requestedUrl: "",
		backTo: null,
	};
}

function sessionListRefreshKey(projectId: string | null): string {
	return projectId ?? "__host__";
}

function projectIdFromEventData(event: EventFrame): string | null | undefined {
	const value = event.data.project_id;
	if (typeof value === "string") return value;
	if (value === null) return null;
	return undefined;
}

function firstKnownProjectId(...projectIds: (string | null | undefined)[]): string | null | undefined {
	for (const projectId of projectIds) {
		if (projectId !== undefined) return projectId;
	}
	return undefined;
}

function sessionListProjectTargets(projectId: string | null): (string | null)[] {
	return [projectId];
}

function cachedProjectIdForSession(queryClient: QueryClient, sessionId: string): string | null | undefined {
	for (const [, sessions] of queryClient.getQueriesData<SessionSummary[]>({ queryKey: ["sessions"] })) {
		const session = sessions?.find((candidate) => candidate.session_id === sessionId);
		if (session) return session.project_id;
	}
	return undefined;
}

function backgroundSessionNeedsWarm(
	session: SessionListItem,
	cache: SelectedSessionCache | null,
	warmedUpdatedAt: string | undefined,
): boolean {
	if (!cache?.snapshot) return true;
	if (warmedUpdatedAt !== session.updated_at) return true;
	if (cache.snapshot.activity !== session.activity) return true;
	if (cache.snapshot.active_leaf_id !== session.active_leaf_id) return true;
	if (session.has_transcript_entries && cache.turnOrder.length === 0) return true;
	return false;
}

function canWarmBackgroundSession(session: SessionListItem): boolean {
	if (session.parent_session_id) return false;
	if (session.metadata?.hidden === true) return false;
	if (session.metadata?.archived === true) return false;
	if (session.metadata?.subagent === true) return false;
	return true;
}

function subagentStatusNeedsWarm(status: DelegationSubagent["status"], activity?: Activity): boolean {
	return activity === "running" || activity === "queued" || status === "running" || status === "queued";
}

function hasCanonicalCachedHistory(cache: SelectedSessionCache, sessionId: string | null): boolean {
	return (
		!!sessionId &&
		cache.sessionId === sessionId &&
		!!cache.snapshot &&
		cache.treeComplete &&
		cache.treeActiveLeafId === (cache.snapshot.active_leaf_id ?? null) &&
		cache.treeTranscriptRevision === (cache.snapshot.transcript_revision ?? null)
	);
}

function LoadingConversation() {
	const [dotCount, setDotCount] = useState(1);

	useEffect(() => {
		const interval = window.setInterval(() => {
			setDotCount((current) => current % 3 + 1);
		}, 250);
		return () => window.clearInterval(interval);
	}, []);

	return (
		<main
			className="workspace-route-state conversation-loading-state"
			data-slot="route-loading"
			role="status"
		>
			<h1>
				Loading conversation
				<span className="conversation-loading-dots" aria-hidden>
					{".".repeat(dotCount)}
				</span>
			</h1>
		</main>
	);
}

export function App({ api: injectedApi, routeHistory: injectedRouteHistory }: AppProps = {}) {
	const api = useMemo(() => injectedApi ?? createAgentApi(), [injectedApi]);
	const routeHistory = useMemo(
		() => injectedRouteHistory === undefined ? browserWorkspaceRouteHistory() : injectedRouteHistory,
		[injectedRouteHistory],
	);
	const queryClient = useQueryClient();
	const initialUiSelection = useMemo(() => loadUiSelection(), []);
	const initialWorkspaceRoute = useMemo(() => initialRouteResult(routeHistory), [routeHistory]);
	const initialSelection = useMemo(
		() => routeInitialSelection(initialWorkspaceRoute, initialUiSelection),
		[initialUiSelection, initialWorkspaceRoute],
	);
	const [connection, setConnection] = useState<ConnectionStatus>("connecting");
	const connectionRef = useRef<ConnectionStatus>("connecting");
	const [disconnected, setDisconnected] = useState(false);
	const [retryingConnection, setRetryingConnection] = useState(false);
	const [workspaceRouteResult, setWorkspaceRouteResult] =
		useState<WorkspaceRouteParseResult>(initialWorkspaceRoute);
	const [routeValidation, setRouteValidation] = useState<RouteValidationState>(
		initialWorkspaceRoute.kind === "route"
			? { kind: "pending" }
			: initialWorkspaceRoute.kind === "unavailable"
				? { kind: "unavailable", state: initialWorkspaceRoute, retryable: false }
				: initialUiSelection.sessionId
					? { kind: "pending" }
					: { kind: "idle" },
	);
	const [routeRevision, setRouteRevision] = useState(0);
	const [routeValidationRetry, setRouteValidationRetry] = useState(0);
	const [selectedProjectId, setSelectedProjectId] = useState<string | null>(initialSelection.projectId);
	// `selectedId` remains as an incremental alias throughout the mature
	// transcript/cache code. Its sole identity source is conversationSessionId.
	const [conversationSessionId, setConversationSessionId] = useState<string | null>(
		initialSelection.conversationSessionId,
	);
	const selectedId = conversationSessionId;
	const selectedRef = useRef<string | null>(initialSelection.conversationSessionId);
	const [notices, setNotices] = useState<ErrorNotice[]>([]);
	const [query, setQuery] = useState("");
	const [newSessionProvider, setNewSessionProvider] = useState<ProviderConfig>(DEFAULT_PROVIDER);
	const [, setProviderConfigurationRevision] = useState(0);
	const providerConfigurationControllerRef = useRef<ProviderConfigurationController | null>(null);
	const providerConfigurationMountGenerationRef = useRef(0);
	const [mcpSelection, setMcpSelection] = useState<McpSelectionState>(new Map());
	const mcpSelectionRef = useRef<McpSelectionState>(mcpSelection);
	const mcpSelectionProviderRef = useRef<ProviderConfig["kind"]>(newSessionProvider.kind);
	const previousMcpInventoryRef = useRef<McpInventory | null>(null);
	const [reconciledMcpInventoryIdentity, setReconciledMcpInventoryIdentity] =
		useState<string | null>(null);
	const [newSessionSetupGeneration, setNewSessionSetupGeneration] = useState(0);
	const [sending, setSending] = useState(false);
	const [workspacePreparationProjectId, setWorkspacePreparationProjectId] =
		useState<string | null>(null);
	const [stopping, setStopping] = useState(false);
	const [resumingTurnId, setResumingTurnId] = useState<string | null>(null);
	const [transcriptDestination, setTranscriptDestination] = useState<TranscriptDestination | null>(null);
	const [sidebarOpen, setSidebarOpen] = useState(() => defaultPanelState(panelModeForViewport()).sidebarOpen);
	const [rightOpen, setRightOpen] = useState(() => defaultPanelState(panelModeForViewport()).rightOpen);
	const [panelMode, setPanelMode] = useState<PanelMode>(() => panelModeForViewport());
	const [sidebarWidth, setSidebarWidth] = useState(loadSidebarWidth);
	const [sidebarResizing, setSidebarResizing] = useState(false);
	const [showArchived, setShowArchived] = useState(false);
	const [showAllDelegations, setShowAllDelegations] = useState(false);
	const [backgroundWarmRevision, setBackgroundWarmRevision] = useState(0);
	const [historyDialog, setHistoryDialog] = useState<HistoryDialogState | null>(null);
	const [exportDialog, setExportDialog] = useState<ExportDialogState | null>(null);
	const [renameSessionId, setRenameSessionId] = useState<string | null>(null);
	const [renameValue, setRenameValue] = useState("");
	const [deleteDialog, setDeleteDialog] = useState<DeleteDialogState | null>(null);
	const [projectDialog, setProjectDialog] = useState<ProjectDialogState | null>(null);
	const [promptDialog, setPromptDialog] = useState<SystemPromptDialogState | null>(null);
	const [mcpLoginDialog, setMcpLoginDialog] = useState<{
		server: string;
		login: McpLoginResult;
		context: string;
		terminalArmed: boolean;
	} | null>(null);
	const [mcpAuthBusyServer, setMcpAuthBusyServer] = useState<string | null>(null);
	const mcpLoginContextRef = useRef("");
	const {
		cache: selectedCache,
		cacheRef: selectedCacheRef,
		drop: dropSelectedCache,
		get: getSelectedCache,
		replace: replaceSelectedCache,
		reset: resetSelectedCache,
		update: updateSelectedCache,
		warm: warmSelectedCache,
	} = useSelectedSessionStore(initialSelection.conversationSessionId);
	const selectedFetchCoordinatorRef = useRef<SelectedSessionFetchCoordinator | null>(null);
	if (!selectedFetchCoordinatorRef.current) {
		selectedFetchCoordinatorRef.current = new SelectedSessionFetchCoordinator({
			sessionId: initialSelection.conversationSessionId,
			selectionVersion: 0,
			loading: !!initialSelection.conversationSessionId,
			retrying: false,
			hadUsableCache: false,
			error: null,
		});
	}
	const selectedFetchCoordinator = selectedFetchCoordinatorRef.current;
	const selectedFetchState = useSyncExternalStore(
		selectedFetchCoordinator.subscribe,
		selectedFetchCoordinator.getSnapshot,
		selectedFetchCoordinator.getSnapshot,
	);

	const selectedSyncTimer = useRef<number | null>(null);
	const sessionListRefreshTimers = useRef(new Map<string, number>());
	const backgroundWarmUpdatedAt = useRef(new Map<string, string>());
	const backgroundWarmInFlight = useRef(new Set<string>());
	const composerHandleRef = useRef<ComposerHandle | null>(null);
	const appShellRef = useRef<HTMLDivElement | null>(null);
	const mobileSidebarToggleRef = useRef<HTMLButtonElement | null>(null);
	const sidebarNewSessionButtonRef = useRef<HTMLButtonElement | null>(null);
	const sidebarWidthRef = useRef(sidebarWidth);
	const sidebarResizeRef = useRef<{
		pointerId: number;
		startX: number;
		startWidth: number;
	} | null>(null);
	const nextSessionTitleRef = useRef<string | null>(null);
	const selectedProjectRef = useRef<string | null>(initialSelection.projectId);
	const routeValidationGenerationRef = useRef(0);
	const workspaceRouteResultRef = useRef(workspaceRouteResult);
	const routeValidationRef = useRef(routeValidation);
	const legacyMigrationPendingRef = useRef(
		initialWorkspaceRoute.kind === "none" && !!initialUiSelection.sessionId,
	);
	const initialCorrectionAppliedRef = useRef(false);
	workspaceRouteResultRef.current = workspaceRouteResult;
	routeValidationRef.current = routeValidation;
	const routeRemoteReadsEnabled = routeReadsEnabled(
		workspaceRouteResult,
		routeValidation,
		routeRevision,
	);
	const routeRemoteReadsEnabledRef = useRef(routeRemoteReadsEnabled);
	routeRemoteReadsEnabledRef.current = routeRemoteReadsEnabled;
	const lastEventIds = useRef(new Map<string, number>());
	const subscribedEventSessionIds = useRef(new Set<string>());
	const panelModeRef = useRef<PanelMode>(panelModeForViewport());
	const sidebarSelectTimer = useRef<number | null>(null);
	const autoLoadedTurnDetailRef = useRef<string | null>(null);
	const nextTranscriptDestinationIdRef = useRef(0);
	const lastForegroundReconcileAt = useRef(Date.now());
	const lastAwakeAt = useRef(Date.now());
	const foregroundReconnectInFlight = useRef<Promise<void> | null>(null);
	const connectionRetryController = useRef(new ConnectionRetryController());
	const delegationListRetryController = useRef(new DelegationListRetryController());
	const handleSessionEventRef = useRef<(event: EventFrame) => void>(() => undefined);
	const connectionRemoteActionBlockedReason = remoteActionBlockedReason(connection);
	const cachedHistoryAvailable = hasCanonicalCachedHistory(selectedCache, selectedId);
	const assertServerMutationAllowed = useCallback(() => {
		assertRemoteActionAllowed(remoteActionBlockedReason(connectionRef.current));
	}, []);
	const assertConnectionReadAllowed = useCallback(() => {
		assertRemoteActionAllowed(remoteActionBlockedReason(connectionRef.current));
	}, []);
	const assertServerReadAllowed = useCallback(() => {
		assertRemoteActionAllowed(remoteActionBlockedReason(connectionRef.current));
		if (!routeRemoteReadsEnabledRef.current) {
			throw new IntermediateUiStateError("Conversation is still loading.");
		}
	}, []);

	const pushErrorNotice = useCallback((text: string, persistent = false) => {
		if (connectionRef.current !== "open") return;
		setNotices((current) => [...current.slice(Math.max(0, current.length - MAX_ERROR_NOTICES + 1)), { id: randomId("notice"), text, persistent }]);
	}, []);
	const reportActionError = useCallback((error: unknown) => {
		if (shouldReportActionError(error)) pushErrorNotice(errorMessage(error));
	}, [pushErrorNotice]);
	const dismissNotice = useCallback((noticeId: string) => {
		setNotices((current) => current.filter((notice) => notice.id !== noticeId));
	}, []);

	useEffect(() => {
		if (selectedRef.current !== selectedId) selectedRef.current = selectedId;
	}, [selectedId]);

	useEffect(() => {
		if (selectedProjectRef.current !== selectedProjectId) selectedProjectRef.current = selectedProjectId;
	}, [selectedProjectId]);

	useEffect(() => {
		const expiringNotice = notices.find((notice) => !notice.persistent);
		if (!expiringNotice) return;
		const timer = window.setTimeout(() => {
			setNotices((current) => current.filter((notice) => notice.id !== expiringNotice.id));
		}, ERROR_NOTICE_TTL_MS);
		return () => window.clearTimeout(timer);
	}, [notices]);

	const projectsQuery = useQuery({
		queryKey: queryKeys.projects,
		queryFn: () => {
			assertConnectionReadAllowed();
			return api.listProjects();
		},
		enabled: connection === "open",
	});
	const projects = projectsQuery.data ?? [];
	const retainedProjectsErrorRef = useRef<unknown>(null);
	if (projectsQuery.error) {
		retainedProjectsErrorRef.current = projectsQuery.error;
	} else if (projectsQuery.data !== undefined && !projectsQuery.isFetching) {
		retainedProjectsErrorRef.current = null;
	}
	const projectsError = errorMessageOrNull(
		projectsQuery.error ?? retainedProjectsErrorRef.current,
	);
	const projectsRetryRef = useRef<Promise<unknown> | null>(null);
	const retryProjects = useCallback(() => {
		if (connectionRef.current !== "open" || projectsRetryRef.current) return;
		let request: Promise<unknown>;
		try {
			request = projectsQuery.refetch();
		} catch (error) {
			request = Promise.reject(error);
		}
		const pending = request.finally(() => {
			if (projectsRetryRef.current === pending) projectsRetryRef.current = null;
		});
		projectsRetryRef.current = pending;
		void pending.catch(() => undefined);
	}, [projectsQuery.refetch]);
	const knownProjectIds = useMemo(
		() => [null, ...projects.map((project) => project.project_id)],
		[projects],
	);
	const knownProjectIdsRef = useRef(knownProjectIds);
	useEffect(() => {
		knownProjectIdsRef.current = knownProjectIds;
	}, [knownProjectIds]);
	const backgroundSessionProjectIds = useMemo(
		() => knownProjectIds.filter((projectId) => projectId !== selectedProjectId),
		[knownProjectIds, selectedProjectId],
	);

	const sessionListCoordinatorRef = useRef<SessionListRequestCoordinator<SessionSummary[]> | null>(null);
	if (!sessionListCoordinatorRef.current) {
		sessionListCoordinatorRef.current = new SessionListRequestCoordinator<SessionSummary[]>(selectedProjectId);
	}
	const sessionListCoordinator = sessionListCoordinatorRef.current;
	const sessionListRequestState = useSyncExternalStore(
		sessionListCoordinator.subscribe,
		sessionListCoordinator.getSnapshot,
		sessionListCoordinator.getSnapshot,
	);
	useEffect(() => {
		sessionListCoordinator.selectProject(selectedProjectId);
	}, [selectedProjectId, sessionListCoordinator]);
	const sessionsQuery = useQuery({
		queryKey: queryKeys.sessions(selectedProjectId),
		queryFn: () =>
			sessionListCoordinator.run(
				selectedProjectId,
				() => {
					assertServerReadAllowed();
					return api.listSessions(100, selectedProjectId);
				},
			),
		enabled: connection === "open" && routeRemoteReadsEnabled,
		refetchInterval: SESSION_LIST_REFETCH_MS,
		refetchIntervalInBackground: true,
		refetchOnReconnect: true,
		refetchOnWindowFocus: true,
	});
	useEffect(() => {
		sessionListCoordinator.setQueryFetching(
			selectedProjectId,
			sessionsQuery.fetchStatus === "fetching",
		);
	}, [selectedProjectId, sessionListCoordinator, sessionsQuery.fetchStatus]);
	const backgroundSessionsQueries = useQueries({
		queries: backgroundSessionProjectIds.map((projectId) => ({
			queryKey: queryKeys.sessions(projectId),
			queryFn: () =>
				sessionListCoordinator.run(
					projectId,
					() => {
						assertServerReadAllowed();
						return api.listSessions(100, projectId);
					},
				),
			enabled: connection === "open" && routeRemoteReadsEnabled,
			refetchInterval: SESSION_LIST_REFETCH_MS,
			refetchIntervalInBackground: true,
			refetchOnReconnect: true,
			refetchOnWindowFocus: true,
		})),
	});
	useEffect(() => {
		for (const [index, projectId] of backgroundSessionProjectIds.entries()) {
			sessionListCoordinator.setQueryFetching(
				projectId,
				backgroundSessionsQueries[index]?.fetchStatus === "fetching",
			);
		}
	}, [backgroundSessionProjectIds, backgroundSessionsQueries, sessionListCoordinator]);
	const sessions = sessionsQuery.data ?? [];
	const backgroundSessions = backgroundSessionsQueries.flatMap((query) => query.data ?? []);
	const allKnownSessions = useMemo(
		() => {
			const byId = new Map<string, SessionListItem>();
			for (const session of [...sessions, ...backgroundSessions]) byId.set(session.session_id, session);
			return [...byId.values()];
		},
		[backgroundSessions, sessions],
	);

	const invalidateKnownSessionLists = useCallback(() => {
		const projectIds = new Set<string | null>(knownProjectIdsRef.current);
		projectIds.add(selectedProjectRef.current);
		return Promise.all(
			Array.from(projectIds).map((projectId) =>
				queryClient.invalidateQueries({ queryKey: queryKeys.sessions(projectId) }),
			),
		);
	}, [queryClient]);

	const mergeSnapshotIntoKnownSessionLists = useCallback(
		(snapshot: SessionSnapshot) => {
			for (const projectId of sessionListProjectTargets(snapshot.project_id)) {
				queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), (current) =>
					mergeSnapshotIntoSessionList(current, snapshot),
				);
			}
		},
		[queryClient],
	);

	const removeSessionFromKnownSessionLists = useCallback(
		(sessionId: string, projectId: string | null) => {
			for (const targetProjectId of sessionListProjectTargets(projectId)) {
				queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(targetProjectId), (current) =>
					current?.filter((candidate) => candidate.session_id !== sessionId),
				);
			}
		},
		[queryClient],
	);

	useEffect(() => {
		if (connection !== "open" || !routeRemoteReadsEnabled) return;
		const key = sessionListRefreshKey(selectedProjectId);
		const timer = sessionListRefreshTimers.current.get(key);
		if (timer !== undefined) {
			window.clearTimeout(timer);
			sessionListRefreshTimers.current.delete(key);
		}
		void invalidateKnownSessionLists();
	}, [connection, invalidateKnownSessionLists, routeRemoteReadsEnabled, selectedProjectId]);

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
	const [workspaceScopeSourceKey, setWorkspaceScopeSourceKey] = useState<string | null>(null);
	const workspaceScopeRef = useRef<WorkspaceScopeEntry[]>(workspaceScope);
	workspaceScopeRef.current = workspaceScope;
	const projectWorkspaces = selectedProject?.workspaces ?? null;
	const projectWorkspacesRef = useRef(projectWorkspaces);
	projectWorkspacesRef.current = projectWorkspaces;
	const projectWorkspaceKey = JSON.stringify(
		projectWorkspaces?.map((workspace) => ({
			workspaceDir: workspace.workspace_dir,
			kind: workspace.kind ?? "git",
		})) ?? [],
	);
	const currentWorkspaceScopeSourceKey = selectedProjectId
		? `${selectedProjectId}\u0000${projectWorkspaceKey}`
		: null;
	useEffect(() => {
		setWorkspaceScope(workspaceScopeForProject(selectedProjectId, projectWorkspacesRef.current ?? []));
		setWorkspaceScopeSourceKey(currentWorkspaceScopeSourceKey);
	}, [currentWorkspaceScopeSourceKey, selectedProjectId]);
	const workspaceConfiguration: WorkspaceConfiguration =
		selectedProjectId === null
			? { status: "ready", scope: null }
			: selectedProject && workspaceScopeSourceKey === currentWorkspaceScopeSourceKey
				? { status: "ready", scope: workspaceScope }
				: !selectedProject && (projectsQuery.status === "success" || projectsError)
					? { status: "unavailable" }
					: { status: "loading" };
	const handleWorkspaceScopeChange = useCallback((scope: WorkspaceScopeEntry[]) => {
		setWorkspaceScope(scope);
		rememberWorkspaceScope(selectedProjectRef.current, scope);
		setNewSessionSetupGeneration((generation) => generation + 1);
	}, []);

	const selectedSession = useMemo(
		() => sessionItems.find((session) => session.session_id === selectedId) ?? null,
		[sessionItems, selectedId],
	);

	const loadedSnapshot = selectedCache.sessionId === selectedId ? selectedCache.snapshot : null;
	const selectedLoading = selectedFetchState.sessionId === selectedId && selectedFetchState.loading;
	const selectedRetrying = selectedFetchState.sessionId === selectedId && selectedFetchState.retrying;
	const selectedError = selectedFetchState.sessionId === selectedId ? selectedFetchState.error : null;
	const selectedErrorHasUsableCache =
		selectedFetchState.sessionId === selectedId && selectedFetchState.hadUsableCache;
	const transcriptLoading = !!selectedId && selectedLoading;
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
	const transcriptTurnPageIdentity = useMemo<TranscriptTurnPageIdentity | null>(
		() =>
			selectedCache.sessionId === selectedId && selectedCache.transcriptTurnsLoaded && selectedId
				? {
						sessionId: selectedId,
						leafId: selectedCache.turnActiveLeafId,
						hydrationRevision: selectedCache.turnPageHydrationRevision,
					}
				: null,
		[
			selectedCache.sessionId,
			selectedCache.transcriptTurnsLoaded,
			selectedCache.turnActiveLeafId,
			selectedCache.turnPageHydrationRevision,
			selectedId,
		],
	);
	const latestTurnCard = orderedTurnCards.at(-1) ?? null;
	const runningTurnCardId = loadedSnapshot?.activity === "running" && latestTurnCard?.status === "open" ? latestTurnCard.id : null;
	const turnCardViews = useMemo(() => {
		if (orderedTurnCards.length === 0) return null;
		return orderedTurnCards.map((card) => {
			const isCurrent = card.id === runningTurnCardId;
			const expanded = expandedTurnIds.has(card.id) || isCurrent;
			const detailEntries = turnDetailEntries(selectedCache, card.id);
			return {
				card,
				entries: expanded ? detailEntries : null,
				detailCached: detailEntries !== null,
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

	const storedProvider = loadedSnapshot?.provider ?? selectedSession?.provider ?? newSessionProvider;
	const activeProvider =
		(selectedId
			? providerConfigurationControllerRef.current?.desired(selectedId)
			: null) ?? storedProvider;
	const activeProviderKind = activeProvider.kind;
	const activeToolsSessionId = loadedSnapshot?.session_id ?? selectedChatSession?.session_id ?? null;
	const toolsQuery = useQuery({
		queryKey: queryKeys.tools(activeProviderKind, activeToolsSessionId),
		queryFn: () => {
			assertServerReadAllowed();
			return api.listTools(activeProviderKind, activeToolsSessionId);
		},
		enabled: connection === "open" && routeRemoteReadsEnabled,
	});
	const tools: ToolListing[] = toolsQuery.data ?? [];
	const mcpInventoryQuery = useQuery({
		queryKey: queryKeys.mcpInventory(newSessionProvider.kind),
		queryFn: async () => {
			assertServerReadAllowed();
			return {
				provider: newSessionProvider.kind,
				inventory: await api.getMcpInventory(newSessionProvider.kind),
			};
		},
		enabled: connection === "open" && routeRemoteReadsEnabled && !selectedId,
	});
	const mcpStatusQuery = useQuery({
		queryKey: queryKeys.mcpStatus,
		queryFn: () => {
			assertServerReadAllowed();
			return api.getMcpStatus();
		},
		enabled: connection === "open" && routeRemoteReadsEnabled && !selectedId,
		refetchInterval: (query) =>
			query.state.data?.servers.some(
				(server) => server.auth_state === "authorization_pending",
			)
				? 2_000
				: false,
	});
	const mcpAuthStatus = mcpStatusQuery.data?.servers ?? [];
	const mcpAuthStatusReady =
		mcpStatusQuery.status === "success" &&
		!mcpStatusQuery.isFetching &&
		!mcpStatusQuery.error;
	const mcpInventoryProvider =
		mcpInventoryQuery.data?.provider === newSessionProvider.kind
			? mcpInventoryQuery.data.provider
			: null;
	const mcpInventory =
		mcpInventoryProvider === newSessionProvider.kind
			? mcpInventoryQuery.data?.inventory ?? null
			: null;
	const mcpInventoryIdentity = mcpInventory
		? JSON.stringify({
				provider: mcpInventoryProvider,
				revision: mcpInventory.revision,
			})
		: null;
	const mcpInventoryReady =
		mcpInventoryProvider === newSessionProvider.kind &&
		reconciledMcpInventoryIdentity === mcpInventoryIdentity &&
		!mcpInventoryQuery.isFetching &&
		!mcpInventoryQuery.error;
	useEffect(() => {
		if (!mcpInventory) return;
		const next = reconcileMcpSelection(
			previousMcpInventoryRef.current,
			mcpInventory,
			mcpSelectionRef.current,
		);
		previousMcpInventoryRef.current = mcpInventory;
		mcpSelectionRef.current = next;
		setMcpSelection(next);
		setReconciledMcpInventoryIdentity(mcpInventoryIdentity);
	}, [mcpInventory, mcpInventoryIdentity]);
	const handleMcpSelectionChange = useCallback((selection: McpSelectionState) => {
		mcpSelectionRef.current = selection;
		setMcpSelection(selection);
		setNewSessionSetupGeneration((generation) => generation + 1);
	}, []);
	const retryMcpInventory = useCallback(() => {
		if (mcpInventoryQuery.isFetching || mcpStatusQuery.isFetching) return;
		try {
			assertServerReadAllowed();
		} catch (error) {
			pushErrorNotice(errorMessage(error));
			return;
		}
		void Promise.allSettled([
			mcpStatusQuery.refetch(),
			mcpInventoryQuery.refetch(),
		]);
	}, [
		assertServerReadAllowed,
		mcpInventoryQuery.isFetching,
		mcpInventoryQuery.refetch,
		mcpStatusQuery.isFetching,
		mcpStatusQuery.refetch,
		pushErrorNotice,
	]);
	const refreshMcpAfterAuthChange = useCallback(async () => {
		await Promise.allSettled([
			mcpStatusQuery.refetch(),
			mcpInventoryQuery.refetch(),
		]);
	}, [mcpInventoryQuery.refetch, mcpStatusQuery.refetch]);
	const mcpLoginContext = `${selectedId ?? "new"}\u0000${selectedProjectId ?? "host"}\u0000${newSessionProvider.kind}\u0000${newSessionSetupGeneration}`;
	mcpLoginContextRef.current = mcpLoginContext;
	const loginMcp = useCallback(async (server: string) => {
		if (mcpAuthBusyServer) return;
		setMcpAuthBusyServer(server);
		try {
			assertServerMutationAllowed();
			const context = mcpLoginContext;
			const login = await api.loginMcp(server);
			if (context !== mcpLoginContextRef.current || selectedRef.current !== null) {
				void api.cancelMcpLogin(server, login.login_id).catch(() => undefined);
				return;
			}
			setMcpLoginDialog({ server, login, context, terminalArmed: false });
			const statusResult = await mcpStatusQuery.refetch();
			if (statusResult.error) {
				setMcpLoginDialog(null);
				void api.cancelMcpLogin(server, login.login_id).catch(() => undefined);
				pushErrorNotice("Could not verify MCP login status");
				return;
			}
			setMcpLoginDialog((current) =>
				current?.server === server && current.login.login_id === login.login_id
					? { ...current, terminalArmed: true }
					: current
			);
		} catch (error) {
			pushErrorNotice(errorMessage(error));
		} finally {
			setMcpAuthBusyServer(null);
		}
	}, [
		api,
		assertServerMutationAllowed,
		mcpAuthBusyServer,
		mcpLoginContext,
		mcpStatusQuery.refetch,
		pushErrorNotice,
	]);
	const completeMcpLogin = useCallback(async (callbackUrl: string) => {
		if (!mcpLoginDialog) return;
		assertServerMutationAllowed();
		await api.completeMcpLogin(
			mcpLoginDialog.server,
			mcpLoginDialog.login.login_id,
			callbackUrl,
		);
		setMcpLoginDialog(null);
		await refreshMcpAfterAuthChange();
	}, [
		api,
		assertServerMutationAllowed,
		mcpLoginDialog,
		refreshMcpAfterAuthChange,
	]);
	const cancelMcpLogin = useCallback(async (confirmCleanup = true) => {
		if (!mcpLoginDialog) return;
		if (
			confirmCleanup &&
			mcpSelectionRef.current.get(mcpLoginDialog.server)?.size &&
			!window.confirm(
				`Continue and clear ${mcpLoginDialog.server}'s selected draft tools?`,
			)
		) return;
		try {
			assertServerMutationAllowed();
			await api.cancelMcpLogin(
				mcpLoginDialog.server,
				mcpLoginDialog.login.login_id,
			);
		} catch (error) {
			const message = errorMessage(error);
			if (
				![
					"mcp_oauth_login_not_found:",
					"mcp_oauth_login_finished:",
					"mcp_oauth_login_cancelled:",
					"mcp_oauth_login_expired:",
				].some((code) => message.startsWith(code))
			) {
				pushErrorNotice(message);
				return;
			}
		}
		const next = clearMcpServerSelection(
			mcpSelectionRef.current,
			mcpLoginDialog.server,
		);
		if (next !== mcpSelectionRef.current) {
			mcpSelectionRef.current = next;
			setMcpSelection(next);
			setNewSessionSetupGeneration((generation) => generation + 1);
		}
		setMcpLoginDialog(null);
		await mcpStatusQuery.refetch().catch((error) => {
			pushErrorNotice(errorMessage(error));
		});
	}, [
		api,
		assertServerMutationAllowed,
		mcpLoginDialog,
		mcpStatusQuery.refetch,
		pushErrorNotice,
	]);
	const logoutMcp = useCallback(async (server: string) => {
		if (mcpAuthBusyServer) return;
		setMcpAuthBusyServer(server);
		try {
			assertServerMutationAllowed();
			await api.logoutMcp(server);
			const next = clearMcpServerSelection(mcpSelectionRef.current, server);
			mcpSelectionRef.current = next;
			setMcpSelection(next);
			setNewSessionSetupGeneration((generation) => generation + 1);
			await refreshMcpAfterAuthChange();
		} catch (error) {
			pushErrorNotice(errorMessage(error));
		} finally {
			setMcpAuthBusyServer(null);
		}
	}, [
		api,
		assertServerMutationAllowed,
		mcpAuthBusyServer,
		pushErrorNotice,
		refreshMcpAfterAuthChange,
	]);
	const cancelOrLogoutMcp = useCallback((server: string) => {
		if (mcpLoginDialog?.server === server) {
			void cancelMcpLogin(false);
			return;
		}
		void logoutMcp(server);
	}, [cancelMcpLogin, logoutMcp, mcpLoginDialog?.server]);
	useEffect(() => {
		if (!mcpLoginDialog || mcpLoginDialog.context === mcpLoginContext) return;
		const stale = mcpLoginDialog;
		setMcpLoginDialog(null);
		void api
			.cancelMcpLogin(stale.server, stale.login.login_id)
			.catch(() => undefined);
	}, [api, mcpLoginContext, mcpLoginDialog]);
	useEffect(() => {
		if (!mcpLoginDialog?.terminalArmed) return;
		if (mcpStatusQuery.error) {
			const stale = mcpLoginDialog;
			setMcpLoginDialog(null);
			void api
				.cancelMcpLogin(stale.server, stale.login.login_id)
				.catch(() => undefined);
			pushErrorNotice("Could not verify MCP login status");
			return;
		}
		if (mcpStatusQuery.status !== "success") return;
		const status = mcpAuthStatus.find(
			(server) => server.server === mcpLoginDialog.server,
		);
		if (status?.auth_state === "authorization_pending") return;
		if (!status || status.auth_kind !== "oauth") {
			setMcpLoginDialog(null);
			pushErrorNotice("MCP login is no longer available");
			return;
		}
		if (status.auth_state !== "ready") {
			setMcpLoginDialog(null);
			pushErrorNotice("MCP login ended before authorization completed");
			return;
		}
		setMcpLoginDialog(null);
		void mcpInventoryQuery.refetch();
	}, [
		api,
		mcpAuthStatus,
		mcpInventoryQuery.refetch,
		mcpLoginDialog,
		mcpStatusQuery.error,
		mcpStatusQuery.status,
		pushErrorNotice,
	]);
	// A selected child keeps its direct parent's board visible so the child row
	// can expose current navigation semantics. This intentionally follows only
	// the canonical direct parent; it does not infer a root or traverse a graph.
	const delegationParentSessionId =
		workspaceRouteResult.kind === "route" ? workspaceRouteResult.route.rootSessionId : null;
	const expandedDelegationQueryKey = queryKeys.delegations(
		delegationParentSessionId,
		RUN_BOARD_EXPANDED_DELEGATION_COUNT,
	);
	const expandedDelegationsAvailable =
		!!delegationParentSessionId &&
		queryClient.getQueryData(expandedDelegationQueryKey) !== undefined;
	const defaultDelegationsQuery = useQuery({
		queryKey: queryKeys.delegations(
			delegationParentSessionId,
			RUN_BOARD_DEFAULT_DELEGATION_COUNT,
		),
		queryFn: () => {
			if (!delegationParentSessionId) throw new Error("select a session first");
			assertServerReadAllowed();
			return api.listDelegations(
				delegationParentSessionId,
				RUN_BOARD_DEFAULT_DELEGATION_COUNT,
			);
		},
		enabled:
			connection === "open" &&
			!!delegationParentSessionId &&
			routeRemoteReadsEnabled,
		// The parent PARKS (goes idle) while a delegation runs, so gate the poll
		// on whether any delegation is actually running — not on the parent's
		// activity — or the missed-event safety net would be off exactly when
		// it's needed.
		refetchInterval: (query) =>
			(query.state.data?.delegations ?? []).some(isDelegationRunning) ? 2_000 : false,
	});
	const expandedDelegationsQuery = useQuery({
		queryKey: expandedDelegationQueryKey,
		queryFn: () => {
			if (!delegationParentSessionId) throw new Error("select a session first");
			assertServerReadAllowed();
			return api.listDelegations(
				delegationParentSessionId,
				RUN_BOARD_EXPANDED_DELEGATION_COUNT,
			);
		},
		enabled:
			connection === "open" &&
			!!delegationParentSessionId &&
			routeRemoteReadsEnabled &&
			showAllDelegations,
		refetchInterval: (query) =>
			(query.state.data?.delegations ?? []).some(isDelegationRunning) ? 2_000 : false,
	});
	const displayedDelegationsQuery =
		showAllDelegations && expandedDelegationsQuery.data
			? expandedDelegationsQuery
			: defaultDelegationsQuery;
	const delegationListRetryScope = useMemo(
		() => ({
			parentSessionId: delegationParentSessionId,
			limit: showAllDelegations
				? RUN_BOARD_EXPANDED_DELEGATION_COUNT
				: RUN_BOARD_DEFAULT_DELEGATION_COUNT,
		}),
		[delegationParentSessionId, showAllDelegations],
	);
	const retryDelegations = useCallback(() => {
		try {
			assertServerReadAllowed();
		} catch (error) {
			reportActionError(error);
			return;
		}
		const refetch = showAllDelegations
			? expandedDelegationsQuery.refetch
			: defaultDelegationsQuery.refetch;
		void delegationListRetryController.current.retry(
			delegationListRetryScope,
			refetch,
		);
	}, [
		assertServerReadAllowed,
		defaultDelegationsQuery.refetch,
		delegationListRetryScope,
		expandedDelegationsQuery.refetch,
		reportActionError,
		showAllDelegations,
	]);
	const delegations = displayedDelegationsQuery.data?.delegations ?? [];
	const hasMoreDelegations =
		(showAllDelegations
			? expandedDelegationsQuery.data?.has_more
			: defaultDelegationsQuery.data?.has_more) ??
		defaultDelegationsQuery.data?.has_more ??
		false;
	const delegationsLoading = showAllDelegations
		? expandedDelegationsQuery.isFetching
		: defaultDelegationsQuery.isLoading;
	const delegationErrorScope = `${delegationParentSessionId ?? ""}:${
		showAllDelegations
			? RUN_BOARD_EXPANDED_DELEGATION_COUNT
			: RUN_BOARD_DEFAULT_DELEGATION_COUNT
	}`;
	const retainedDelegationErrorRef = useRef<{
		scope: string;
		error: unknown;
	}>({ scope: delegationErrorScope, error: null });
	if (retainedDelegationErrorRef.current.scope !== delegationErrorScope) {
		retainedDelegationErrorRef.current = {
			scope: delegationErrorScope,
			error: null,
		};
	}
	const currentDelegationError = showAllDelegations
		? expandedDelegationsQuery.error ?? (
			expandedDelegationsQuery.data ? null : defaultDelegationsQuery.error
		)
		: defaultDelegationsQuery.error;
	if (currentDelegationError) {
		retainedDelegationErrorRef.current.error = currentDelegationError;
	} else if (
		(showAllDelegations
			? expandedDelegationsQuery.data
			: defaultDelegationsQuery.data) !== undefined &&
		!(showAllDelegations
			? expandedDelegationsQuery.isFetching
			: defaultDelegationsQuery.isFetching)
	) {
		retainedDelegationErrorRef.current.error = null;
	}
	const delegationsError = errorMessageOrNull(
		currentDelegationError ?? retainedDelegationErrorRef.current.error,
	);
	const delegationsRetrying = showAllDelegations
		? expandedDelegationsQuery.isFetching
		: defaultDelegationsQuery.isFetching;
	// `delegating` status for the selected session: the parent reports idle while
	// its subagents are still in flight. Only known for the selected session,
	// whose delegations are fetched above.
	const hasRunningDelegations =
		loadedSnapshot?.session_id === delegationParentSessionId &&
		delegations.some(isDelegationRunning);
	const delegationSubagentIds = useMemo(
		() => delegations.flatMap((delegation) => delegation.subagents.map((subagent) => subagent.id)),
		[delegations],
	);
	const delegationSubagentNames = useMemo(() => {
		const names = new Map<string, string>();
		const knownSessions = new Map(allKnownSessions.map((session) => [session.session_id, session]));
		for (const delegation of delegations) {
			for (const subagent of delegation.subagents) {
				const session =
					(loadedSnapshot?.session_id === subagent.id ? loadedSnapshot : null) ??
					knownSessions.get(subagent.id) ??
					getSelectedCache(subagent.id)?.snapshot;
				// Prefer the title carried by `delegation.list` so a subagent's name
				// renders immediately, without waiting on a per-child session warm.
				// Fall back to the warmed snapshot title so a freshly-spawned child
				// whose title hasn't generated yet still gets named once hydrated.
				const name =
					subagent.title?.trim() ||
					(session ? sessionTitle(session, "").trim() : "") ||
					"Agent";
				names.set(subagent.id, name);
			}
		}
		return names;
	}, [
		allKnownSessions,
		backgroundWarmRevision,
		delegations,
		getSelectedCache,
		loadedSnapshot,
	]);
	const backgroundSubagentWarmCandidates = useMemo(
		() =>
			delegations.flatMap((delegation) =>
				delegation.subagents
					.filter((subagent) => subagent.id !== selectedId)
					.filter((subagent) => {
						if (backgroundWarmUpdatedAt.current.has(subagent.id)) return false;
						const cache = getSelectedCache(subagent.id);
						if (!cache?.snapshot) return true;
						if (cache.snapshot.activity !== subagent.activity && subagent.activity) return true;
						if (cache.snapshot.has_transcript_entries && cache.turnOrder.length === 0) return true;
						return subagentStatusNeedsWarm(subagent.status, subagent.activity);
					})
					.map((subagent) => subagent.id),
			),
		[backgroundWarmRevision, delegations, getSelectedCache, selectedId],
	);
	const reasoningEfforts = reasoningEffortsForProvider(activeProvider);
	const hasTranscriptEntries =
		loadedSnapshot?.has_transcript_entries ??
		selectedSession?.has_transcript_entries ??
		(loadedSnapshot ? loadedEntries.length > 0 || loadedSnapshot.active_leaf_id !== null : false);
	const modelLocked = !!selectedId && !!loadedSnapshot && hasTranscriptEntries;
	const modelControlsDisabled = !!selectedId && (!loadedSnapshot || loadedSnapshot.activity !== "idle");
	const reasoningControlsDisabled = !!selectedId && !loadedSnapshot;

	const applyConversationIdentity = useCallback((sessionId: string | null) => {
		const previousSessionId = selectedRef.current;
		if (sessionId === previousSessionId) {
			if (sessionId === null) nextSessionTitleRef.current = null;
			return;
		}
		if (sessionId === null) nextSessionTitleRef.current = null;
		setTranscriptDestination((current) =>
			current?.sessionId === sessionId ? current : null
		);
		setHistoryDialog((current) => current?.sessionId === sessionId ? current : null);
		selectedRef.current = sessionId;
		setConversationSessionId(sessionId);
		setShowAllDelegations(false);
		const nextCache = resetSelectedCache(sessionId);
		selectedFetchCoordinator.select(
			sessionId,
			hasUsableSelectedSessionCache(nextCache, sessionId),
		);
	}, [resetSelectedCache, selectedFetchCoordinator]);

	const acknowledgeTranscriptDestination = useCallback((destinationId: number) => {
		setTranscriptDestination((current) =>
			clearAcknowledgedTranscriptDestination(current, destinationId)
		);
	}, []);

	const applyProjectConversationIdentity = useCallback((projectId: string | null, sessionId: string | null) => {
		selectedProjectRef.current = projectId;
		sessionListCoordinator.selectProject(projectId);
		setSelectedProjectId(projectId);
		applyConversationIdentity(sessionId);
	}, [applyConversationIdentity, sessionListCoordinator]);

	const applyParsedWorkspaceRoute = useCallback(
		(parsed: WorkspaceRouteParseResult, options: { correct?: boolean } = {}) => {
			const next =
				options.correct !== false && parsed.kind === "route" && parsed.correction
					? routeHistory?.correct(parsed) ?? parsed
					: parsed;
			const unchangedValidatedRoute =
				next.kind === "route" &&
				workspaceRouteResultRef.current.kind === "route" &&
				routeValidationRef.current.kind === "valid" &&
				next.canonicalUrl === workspaceRouteResultRef.current.canonicalUrl;
			if (unchangedValidatedRoute) {
				setWorkspaceRouteResult(next);
				return next;
			}
			setHistoryDialog(null);
			routeValidationGenerationRef.current += 1;
			routeRemoteReadsEnabledRef.current = false;
			selectedFetchCoordinator.restart(
				hasUsableSelectedSessionCache(selectedCacheRef.current, selectedRef.current),
			);
			setRouteRevision((current) => current + 1);
			setWorkspaceRouteResult(next);
			if (next.kind === "route") {
				const projectId = routeScopeProjectId(next.route);
				const conversationId = routeConversationSessionId(next.route);
				setRouteValidation({ kind: "pending" });
				applyProjectConversationIdentity(projectId, conversationId);
				return next;
			}
			setRouteValidation(
				next.kind === "unavailable"
					? { kind: "unavailable", state: next, retryable: false }
					: { kind: "idle" },
			);
			if (next.kind === "none") {
				rememberUiSelection(selectedProjectRef.current, null);
			}
			applyConversationIdentity(null);
			return next;
		},
		[
			applyConversationIdentity,
			applyProjectConversationIdentity,
			routeHistory,
			selectedFetchCoordinator,
		],
	);

	const applyNavigation = useCallback(
		(navigation: RouteNavigation) => {
			const parsed = routeHistory?.apply(navigation) ?? parseWorkspaceRoute(navigation.url);
			if (parsed) applyParsedWorkspaceRoute(parsed);
		},
		[applyParsedWorkspaceRoute, routeHistory],
	);

	const openRootConversation = useCallback(
		(projectId: string | null, sessionId: string) => {
			applyNavigation(selectRootRun(routeScope(projectId), sessionId));
		},
		[applyNavigation],
	);

	const openConversation = useCallback(
		(sessionId: string) => {
			if (
				workspaceRouteResult.kind === "route" &&
				workspaceRouteResult.route.destination === "conversation" &&
				routeConversationSessionId(workspaceRouteResult.route) === sessionId
			) {
				return;
			}
			const knownSession = allKnownSessions.find((session) => session.session_id === sessionId);
			if (knownSession && !knownSession.parent_session_id) {
				openRootConversation(knownSession.project_id, sessionId);
				return;
			}
			if (
				workspaceRouteResult.kind === "route" &&
				(sessionId === workspaceRouteResult.route.rootSessionId ||
					sessionId === loadedSnapshot?.parent_session_id)
			) {
				applyNavigation(
					sessionId === workspaceRouteResult.route.rootSessionId
						? selectRootRun(workspaceRouteResult.route.scope, sessionId)
						: openAgentConversation(workspaceRouteResult.route, sessionId),
				);
				return;
			}
			if (workspaceRouteResult.kind === "route") {
				applyNavigation(openAgentConversation(workspaceRouteResult.route, sessionId));
				return;
			}
			openRootConversation(selectedProjectRef.current, sessionId);
		},
		[
			allKnownSessions,
			applyNavigation,
			loadedSnapshot?.parent_session_id,
			openRootConversation,
			workspaceRouteResult,
		],
	);

	const selectSession = useCallback(
		(sessionId: string | null) => {
			if (sessionId === null) {
				routeValidationGenerationRef.current += 1;
				const empty = routeHistory?.clear("push") ?? { kind: "none" as const };
				setWorkspaceRouteResult(empty);
				setRouteValidation({ kind: "idle" });
				rememberUiSelection(selectedProjectRef.current, null);
				applyConversationIdentity(null);
				return;
			}
			openConversation(sessionId);
		},
		[applyConversationIdentity, openConversation, routeHistory],
	);

	const selectProjectSession = useCallback(
		(projectId: string | null, sessionId: string | null) => {
			if (sessionId) {
				openRootConversation(projectId, sessionId);
				return;
			}
			routeValidationGenerationRef.current += 1;
			const empty = routeHistory?.clear("push") ?? { kind: "none" as const };
			setWorkspaceRouteResult(empty);
			setRouteValidation({ kind: "idle" });
			rememberUiSelection(projectId, null);
			applyProjectConversationIdentity(projectId, null);
		},
		[applyProjectConversationIdentity, openRootConversation, routeHistory],
	);

	useEffect(() => {
		if (
			!initialCorrectionAppliedRef.current &&
			initialWorkspaceRoute.kind === "route" &&
			initialWorkspaceRoute.correction
		) {
			initialCorrectionAppliedRef.current = true;
			applyParsedWorkspaceRoute(initialWorkspaceRoute);
		}
		return routeHistory?.subscribe((parsed) => {
			applyParsedWorkspaceRoute(parsed);
		});
	}, [applyParsedWorkspaceRoute, initialWorkspaceRoute, routeHistory]);

	const invalidateSessionList = useCallback(
		(projectId = selectedProjectRef.current) => {
			return queryClient.invalidateQueries({
				queryKey: queryKeys.sessions(projectId),
			});
		},
		[queryClient],
	);

	const scheduleSessionListRefresh = useCallback(
		(projectId = selectedProjectRef.current, delayMs = SESSION_LIST_REFRESH_DEBOUNCE_MS) => {
			const key = sessionListRefreshKey(projectId);
			if (sessionListRefreshTimers.current.has(key)) return;
			const timer = window.setTimeout(() => {
				sessionListRefreshTimers.current.delete(key);
				void invalidateSessionList(projectId);
			}, delayMs);
			sessionListRefreshTimers.current.set(key, timer);
		},
		[invalidateSessionList],
	);

	const fetchSessionSnapshot = useCallback(
		async (sessionId: string, source: string, validationRead = false) => {
			if (validationRead) assertConnectionReadAllowed();
			else assertServerReadAllowed();
			const shouldLogPerf = perfEnabled();
			const startedAt = perfNow();
			if (shouldLogPerf) perfLog("session.get start", { sessionId, source });
			const nextSnapshot = await api.getSession(sessionId, {
				includeEntries: false,
			});
			if (shouldLogPerf) {
				const rpcMs = perfNow() - startedAt;
				perfLog("session.get end", {
					sessionId,
					entries: nextSnapshot.entries?.length ?? 0,
					approxBytes: approximateJsonSize(nextSnapshot),
					rpcMs: Math.round(rpcMs),
					entryScope: "none",
				});
			}
			return nextSnapshot;
		},
		[api, assertConnectionReadAllowed, assertServerReadAllowed],
	);

	useEffect(() => {
		if (connection !== "open") return;
		if (
			workspaceRouteResult.kind === "route" &&
			routeValidationRef.current.kind === "valid" &&
			routeValidationRef.current.revision === routeRevision &&
			routeValidationRef.current.canonicalUrl === workspaceRouteResult.canonicalUrl
		) {
			return;
		}
		const generation = ++routeValidationGenerationRef.current;
		const stillCurrent = () => routeValidationGenerationRef.current === generation;
		const validateProject = (route: WorkspaceRoute, snapshot: SessionSnapshot) =>
			routeScopeProjectId(route) === snapshot.project_id;
		const run = async () => {
			if (workspaceRouteResult.kind === "none") {
				if (!legacyMigrationPendingRef.current || !initialUiSelection.sessionId) return;
				setRouteValidation({ kind: "pending" });
				try {
					const selected = await fetchSessionSnapshot(initialUiSelection.sessionId, "legacy-route", true);
					if (!stillCurrent()) return;
					const root = selected.parent_session_id
						? await fetchSessionSnapshot(selected.parent_session_id, "legacy-route-parent", true)
						: selected;
					if (!stillCurrent()) return;
					if (root.parent_session_id) {
						setRouteValidation({
							kind: "unavailable",
							state: routeRootUnavailable(
								"Nested agent conversations are not available because the backend currently exposes direct parents only.",
							),
							retryable: false,
						});
						return;
					}
					if (
						selected.project_id !== initialUiSelection.projectId ||
						root.project_id !== initialUiSelection.projectId
					) {
						setRouteValidation({
							kind: "unavailable",
							state: projectMismatchUnavailable(
								rootConversationRoute(routeScope(initialUiSelection.projectId), root.session_id),
								root.project_id,
							),
							retryable: false,
						});
						return;
					}
					legacyMigrationPendingRef.current = false;
					rememberUiSelection(initialUiSelection.projectId, null);
					const resume = legacyWorkspaceResume(workspaceRouteResult, initialUiSelection, {
						kind: "known",
						rootSessionId: root.session_id,
					});
					if (resume.kind === "legacy-route") applyNavigation(resume.navigation);
				} catch (error) {
					if (!stillCurrent()) return;
					setRouteValidation({
						kind: "unavailable",
						state: {
							kind: "unavailable",
							issue: "invalid-conversation",
							message: `Couldn’t restore the previous Conversation: ${errorMessage(error)}`,
							requestedUrl: "",
							backTo: null,
						},
						retryable: true,
					});
				}
				return;
			}
			if (workspaceRouteResult.kind !== "route") return;
			const route = workspaceRouteResult.route;
			setRouteValidation({ kind: "pending" });
			try {
				const root = await fetchSessionSnapshot(route.rootSessionId, "route-root", true);
				if (!stillCurrent()) return;
				if (!validateProject(route, root)) {
					setRouteValidation({
						kind: "unavailable",
						state: projectMismatchUnavailable(route, root.project_id),
						retryable: false,
					});
					return;
				}
				if (root.parent_session_id) {
					setRouteValidation({
						kind: "unavailable",
						state: routeRootUnavailable("The requested root run is an agent session, not a root session."),
						retryable: false,
					});
					return;
				}

				const conversationId = routeConversationSessionId(route);
				let conversation = root;
				if (conversationId !== route.rootSessionId) {
					try {
						conversation = await fetchSessionSnapshot(conversationId, "route-conversation", true);
					} catch (error) {
						if (!stillCurrent()) return;
						if (route.destination === "execution") {
							applyParsedWorkspaceRoute(
								fallbackExecutionConversation(route, "unavailable"),
							);
							return;
						}
						throw error;
					}
					if (!stillCurrent()) return;
					const projectMatches = validateProject(route, conversation);
					const validMembership =
						projectMatches &&
						conversation.parent_session_id === route.rootSessionId;
					if (!validMembership) {
						if (route.destination === "execution") {
							const fallback = fallbackExecutionConversation(
								route,
								projectMatches
									? "wrong-root-membership"
									: "unavailable",
							);
							applyParsedWorkspaceRoute(fallback);
						} else if (!projectMatches) {
							setRouteValidation({
								kind: "unavailable",
								state: projectMismatchUnavailable(route, conversation.project_id),
								retryable: false,
							});
						} else {
							setRouteValidation({
								kind: "unavailable",
								state: unavailableConversationRoute(
									route,
									"The requested Conversation is not a direct agent of this root run.",
								),
								retryable: false,
							});
						}
						return;
					}
				}

				if (
					route.destination === "execution" &&
					route.focus.kind === "agent" &&
					route.focus.sessionId !== conversation.session_id
				) {
					let focused: SessionSnapshot;
					try {
						focused = await fetchSessionSnapshot(route.focus.sessionId, "route-focus", true);
					} catch {
						if (!stillCurrent()) return;
						setRouteValidation({
							kind: "unavailable",
							state: unavailableExecutionDetail(route, "focus"),
							retryable: false,
						});
						return;
					}
					if (!stillCurrent()) return;
					if (
						!validateProject(route, focused) ||
						focused.parent_session_id !== route.rootSessionId
					) {
						setRouteValidation({
							kind: "unavailable",
							state: unavailableExecutionDetail(route, "focus"),
							retryable: false,
						});
						return;
					}
				}
				if (route.destination === "execution" && route.focus.kind === "delegation") {
					setRouteValidation({
						kind: "unavailable",
						state: unavailableExecutionDetail(
							route,
							"focus",
							"Delegation focus is not available because the backend does not expose an unbounded canonical delegation lookup.",
						),
						retryable: false,
					});
					return;
				}
				if (route.destination === "execution" && route.handoff) {
					setRouteValidation({
						kind: "unavailable",
						state: unavailableExecutionDetail(
							route,
							"handoff",
							"The requested handoff is not available.",
						),
						retryable: false,
					});
					return;
				}
				rememberUiSelection(routeScopeProjectId(route), null);
				setRouteValidation({
					kind: "valid",
					revision: routeRevision,
					canonicalUrl: workspaceRouteResult.canonicalUrl,
					projectId: routeScopeProjectId(route),
					conversationSessionId: routeConversationSessionId(route),
				});
			} catch (error) {
				if (!stillCurrent()) return;
				setRouteValidation({
					kind: "unavailable",
					state:
						route.destination === "conversation"
							? unavailableConversationRoute(
								route,
								`Couldn’t load the requested conversation: ${errorMessage(error)}`,
							)
							: routeRootUnavailable(
								`Couldn’t load the requested execution: ${errorMessage(error)}`,
							),
					retryable: true,
				});
			}
		};
		void run();
		return () => {
			if (routeValidationGenerationRef.current === generation) {
				routeValidationGenerationRef.current += 1;
			}
		};
	}, [
		applyNavigation,
		applyParsedWorkspaceRoute,
		connection,
		fetchSessionSnapshot,
		initialUiSelection,
		routeRevision,
		routeValidationRetry,
		workspaceRouteResult,
	]);

	const commitSelectedSnapshot = useCallback(
		(snapshot: SessionSnapshot) => {
			const observedEventId = lastEventIds.current.get(snapshot.session_id) ?? 0;
			lastEventIds.current.set(snapshot.session_id, Math.max(observedEventId, snapshot.last_event_id));
			if (snapshot.session_id === selectedRef.current) {
				updateSelectedCache((current) =>
					applySelectedSnapshot(current.sessionId === snapshot.session_id ? current : emptySelectedSessionCache(snapshot.session_id), snapshot),
				);
			}
			mergeSnapshotIntoKnownSessionLists(snapshot);
		},
		[mergeSnapshotIntoKnownSessionLists],
	);

	const fetchTranscriptTurns = useCallback(
		(sessionId: string) => {
			assertServerReadAllowed();
			return api.getTranscriptTurns(sessionId, { limit: TRANSCRIPT_TURN_PAGE_SIZE });
		},
		[api, assertServerReadAllowed],
	);

	const refreshTranscriptTurns = useCallback(
		async (sessionId: string, selectionVersion?: number) => {
			const result = await fetchTranscriptTurns(sessionId);
			if (selectedRef.current !== sessionId) return null;
			if (
				selectionVersion !== undefined &&
				!selectedFetchCoordinator.isCurrent(sessionId, selectionVersion)
			) {
				return null;
			}
			updateSelectedCache((current) => applyTranscriptTurns(current.sessionId === sessionId ? current : selectedCacheRef.current, result));
			return result;
		},
		[fetchTranscriptTurns, selectedFetchCoordinator, updateSelectedCache],
	);

	const warmBackgroundSession = useCallback(
		async (sessionId: string) => {
			if (selectedRef.current === sessionId) return false;
			const snapshot = await fetchSessionSnapshot(sessionId, "background");
			if (selectedRef.current === sessionId) return false;
			const observedEventId = lastEventIds.current.get(snapshot.session_id) ?? 0;
			lastEventIds.current.set(snapshot.session_id, Math.max(observedEventId, snapshot.last_event_id));
			mergeSnapshotIntoKnownSessionLists(snapshot);
			warmSelectedCache(sessionId, (current) => applySelectedSnapshot(current, snapshot));
			if (snapshot.has_transcript_entries) {
				const turns = await fetchTranscriptTurns(sessionId);
				if (selectedRef.current === sessionId) return false;
				const nextSnapshot = snapshotWithTranscriptTurnsMetadata(snapshot, turns);
				warmSelectedCache(sessionId, (current) =>
					applyTranscriptTurns(applySelectedSnapshot(current, nextSnapshot), turns),
				);
			}
			return true;
		},
		[fetchSessionSnapshot, fetchTranscriptTurns, mergeSnapshotIntoKnownSessionLists, warmSelectedCache],
	);

	useEffect(() => {
		if (connection !== "open" || !routeRemoteReadsEnabled) return;
		const sessionCandidates = allKnownSessions
			.filter((session) => session.session_id !== selectedRef.current)
			.filter(canWarmBackgroundSession)
			.filter((session) =>
				backgroundSessionNeedsWarm(
					session,
					getSelectedCache(session.session_id),
					backgroundWarmUpdatedAt.current.get(session.session_id),
				),
			);
		const candidates = [
			...sessionCandidates.map((session) => ({
				id: session.session_id,
				updatedAt: session.updated_at,
				label: "session",
			})),
			...backgroundSubagentWarmCandidates.map((sessionId) => ({
				id: sessionId,
				updatedAt: undefined,
				label: "subagent",
			})),
		];
		let availableSlots = Math.max(0, BACKGROUND_SESSION_WARM_CONCURRENCY - backgroundWarmInFlight.current.size);
		for (const candidate of candidates) {
			if (availableSlots <= 0) break;
			if (backgroundWarmInFlight.current.has(candidate.id)) continue;
			availableSlots -= 1;
			backgroundWarmInFlight.current.add(candidate.id);
			void warmBackgroundSession(candidate.id)
				.then((warmed) => {
					if (!warmed) return;
					if (candidate.updatedAt) backgroundWarmUpdatedAt.current.set(candidate.id, candidate.updatedAt);
					else backgroundWarmUpdatedAt.current.set(candidate.id, "subagent");
					if (candidate.label === "subagent") {
						setBackgroundWarmRevision((current) => current + 1);
					}
				})
				.catch((error) => {
					console.warn(`background ${candidate.label} warm failed`, candidate.id, error);
				})
				.finally(() => {
					backgroundWarmInFlight.current.delete(candidate.id);
				});
		}
	}, [
		allKnownSessions,
		backgroundSubagentWarmCandidates,
		connection,
		getSelectedCache,
		routeRemoteReadsEnabled,
		warmBackgroundSession,
	]);

	const loadOlderTranscriptTurns = useCallback(
		async (request: OlderTurnsLoadRequest): Promise<OlderTurnsLoadResult> => {
			const sessionId = selectedRef.current;
			const resultFor = (
				status: OlderTurnsLoadResult["status"],
				turnPageHydrationRevision?: number,
			): OlderTurnsLoadResult => ({ ...request, status, turnPageHydrationRevision });
			if (!sessionId || sessionId !== request.sessionId || loadingOlderTurns) return resultFor("stale");
			const cache = selectedCacheRef.current;
			if (cache.sessionId !== sessionId || !cache.turnHasMoreBefore || !cache.turnBeforeEntryId) {
				return resultFor("noop");
			}
			const beforeEntryId = cache.turnBeforeEntryId;
			try {
				assertServerReadAllowed();
				setLoadingOlderTurns(true);
				const result = await api.getTranscriptTurns(sessionId, {
					beforeEntryId,
					limit: TRANSCRIPT_TURN_PAGE_SIZE,
				});
				if (selectedRef.current !== sessionId) return resultFor("stale");
				const completion = prependTranscriptTurns(selectedCacheRef.current, result);
				replaceSelectedCache(completion.cache);
				return resultFor(completion.status, completion.turnPageHydrationRevision);
			} catch (error) {
				if (selectedRef.current === sessionId) reportActionError(error);
				return resultFor("failed");
			} finally {
				setLoadingOlderTurns(false);
			}
		},
		[api, assertServerReadAllowed, loadingOlderTurns, replaceSelectedCache, reportActionError],
	);

	const getFreshSession = useCallback(
		async (sessionId: string, selectionVersion: number) => {
			const snapshot = await fetchSessionSnapshot(sessionId, "fetch");
			if (!selectedFetchCoordinator.isCurrent(sessionId, selectionVersion)) return null;
			commitSelectedSnapshot(snapshot);
			let turns: TranscriptTurnsResult | null = null;
			try {
				turns = await refreshTranscriptTurns(sessionId, selectionVersion);
			} finally {
				if (
					selectedFetchCoordinator.isCurrent(sessionId, selectionVersion) &&
					selectedCacheRef.current.snapshot?.session_id !== sessionId
				) {
					commitSelectedSnapshot(turns ? snapshotWithTranscriptTurnsMetadata(snapshot, turns) : snapshot);
				}
			}
			if (
				selectedFetchCoordinator.isCurrent(sessionId, selectionVersion) &&
				snapshot.project_id !== selectedProjectRef.current
			) {
				// Route validation owns project identity. A route/project mismatch
				// is rendered explicitly rather than silently changing scope here.
			}
			if (!selectedFetchCoordinator.isCurrent(sessionId, selectionVersion)) return null;
			const cache = selectedCacheRef.current;
			if (!hasUsableSelectedSessionCache(cache, sessionId)) {
				throw new Error("selected session transcript did not finish loading");
			}
			return {
				snapshot: cache.sessionId === sessionId && cache.snapshot ? cache.snapshot : snapshot,
				entries: cache.sessionId === sessionId ? selectedEntries(cache) : [],
			};
		},
		[
			assertServerReadAllowed,
			commitSelectedSnapshot,
			fetchSessionSnapshot,
			refreshTranscriptTurns,
			selectedFetchCoordinator,
			sessionListCoordinator,
		],
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
		async (
			sessionId: string,
			options: { preserveRenderedCache?: boolean } = {},
		) => {
			if (sessionId !== selectedRef.current) return null;
			assertServerReadAllowed();
			const cacheBeforeRequest = selectedCacheRef.current;
			const hasUsableCache =
				options.preserveRenderedCache ||
				hasUsableSelectedSessionCache(cacheBeforeRequest, sessionId);
			return selectedFetchCoordinator.run(sessionId, hasUsableCache, async (selectionVersion) => {
				for (;;) {
					const cacheAtRefreshStart = selectedCacheRef.current;
					const currentSnapshot =
						cacheAtRefreshStart.sessionId === sessionId
							? cacheAtRefreshStart.snapshot
							: null;
					let result: { snapshot: SessionSnapshot; entries: TranscriptEntry[] } | null;
					if (!currentSnapshot) {
						result = await getFreshSession(sessionId, selectionVersion);
						return result;
					}
					const refreshFence = captureSelectedSessionRefresh(cacheAtRefreshStart);
					const snapshot = await fetchSessionSnapshot(sessionId, "refresh");
					if (!selectedFetchCoordinator.isCurrent(sessionId, selectionVersion)) return null;
					let nextCache = applySelectedSnapshot(
						cacheAtRefreshStart,
						snapshot,
					);
					const needsTurns =
						!cacheAtRefreshStart.transcriptTurnsLoaded ||
						cacheAtRefreshStart.turnTranscriptRevision !== (snapshot.transcript_revision ?? null) ||
						cacheAtRefreshStart.turnActiveLeafId !== (snapshot.active_leaf_id ?? null) ||
						(snapshot.has_transcript_entries && cacheAtRefreshStart.turnOrder.length === 0);
					if (needsTurns) {
						const turns = await fetchTranscriptTurns(sessionId);
						if (!selectedFetchCoordinator.isCurrent(sessionId, selectionVersion)) return null;
						nextCache = applyTranscriptTurns(nextCache, turns);
					}
					if (!selectedFetchCoordinator.isCurrent(sessionId, selectionVersion)) return null;
					if (!hasUsableSelectedSessionCache(nextCache, sessionId)) {
						throw new Error("selected session transcript did not finish loading");
					}
					const commit = commitSelectedSessionRefresh(
						refreshFence,
						selectedCacheRef.current,
						nextCache,
					);
					if (!commit.committed) continue;
					replaceSelectedCache(commit.cache);
					const committedSnapshot = nextCache.snapshot ?? snapshot;
					const observedEventId = lastEventIds.current.get(sessionId) ?? 0;
					lastEventIds.current.set(sessionId, Math.max(observedEventId, committedSnapshot.last_event_id));
					mergeSnapshotIntoKnownSessionLists(committedSnapshot);
					if (committedSnapshot.project_id !== selectedProjectRef.current) {
						// Route validation owns project identity.
					}
					result = {
						snapshot: committedSnapshot,
						entries: selectedEntries(nextCache),
					};
					return result;
				}
			});
		},
		[
			commitSelectedSnapshot,
			fetchTranscriptTurns,
			fetchSessionSnapshot,
			getFreshSession,
			mergeSnapshotIntoKnownSessionLists,
			replaceSelectedCache,
			selectedFetchCoordinator,
			sessionListCoordinator,
		],
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
	const retrySelected = useCallback(() => {
		const sessionId = selectedRef.current;
		if (!sessionId) return;
		try {
			assertServerReadAllowed();
		} catch (error) {
			reportActionError(error);
			return;
		}
		void refreshSelectedSessionState(sessionId).catch(() => undefined);
	}, [assertServerReadAllowed, refreshSelectedSessionState, reportActionError]);

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
			const shouldLogPerf = perfEnabled();
			const startedAt = perfNow();
			if (shouldLogPerf) {
				perfLog("transcript.turn_detail start", {
					sessionId,
					cardId,
					mode: options.mode,
					startSequence: card.start_sequence,
					endSequence: card.end_sequence,
					sequenceSpan: card.end_sequence - card.start_sequence + 1,
				});
			}
			try {
				assertServerReadAllowed();
				if (options.mode === "manual") setLoadingTurnId(cardId);
				else setAutoLoadingTurnId(cardId);
				const result = await api.getTranscriptTurnDetail(sessionId, {
					cardId: card.id,
					leafId: card.active_leaf_id,
					startSequence: card.start_sequence,
					endSequence: card.end_sequence,
				});
				const rpcMs = perfNow() - startedAt;
				if (selectedRef.current !== sessionId) return;
				let applied = false;
				const applyStartedAt = perfNow();
				updateSelectedCache((current) => {
					const detail = applyTurnDetail(current.sessionId === sessionId ? current : selectedCacheRef.current, sessionId, result.card_id, result.entries);
					applied = detail.applied;
					return detail.cache;
				});
				const applyMs = perfNow() - applyStartedAt;
				if (applied && options.mode === "manual") setExpandedTurnIds((current) => new Set(current).add(result.card_id));
				if (shouldLogPerf) {
					perfLog("transcript.turn_detail end", {
						sessionId,
						cardId: result.card_id,
						mode: options.mode,
						applied,
						entries: result.entries.length,
						approxBytes: approximateJsonSize(result),
						rpcMs: Math.round(rpcMs),
						applyMs: Math.round(applyMs),
						totalBeforePaintMs: Math.round(perfNow() - startedAt),
					});
					requestAnimationFrame(() => {
						perfLog("transcript.turn_detail paint", {
							sessionId,
							cardId: result.card_id,
							mode: options.mode,
							totalMs: Math.round(perfNow() - startedAt),
						});
					});
				}
			} catch (error) {
				if (selectedRef.current === sessionId) reportActionError(error);
			} finally {
				if (options.mode === "manual") setLoadingTurnId((current) => (current === cardId ? null : current));
				else setAutoLoadingTurnId((current) => (current === cardId ? null : current));
			}
		},
		[api, assertServerReadAllowed, reportActionError, updateSelectedCache],
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
		if (connectionRemoteActionBlockedReason) return;
		const cache = selectedCacheRef.current;
		const card = cache.turnCardsById.get(runningTurnCardId);
		const autoLoadKey = card ? `${runningTurnCardId}:${card.active_leaf_id}` : runningTurnCardId;
		if (autoLoadedTurnDetailRef.current === autoLoadKey) return;
		if (turnDetailEntries(cache, runningTurnCardId)) return;
		autoLoadedTurnDetailRef.current = autoLoadKey;
		void loadTurnDetail(runningTurnCardId, { mode: "auto" });
	}, [autoLoadingTurnId, connectionRemoteActionBlockedReason, loadTurnDetail, runningTurnCardId, selectedCache.turnCardsById, selectedCache.turnDetailsById]);

	useEffect(() => {
		autoLoadedTurnDetailRef.current = null;
	}, [selectedId]);

	const reconcileAfterForeground = useCallback(
		(options: { forceReconnect?: boolean } = {}) => {
			if (typeof document !== "undefined" && document.visibilityState === "hidden") return;
			const now = Date.now();
			if (now - lastForegroundReconcileAt.current < FOREGROUND_RECONCILE_THROTTLE_MS) return;
			lastForegroundReconcileAt.current = now;
			const sessionId = selectedRef.current;
			const reconcile = () => {
				void invalidateKnownSessionLists();
				if (!sessionId) return;
				void syncActiveBranchNow(sessionId).catch(() => undefined);
			};
			if (!options.forceReconnect) {
				reconcile();
				return;
			}
			if (!foregroundReconnectInFlight.current) {
				foregroundReconnectInFlight.current = api.reconnect().finally(() => {
					foregroundReconnectInFlight.current = null;
				});
			}
			void foregroundReconnectInFlight.current
				.then(reconcile)
				.catch(() => undefined);
		},
		[api, invalidateKnownSessionLists, syncActiveBranchNow],
	);

	const retryConnection = useCallback(() => {
		setRetryingConnection(true);
		void connectionRetryController.current.retry(
			() => api.reconnect(),
			() => undefined,
			() => setRetryingConnection(false),
		);
	}, [api]);

	useEffect(() => {
		const awakeHeartbeat = window.setInterval(() => {
			const now = Date.now();
			if (now - lastAwakeAt.current < FOREGROUND_RECONNECT_AFTER_MS) {
				lastAwakeAt.current = now;
			}
		}, AWAKE_HEARTBEAT_MS);
		const reconcileMaybeAfterSleep = () => {
			const sleptForMs = Date.now() - lastAwakeAt.current;
			lastAwakeAt.current = Date.now();
			reconcileAfterForeground({ forceReconnect: sleptForMs >= FOREGROUND_RECONNECT_AFTER_MS });
		};
		const onVisibilityChange = () => {
			if (document.visibilityState === "visible") reconcileMaybeAfterSleep();
		};
		const onFocus = () => reconcileMaybeAfterSleep();
		const onPageShow = (event: PageTransitionEvent) => {
			if (event.persisted) reconcileAfterForeground({ forceReconnect: true });
		};
		document.addEventListener("visibilitychange", onVisibilityChange);
		window.addEventListener("focus", onFocus);
		window.addEventListener("pageshow", onPageShow);
		return () => {
			window.clearInterval(awakeHeartbeat);
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
				void refreshSelectedSessionState(sessionId).catch(() => undefined);
			}, delayMs);
		},
		[refreshSelectedSessionState],
	);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current) => {
			if (!sessionId) return null;
			return refreshSelectedSessionState(sessionId);
		},
		[refreshSelectedSessionState],
	);

	useEffect(() => {
		if (connection !== "open" || !routeRemoteReadsEnabled) return;
		if (!selectedId) {
			resetSelectedCache(null);
			if (selectedFetchCoordinator.getSnapshot().sessionId !== null) {
				selectedFetchCoordinator.select(null, false);
			}
			return;
		}
		selectedFetchCoordinator.restart(
			hasUsableSelectedSessionCache(selectedCacheRef.current, selectedId),
		);
		void refreshSelectedSessionState(selectedId).catch(() => undefined);
	}, [
		connection,
		refreshSelectedSessionState,
		resetSelectedCache,
		routeRemoteReadsEnabled,
		selectedFetchCoordinator,
		selectedId,
	]);

	const handleSessionEvent = useCallback(
		(event: EventFrame) => {
			const currentSessions = queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(selectedProjectRef.current));
			const eventSession = currentSessions?.find((session) => session.session_id === event.session_id);
			const eventProjectId =
				firstKnownProjectId(
					projectIdFromEventData(event),
					eventSession?.project_id,
					loadedSnapshot?.session_id === event.session_id ? loadedSnapshot.project_id : undefined,
					cachedProjectIdForSession(queryClient, event.session_id),
				) ?? selectedProjectRef.current;
			const previousEventId = lastEventIds.current.get(event.session_id) ?? 0;
			if (event.event_id <= previousEventId) return;

			const refreshPlan = refreshPlanForEvent(event);
			lastEventIds.current.set(event.session_id, event.event_id);
			backgroundWarmUpdatedAt.current.delete(event.session_id);
			let shouldSyncSelected = refreshPlan.syncSelected && event.session_id === selectedRef.current;
			if (event.session_id === selectedRef.current) {
				const queue = queueProjectionFromEvent(event);
				if (queue) {
					replaceSelectedCache(
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
				replaceSelectedCache(
					applyEventHighWater(
						selectedCacheRef.current,
						event.session_id,
						event.event_id,
					),
				);
			}
			if (shouldSyncSelected) scheduleActiveBranchSync(event.session_id);
			const activity = activityFromEvent(event);
			for (const projectId of sessionListProjectTargets(eventProjectId)) {
				patchSessionListEventSummary(queryClient, projectId, event, activity);
			}
			if (refreshPlan.refreshList) {
				for (const projectId of sessionListProjectTargets(eventProjectId)) {
					scheduleSessionListRefresh(projectId);
				}
				if (delegationParentSessionId) {
					// The Agents outline reads delegation.*; because there is no dedicated
					// delegation event, subagent lifecycle events and the typed completion
					// observation refresh it. The 2s poll covers any missed event.
					void queryClient.invalidateQueries({ queryKey: delegationQueryPrefix(delegationParentSessionId) });
				}
			}

			if (event.session_id === selectedRef.current) {
				if (event.event === "model.error") pushErrorNotice(modelErrorNotice(event.data));
				if (event.event === "compaction.error") pushErrorNotice(compactionErrorNotice(event.data));
				if (event.event === "subagent.idle") {
					const outcome = typeof event.data.outcome === "string" ? event.data.outcome : null;
					if (outcome === "Crashed") pushErrorNotice(subagentFailureNotice(event.data));
				}
				if (event.event === "turn.finished") {
					const outcome = typeof event.data.outcome === "string" ? event.data.outcome : null;
					if (outcome === "Crashed") pushErrorNotice("turn crashed");
				}
			}
		},
		[
			pushErrorNotice,
			queryClient,
			replaceSelectedCache,
			scheduleActiveBranchSync,
			scheduleSessionListRefresh,
			delegationParentSessionId,
			loadedSnapshot?.session_id,
			loadedSnapshot?.project_id,
		],
	);

	useEffect(() => {
		handleSessionEventRef.current = handleSessionEvent;
	}, [handleSessionEvent]);

	useEffect(() => {
		const offStatus = api.onStatus((status) => {
			connectionRef.current = status;
			setConnection(status);
			if (status === "open") setDisconnected(false);
			else if (status !== "connecting") setDisconnected(true);
			subscribedEventSessionIds.current.clear();
			if (status !== "open") return;
			connectionRetryController.current.opened();
			setRetryingConnection(false);
			void Promise.all([
				queryClient.invalidateQueries({ queryKey: queryKeys.projects }),
				queryClient.invalidateQueries({ queryKey: queryKeys.systemPromptRoot }),
				invalidateKnownSessionLists(),
			]).catch(() => undefined);
		});
		const offEvent = api.onEvent((event) => handleSessionEventRef.current(event));
		void api.connect().catch(() => undefined);
		return () => {
			offStatus();
			offEvent();
			if (selectedSyncTimer.current !== null) window.clearTimeout(selectedSyncTimer.current);
			for (const timer of sessionListRefreshTimers.current.values()) window.clearTimeout(timer);
			sessionListRefreshTimers.current.clear();
			api.close();
		};
	}, [api, invalidateKnownSessionLists, queryClient]);

	useEffect(() => {
		if (toolsQuery.error) pushErrorNotice(errorMessage(toolsQuery.error));
	}, [toolsQuery.error, pushErrorNotice]);

	useEffect(() => {
		if (projectsQuery.status !== "success") return;
		const currentProjectId = selectedProjectRef.current;
		if (currentProjectId === null || projects.some((project) => project.project_id === currentProjectId)) return;
		if (workspaceRouteResult.kind === "route") {
			if (routeValidation.kind !== "valid") return;
			setRouteValidation({
				kind: "unavailable",
				state: projectMismatchUnavailable(workspaceRouteResult.route, null),
				retryable: false,
			});
			return;
		}
		selectProjectSession(null, null);
		setQuery("");
		composerHandleRef.current?.setValue("");
	}, [
		projects,
		projectsQuery.status,
		routeValidation.kind,
		selectProjectSession,
		workspaceRouteResult,
	]);

	useEffect(() => {
		if (!loadedSnapshot) return;
		const observedEventId = lastEventIds.current.get(loadedSnapshot.session_id) ?? 0;
		lastEventIds.current.set(loadedSnapshot.session_id, Math.max(observedEventId, loadedSnapshot.last_event_id));
		mergeSnapshotIntoKnownSessionLists(loadedSnapshot);
		// `last_event_id` is a transient replay cursor for the daemon's in-memory-ish
		// event buffer. The daemon may clear old event rows after a session becomes
		// idle, so a fresh `session.get` can legitimately report a smaller cursor
		// than this tab has already observed. Revisions and explicit
		// foreground/reconnect reconciliation drive freshness; never use the event
		// cursor mismatch as a durable selected-session refresh trigger.
	}, [loadedSnapshot, mergeSnapshotIntoKnownSessionLists]);

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
				assertServerReadAllowed();
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
		[api, assertServerReadAllowed],
	);

	useEffect(() => {
		if (connection !== "open") return;
		const selectedHasEventCursor =
			!!selectedId && (lastEventIds.current.has(selectedId) || loadedSnapshot?.session_id === selectedId);
		const desiredSessionIds = new Set<string>();
		if (routeRemoteReadsEnabled) {
			for (const session of sessions) {
				desiredSessionIds.add(session.session_id);
			}
			for (const delegationSubagentId of delegationSubagentIds) {
				desiredSessionIds.add(delegationSubagentId);
			}
			if (selectedId && selectedHasEventCursor) desiredSessionIds.add(selectedId);
		}
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
					pushErrorNotice(errorMessage(error));
				});
		}
	}, [
		api,
		connection,
		handleSessionEvent,
		loadedSnapshot?.last_event_id,
		loadedSnapshot?.session_id,
		pushErrorNotice,
		routeRemoteReadsEnabled,
		selectedId,
		sessions,
		delegationSubagentIds,
	]);

	const commitConfiguredProvider = useCallback(
		(
			target: ProviderConfigurationTarget,
			provider: ProviderConfig,
			result: Awaited<ReturnType<AgentApi["configureSession"]>>,
		) => {
			patchSessionListProvider(queryClient, target.projectId, target.sessionId, provider);
			warmSelectedCache(target.sessionId, (current) => {
				if (!current.snapshot) return current;
				return applySelectedSnapshot(current, {
					...current.snapshot,
					entries: selectedEntries(current),
					provider,
					metadata: result.metadata ?? current.snapshot.metadata,
					activity: result.activity,
				});
			});
			invalidateSessionList(target.projectId);
		},
		[invalidateSessionList, queryClient, warmSelectedCache],
	);
	if (!providerConfigurationControllerRef.current) {
		providerConfigurationControllerRef.current = new ProviderConfigurationController({
			configure: (target, provider) => {
				assertServerMutationAllowed();
				return api.configureSession({ sessionId: target.sessionId, provider });
			},
			commit: commitConfiguredProvider,
			fail: (target, edit, error) => {
				pushErrorNotice(`Could not update ${edit}: ${errorMessage(error)}. Try again.`, true);
				invalidateSessionList(target.projectId);
				if (selectedRef.current === target.sessionId) {
					void refreshSelectedSessionState(target.sessionId).catch(() => undefined);
				}
			},
			change: () => setProviderConfigurationRevision((revision) => revision + 1),
		});
	}
	const providerConfigurationController = providerConfigurationControllerRef.current;
	useEffect(() => {
		const generation = ++providerConfigurationMountGenerationRef.current;
		return () => {
			queueMicrotask(() => {
				if (providerConfigurationMountGenerationRef.current === generation) {
					providerConfigurationController.dispose();
				}
			});
		};
	}, [providerConfigurationController]);

	const changeModel = useCallback(
		(modelKey: string) => {
			if (modelLocked) return;
			const sessionId = selectedRef.current;
			if (!sessionId) {
				const provider = providerFromModelKey(modelKey, activeProvider);
				const providerChange = mcpSelectionForProviderChange(
					mcpSelectionProviderRef.current,
					provider.kind,
					mcpSelectionRef.current,
				);
				if (mcpSelectionProviderRef.current !== provider.kind) {
					mcpSelectionProviderRef.current = provider.kind;
					previousMcpInventoryRef.current = null;
					mcpSelectionRef.current = providerChange.selection;
					setMcpSelection(providerChange.selection);
				}
				setNewSessionProvider(provider);
				setNewSessionSetupGeneration((generation) => generation + 1);
				return;
			}
			assertServerMutationAllowed();
			providerConfigurationController.update(
				{ sessionId, projectId: selectedProjectRef.current },
				activeProvider,
				(provider) => providerFromModelKey(modelKey, provider),
				"model",
			);
		},
		[
			activeProvider,
			assertServerMutationAllowed,
			modelLocked,
			providerConfigurationController,
		],
	);

	const changeReasoningEffort = useCallback(
		(effort: ReasoningEffort) => {
			const sessionId = selectedRef.current;
			if (!sessionId) {
				setNewSessionProvider(withReasoningEffort(activeProvider, effort));
				setNewSessionSetupGeneration((generation) => generation + 1);
				return;
			}
			assertServerMutationAllowed();
			providerConfigurationController.update(
				{ sessionId, projectId: selectedProjectRef.current },
				activeProvider,
				(provider) => withReasoningEffort(provider, effort),
				"reasoning effort",
			);
		},
		[activeProvider, assertServerMutationAllowed, providerConfigurationController],
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
		assertServerMutationAllowed();
		const projectId = selectedProjectRef.current;
		const title = renameValue.trim();
		if (!title) throw new Error("session title is required");
		const result = await api.renameSession(renameSessionId, title);
		patchSessionListMetadata(queryClient, projectId, renameSessionId, { title });
		patchSelectedSnapshot(renameSessionId, (snapshot) => ({
			...snapshot,
			metadata: result.metadata ?? { ...snapshot.metadata, title },
			activity: result.activity,
		}));
		invalidateSessionList(projectId);
		closeRenameDialog();
	}, [api, assertServerMutationAllowed, closeRenameDialog, invalidateSessionList, patchSelectedSnapshot, queryClient, renameSessionId, renameValue]);

	const setSessionArchived = useCallback(
		async (session: SessionListItem, archived: boolean) => {
			assertServerMutationAllowed();
			const sessionId = session.session_id;
			const projectId = session.project_id;
			const currentSnapshot = loadedSnapshot?.session_id === sessionId ? loadedSnapshot : null;
			const activity = currentSnapshot?.activity ?? session.activity;
			if (activity !== "idle") throw new Error("only idle sessions can be archived");
			if (session.has_running_delegations ?? false) throw new Error("can't archive a session with running subagents");
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
				projectId,
				sessionId,
				archived ? { archived: true } : {},
				archived ? [] : ["archived"],
			);
			patchSelectedSnapshot(sessionId, (snapshot) => ({
				...snapshot,
				metadata: result.metadata ?? metadata,
				activity: result.activity,
			}));
			invalidateSessionList(projectId);
		},
		[api, assertServerMutationAllowed, invalidateSessionList, loadedSnapshot, patchSelectedSnapshot, queryClient],
	);

	const closeDeleteDialog = useCallback(() => {
		setDeleteDialog(null);
	}, []);

	const deleteSession = useCallback(async () => {
		if (!deleteDialog || deleteDialog.deleting) return;
		assertServerMutationAllowed();
		setDeleteDialog((current) => (current ? { ...current, deleting: true } : current));
		const session = deleteDialog.session;
		const sessionId = session.session_id;
		try {
			const current = sessionId === selectedRef.current ? await refreshSelected(sessionId) : null;
			const activity = current?.snapshot.activity ?? session.activity;
			if (activity !== "idle") throw new Error("only idle sessions can be deleted");
			if (session.has_running_delegations ?? false) throw new Error("can't delete a session with running subagents");

			await api.deleteSession(sessionId);
			if (selectedSyncTimer.current !== null) {
				window.clearTimeout(selectedSyncTimer.current);
				selectedSyncTimer.current = null;
			}
			lastEventIds.current.delete(sessionId);
			backgroundWarmUpdatedAt.current.delete(sessionId);
			backgroundWarmInFlight.current.delete(sessionId);
			dropSelectedCache(sessionId);
			removeSessionFromKnownSessionLists(sessionId, session.project_id);
			composerHandleRef.current?.clearSession(sessionId);

			if (selectedRef.current === sessionId) {
				selectSession(null);
				composerHandleRef.current?.setValue("");
			}

			closeDeleteDialog();
			invalidateSessionList(session.project_id);
		} catch (error) {
			setDeleteDialog((current) => (current?.session.session_id === sessionId ? { ...current, deleting: false } : current));
			if (isSelectedSessionFetchError(error)) return;
			throw error;
		}
	}, [api, assertServerMutationAllowed, closeDeleteDialog, deleteDialog, dropSelectedCache, invalidateSessionList, refreshSelected, removeSessionFromKnownSessionLists, selectSession]);

	const createSession = useCallback(
		(title?: string) => {
			nextSessionTitleRef.current = title?.trim() || null;
			mcpSelectionRef.current = new Map();
			setMcpSelection(new Map());
			setNewSessionSetupGeneration((generation) => generation + 1);
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
		async (
			sessionId: string,
			text: string,
			snapshot: SessionSnapshot,
			clientInputId: string,
		) => {
			assertServerMutationAllowed();
			const projectId = snapshot.project_id;
			const baseLeafId = selectedBaseLeafId(
				selectedCacheRef.current,
				sessionId,
				snapshot.active_leaf_id ?? null,
			);
			if (isArchivedSession(snapshot)) {
				const current = snapshot;
				if (current.activity !== "idle") {
					throw new Error("only idle archived sessions can be resumed");
				}
				const metadata = { ...current.metadata };
				delete metadata.archived;
				const result = await api.configureSession({
					sessionId,
					provider: current.provider,
					metadata,
				});
				patchSessionListMetadata(queryClient, projectId, sessionId, {}, ["archived"]);
				patchSelectedSnapshot(sessionId, (snapshot) => ({
					...snapshot,
					metadata: result.metadata ?? metadata,
					activity: result.activity,
				}));
				invalidateSessionList(projectId);
			}
			const content = textContent(text);
			const result = await api.queueFollowUp({
				sessionId,
				clientInputId,
				expectedActiveLeafId: snapshot.activity === "idle" ? (snapshot.active_leaf_id ?? null) : undefined,
				baseLeafId,
				content,
			});
			if (selectedRef.current !== sessionId) {
				invalidateSessionList(projectId);
				return;
			}
			if (result.queue) {
				updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue!));
			}
			if (result.queued) {
				invalidateSessionList(projectId);
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
				}
			}
			if (selectedRef.current === sessionId) {
				await refreshSelectedSessionState(sessionId);
			}
		},
		[
			api,
			assertServerMutationAllowed,
			commitSelectedSnapshot,
			invalidateSessionList,
			patchSelectedSnapshot,
			refreshSelectedSessionState,
			updateSelectedCache,
		],
	);

	const startNewSession = useCallback(
		async (text: string, clientInputId: string, sessionId: string) => {
			assertServerMutationAllowed();
			const projectId = selectedProjectRef.current;
			const title = nextSessionTitleRef.current || titleFromText(text);
			nextSessionTitleRef.current = null;
			const submittedWorkspaceScope = projectId
				? workspaceScopeRef.current.map((entry) => ({ ...entry }))
				: [];
			const workspaces = projectId
				? startWorkspacesFromScope(submittedWorkspaceScope)
				: undefined;
			const params = {
				sessionId,
				projectId,
				provider: newSessionProvider,
				metadata: {
					title,
					created_by: "web",
					compaction: {
						config: newSessionCompactionConfig(),
					},
				},
				clientInputId,
				priority: "follow_up" as const,
				content: textContent(text),
				workspaces,
				mcp: mcpSelectionPayloadForProvider(
					newSessionProvider.kind,
					mcpSelectionProviderRef.current,
					mcpInventoryProvider,
					mcpInventory,
					mcpInventoryReady,
					mcpSelectionRef.current,
					mcpAuthStatus,
					mcpAuthStatusReady,
				),
			};
			let result;
			try {
				setWorkspacePreparationProjectId(
					submittedWorkspaceScope.some((entry) => entry.included) ? projectId : null,
				);
				try {
					result = await api.startSession(params);
				} finally {
					setWorkspacePreparationProjectId(null);
				}
			} catch (error) {
				if (errorMessage(error).startsWith("mcp_inventory_changed:")) {
					await queryClient.refetchQueries({
						queryKey: queryKeys.mcpInventory(newSessionProvider.kind),
					});
				}
				throw error;
			}
			mcpSelectionRef.current = new Map();
			setMcpSelection(new Map());
			void queryClient.invalidateQueries({
				queryKey: queryKeys.sessions(projectId),
			});
			openRootConversation(projectId, result.session_id);
			return result.session_id;
		},
		[
			api,
			assertServerMutationAllowed,
			mcpInventory,
			mcpInventoryProvider,
			mcpInventoryReady,
			mcpAuthStatus,
			mcpAuthStatusReady,
			newSessionProvider,
			openRootConversation,
			queryClient,
		],
	);

	const switchToTarget = useCallback(
		async (
			sessionId: string,
			projectId: string | null,
			snapshot: SessionSnapshot,
			targetCache: SelectedSessionCache,
			target: HistoryTargetOption,
		) => {
			assertServerMutationAllowed();
			const expectedTranscriptRevision =
				targetCache.treeTranscriptRevision ?? snapshot.transcript_revision ?? null;
			if (targetCache.sessionId !== sessionId || targetCache.snapshot?.session_id !== sessionId) {
				throw new IntermediateUiStateError("session is still loading");
			}
			if (snapshot.activity !== "idle") {
				throw new Error("stop the active turn before switching history");
			}
			const targetBranchIds = branchFromTree(targetCache, target.actionLeafId).map((node) => node.id);
			if (target.actionLeafId && !targetBranchIds.includes(target.actionLeafId)) {
				throw new Error("history index is still loading; please wait for the switch list to finish");
			}
			const restoreText = await restoreTextForTarget(
				api,
				sessionId,
				target,
				targetCache,
				selectedCacheRef,
				updateSelectedCache,
				assertServerReadAllowed,
			);
			let result;
			try {
				result = await api.switchHistory({
					sessionId,
					leafId: target.actionLeafId,
					expectedActiveLeafId: target.expectedActiveLeafId ?? snapshot.active_leaf_id ?? null,
					expectedTranscriptRevision,
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
			if (selectedRef.current !== sessionId) {
				invalidateSessionList(projectId);
				return;
			}
			const destinationChanged =
				result.active_leaf_id !== (snapshot.active_leaf_id ?? null);
			const turnPageHydrationRevisionBeforeSwitch =
				selectedCacheRef.current.turnPageHydrationRevision;
			const hadUsableCacheBeforeSwitch =
				hasUsableSelectedSessionCache(selectedCacheRef.current, sessionId);
			if (restoreText !== null) composerHandleRef.current?.setValue(restoreText);
			updateSelectedCache((current) =>
				current.sessionId === sessionId
					? applySwitchResultToCache(current, result)
					: current,
			);
			if (destinationChanged) {
				setTranscriptDestination({
					id: ++nextTranscriptDestinationIdRef.current,
					sessionId,
					targetLeafId: result.active_leaf_id,
					minimumTurnPageHydrationRevision:
						turnPageHydrationRevisionBeforeSwitch + 1,
				});
				// Fence an old usable-cache refresh and start a new canonical
				// request without replacing the still-valid rendered page.
				selectedFetchCoordinator.restart(hadUsableCacheBeforeSwitch);
			}
			await refreshSelectedSessionState(sessionId, {
				preserveRenderedCache: destinationChanged && hadUsableCacheBeforeSwitch,
			});
			if (result.last_event_id !== undefined) lastEventIds.current.set(sessionId, result.last_event_id);
			invalidateSessionList(projectId);
		},
		[
			api,
			assertServerMutationAllowed,
			assertServerReadAllowed,
			ensureTreeIndex,
			invalidateSessionList,
			refreshSelectedSessionState,
			selectedFetchCoordinator,
			updateSelectedCache,
		],
	);

	const handleSwitchHistoryTarget = useCallback(
		(target: HistoryTargetOption) => {
			const dialog = historyDialog;
			if (!dialog) return;
			try {
				assertServerMutationAllowed();
			} catch (error) {
				pushErrorNotice(errorMessage(error));
				return;
			}
			const sessionId = dialog.sessionId;
			const targetCache = getSelectedCache(sessionId);
			const snapshot = targetCache?.snapshot;
			if (!targetCache || !snapshot || snapshot.session_id !== sessionId) {
				setHistoryDialog(null);
				return;
			}
			const projectId = snapshot.project_id;
			setHistoryDialog(null);
			void switchToTarget(sessionId, projectId, snapshot, targetCache, target)
				.catch((error) => {
					reportActionError(error);
				});
		},
		[assertServerMutationAllowed, getSelectedCache, historyDialog, pushErrorNotice, reportActionError, switchToTarget],
	);

	const promoteQueuedInput = useCallback(
		async (inputId: string) => {
			assertServerMutationAllowed();
			const sessionId = requireSelected();
			const projectId = selectedProjectRef.current;
			const result = await api.promoteQueuedInput(sessionId, inputId);
			if (result.queue) {
				updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue!));
			}
			await queryClient.invalidateQueries({ queryKey: queryKeys.sessions(projectId) });
		},
		[api, assertServerMutationAllowed, queryClient, requireSelected, updateSelectedCache],
	);

	const updateQueuedInput = useCallback(
		async (inputId: string, text: string) => {
			assertServerMutationAllowed();
			const sessionId = requireSelected();
			const projectId = selectedProjectRef.current;
			const queueRevision = selectedCacheRef.current.sessionId === sessionId ? selectedCacheRef.current.snapshot?.queue_revision : undefined;
			const result = await api.updateQueuedInput(sessionId, inputId, textContent(text), queueRevision);
			updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue));
			invalidateSessionList(projectId);
		},
		[api, assertServerMutationAllowed, invalidateSessionList, requireSelected, updateSelectedCache],
	);

	const cancelQueuedInput = useCallback(
		async (inputId: string) => {
			assertServerMutationAllowed();
			const sessionId = requireSelected();
			const queueRevision = selectedCacheRef.current.sessionId === sessionId ? selectedCacheRef.current.snapshot?.queue_revision : undefined;
			const result = await api.cancelQueuedInput(sessionId, inputId, queueRevision);
			updateSelectedCache((current) => applyQueueProjection(current, sessionId, result.queue));
			invalidateSessionList();
		},
		[api, assertServerMutationAllowed, invalidateSessionList, requireSelected, updateSelectedCache],
	);

	const reorderQueuedInput = useCallback(
		async (inputId: string, direction: "up" | "down") => {
			assertServerMutationAllowed();
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
			invalidateSessionList();
		},
		[api, assertServerMutationAllowed, invalidateSessionList, loadedSnapshot?.queued_inputs, requireSelected, updateSelectedCache],
	);

	const stopActiveTurn = useCallback(async () => {
		try {
			assertServerMutationAllowed();
			const sessionId = requireSelected();
			const projectId = selectedProjectRef.current;
			setStopping(true);
			await stopSession(sessionId, {
				interrupt: (targetSessionId) => api.interrupt(targetSessionId),
				refresh: (targetSessionId) => syncActiveBranchNow(targetSessionId),
				invalidateSessions: () =>
					queryClient.invalidateQueries({
						queryKey: queryKeys.sessions(projectId),
					}),
			});
		} catch (error) {
			reportActionError(error);
		} finally {
			setStopping(false);
		}
	}, [api, assertServerMutationAllowed, queryClient, reportActionError, requireSelected, syncActiveBranchNow]);

	const invalidateDelegations = useCallback(
		(parentSessionId: string) =>
			queryClient.invalidateQueries({ queryKey: delegationQueryPrefix(parentSessionId) }),
		[queryClient],
	);

	const cancelDelegation = useCallback(
		async (parentSessionId: string, delegationId: string) => {
			assertServerMutationAllowed();
			await api.cancelDelegation(parentSessionId, delegationId);
			await invalidateDelegations(parentSessionId);
		},
		[api, assertServerMutationAllowed, invalidateDelegations],
	);

	const resumeTerminalTurn = useCallback(
		async (leafId?: string | null) => {
			assertServerMutationAllowed();
			const sessionId = requireSelected();
			const projectId = selectedProjectRef.current;
			const current = await refreshSelected(sessionId);
			const activeLeafId = leafId ?? current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null;
			if (!activeLeafId) throw new Error("no terminal turn to resume");
			if ((current?.snapshot.activity ?? loadedSnapshot?.activity) !== "idle") {
				throw new Error("stop the active turn before retrying");
			}
			setResumingTurnId(activeLeafId);
			try {
				await api.resumeTurn({
					sessionId,
					leafId: activeLeafId,
					expectedActiveLeafId: current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null,
				});
				await Promise.all([
					syncActiveBranchNow(sessionId),
					queryClient.invalidateQueries({ queryKey: queryKeys.sessions(projectId) }),
				]);
			} finally {
				setResumingTurnId(null);
			}
		},
		[api, assertServerMutationAllowed, loadedSnapshot?.active_leaf_id, loadedSnapshot?.activity, queryClient, refreshSelected, requireSelected, syncActiveBranchNow],
	);

	const openHistoryDialog = useCallback(
		(snapshot: SessionSnapshot) => {
			if (snapshot.activity !== "idle") {
				throw new Error("stop the active turn before switching history");
			}
			const sessionId = snapshot.session_id;
			const cache = selectedCacheRef.current;
			const treeRevisionMatches =
				snapshot.transcript_revision === undefined ||
				cache.treeTranscriptRevision === null ||
				cache.treeTranscriptRevision === snapshot.transcript_revision;
			const cachedNodes = cache.sessionId === sessionId && treeRevisionMatches ? treeNodesInOrder(cache) : [];
			const treeComplete = cache.sessionId === sessionId && treeRevisionMatches && cache.treeComplete;
			setHistoryDialog({
				sessionId,
				nodes: treeComplete || connectionRef.current !== "open" ? cachedNodes : [],
				activeLeafId: snapshot.active_leaf_id,
				loading: !treeComplete && connectionRef.current === "open",
				error: null,
			});
			if (connectionRef.current !== "open") return;
			void ensureTreeIndex(sessionId, {
				onPage: (nodes, complete) => {
					setHistoryDialog((current) => {
						if (!current || current.sessionId !== sessionId) return current;
						return {
							...current,
							nodes: complete ? nodes : [],
							activeLeafId: selectedCacheRef.current.treeActiveLeafId ?? snapshot.active_leaf_id,
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
		[ensureTreeIndex],
	);

	const executeSlash = useCallback(
		async (
			parsed: ParsedSlash,
			submittedSessionId: string | null,
			submittedSnapshot: SessionSnapshot | null,
		) => {
			const name = parsed.name;
			const args = parsed.args;
			if (!name || name === "help") {
				composerHandleRef.current?.setValue("/");
				requestAnimationFrame(() => composerHandleRef.current?.focus());
				return;
			}
			if (!findCommand(name)) {
				throw new Error(`unknown command: /${name}`);
			}
			if (name === "system") {
				if (args) {
					throw new Error("/system is read-only; edit PI.md in the repo to change the prompt");
				}
				if (!submittedSessionId || submittedSnapshot?.session_id !== submittedSessionId) {
					throw new Error("/system requires a selected session");
				}
				assertServerReadAllowed();
				setPromptDialog({ loading: true, template: "", rendered: null, view: "rendered", error: null });
				try {
					const next = await queryClient.fetchQuery({
						queryKey: queryKeys.systemPrompt(submittedSessionId),
						queryFn: () => {
							assertServerReadAllowed();
							return api.getSystemPrompt(submittedSessionId);
						},
						staleTime: 0,
					});
					setPromptDialog((current) => current ? {
						loading: false,
						template: next.template,
						rendered: next.rendered,
						view: next.rendered ? "rendered" : "template",
						error: null,
					} : current);
				} catch (error) {
					setPromptDialog((current) => current ? {
						loading: false,
						template: "",
						rendered: null,
						view: "template",
						error: errorMessage(error),
					} : current);
				}
				return;
			}

			if (!submittedSessionId || submittedSnapshot?.session_id !== submittedSessionId) {
				throw new IntermediateUiStateError("session is still loading");
			}
			const sessionId = submittedSessionId;
			if (name === "switch") {
				if (
					connectionRef.current !== "open" &&
					!hasCanonicalCachedHistory(selectedCacheRef.current, sessionId)
				) {
					throw new Error("load session history before inspecting it offline");
				}
				openHistoryDialog(submittedSnapshot);
				return;
			}
			if (name === "export") {
				const cachedEntries =
					selectedCacheRef.current.sessionId === sessionId
						? activeBranchEntriesForExport(selectedCacheRef.current)
						: [];
				const cachedBlocks =
					selectedCacheRef.current.sessionId === sessionId
						? buildCachedExportBlocks(selectedCacheRef.current)
						: [];
				const current = remoteActionBlockedReason(connectionRef.current)
					? null
					: await (async () => {
							assertServerReadAllowed();
							return api.getSession(sessionId, { includeEntries: true, entryScope: SELECTED_SESSION_DISPLAY_SCOPE });
						})();
				if (current && selectedRef.current === sessionId) commitSelectedSnapshot(current);
				setExportDialog({
					entries: current ? (current.entries ?? []) : cachedEntries,
					blocks: current ? undefined : cachedBlocks,
				});
				return;
			}
			if (name === "compact") {
				assertServerMutationAllowed();
				await api.requestCompaction(sessionId);
				return;
			}
			throw new Error(`unknown command: /${name}`);
		},
		[api, assertServerMutationAllowed, assertServerReadAllowed, commitSelectedSnapshot, openHistoryDialog, queryClient],
	);

	const submitComposer = useCallback(
		async (submission: ComposerSubmission) => {
			if (!submission.text.trim() || sending) return false;
			if (
				connectionRemoteActionBlockedReason &&
				composerTextNeedsConnection(submission.text, { cachedHistoryAvailable })
			) {
				return false;
			}
			setSending(true);
			try {
				return await routeComposerSubmission(submission, {
					getLoadedSnapshot: (sessionId) => {
						return getSelectedCache(sessionId)?.snapshot ?? null;
					},
					executeSlash,
					queueFollowUp: queueUserInput,
					steerSubagent: (params) => {
						assertServerMutationAllowed();
						return api.steerSubagent(params);
					},
					startNewSession,
					reportError: (error) => {
						if (shouldReportActionError(error)) pushErrorNotice(errorMessage(error));
					},
				});
			} finally {
				setSending(false);
			}
		},
		[api, assertServerMutationAllowed, cachedHistoryAvailable, connectionRemoteActionBlockedReason, executeSlash, getSelectedCache, pushErrorNotice, queueUserInput, sending, startNewSession],
	);

	const canStop = !!selectedId && loadedSnapshot?.activity === "running";
	const queuedInputs = (loadedSnapshot?.queued_inputs ?? []).filter(
		(input) => input.content_type !== "daemon_tool_observation" && input.editable !== false,
	);
	const handleToggleArchived = useCallback(() => {
		setShowArchived((show) => !show);
	}, []);
	const handleSelectProject = useCallback(
		(projectId: string | null) => {
			if (projectId === selectedProjectRef.current) return;
			selectProjectSession(projectId, null);
			setQuery("");
			composerHandleRef.current?.setValue("");
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
		assertServerMutationAllowed();
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
			closeProjectDialog();
		} catch (error) {
			setProjectDialog((current) => (current ? { ...current, saving: false } : current));
			throw error;
		}
	}, [api, assertServerMutationAllowed, closeProjectDialog, projectDialog, queryClient, selectProjectSession]);
	const handleSidebarNew = useCallback(() => {
		void createSession();
		if (panelModeRef.current !== "wide") setSidebarOpen(false);
	}, [createSession]);
	const handleArchiveToggle = useCallback(
		(session: SessionListItem) => {
			void setSessionArchived(session, !isArchivedSession(session)).catch((error) => pushErrorNotice(errorMessage(error)));
		},
		[pushErrorNotice, setSessionArchived],
	);
	const handleSidebarDelete = useCallback((session: SessionListItem) => {
		setDeleteDialog({ session, deleting: false });
	}, []);
	const handleModelChange = useCallback(
		(value: string) => {
			try {
				changeModel(value);
			} catch (error) {
				pushErrorNotice(errorMessage(error));
			}
		},
		[changeModel, pushErrorNotice],
	);
	const handleReasoningEffortChange = useCallback(
		(value: ReasoningEffort) => {
			try {
				changeReasoningEffort(value);
			} catch (error) {
				pushErrorNotice(errorMessage(error));
			}
		},
		[changeReasoningEffort, pushErrorNotice],
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
			if (nextMode !== "wide") {
				if (sidebarResizeRef.current) {
					setSidebarWidth(sidebarWidthRef.current);
					saveSidebarWidth(sidebarWidthRef.current);
				}
				sidebarResizeRef.current = null;
				setSidebarResizing(false);
			}
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
	const applySidebarWidth = useCallback((width: number, persist = false) => {
		const nextWidth = clampSidebarWidth(width);
		sidebarWidthRef.current = nextWidth;
		appShellRef.current?.style.setProperty("--sidebar-width", `${nextWidth}px`);
		if (persist) {
			setSidebarWidth(nextWidth);
			saveSidebarWidth(nextWidth);
		}
	}, []);
	const handleSidebarResizePointerDown = useCallback(
		(event: ReactPointerEvent<HTMLDivElement>) => {
			if (event.button !== 0 || panelModeRef.current !== "wide") return;
			event.preventDefault();
			sidebarResizeRef.current = {
				pointerId: event.pointerId,
				startX: event.clientX,
				startWidth: sidebarWidthRef.current,
			};
			event.currentTarget.setPointerCapture(event.pointerId);
			setSidebarResizing(true);
		},
		[],
	);
	const handleSidebarResizePointerMove = useCallback(
		(event: ReactPointerEvent<HTMLDivElement>) => {
			const resize = sidebarResizeRef.current;
			if (!resize || resize.pointerId !== event.pointerId) return;
			applySidebarWidth(resize.startWidth + event.clientX - resize.startX);
		},
		[applySidebarWidth],
	);
	const handleSidebarResizePointerEnd = useCallback(
		(event: ReactPointerEvent<HTMLDivElement>) => {
			const resize = sidebarResizeRef.current;
			if (!resize || resize.pointerId !== event.pointerId) return;
			sidebarResizeRef.current = null;
			setSidebarResizing(false);
			applySidebarWidth(sidebarWidthRef.current, true);
			if (event.currentTarget.hasPointerCapture(event.pointerId)) {
				event.currentTarget.releasePointerCapture(event.pointerId);
			}
		},
		[applySidebarWidth],
	);
	const handleSidebarResizeKeyDown = useCallback(
		(event: ReactKeyboardEvent<HTMLDivElement>) => {
			let nextWidth: number;
			switch (event.key) {
				case "ArrowLeft":
					nextWidth = sidebarWidthRef.current - SIDEBAR_KEYBOARD_STEP;
					break;
				case "ArrowRight":
					nextWidth = sidebarWidthRef.current + SIDEBAR_KEYBOARD_STEP;
					break;
				case "Home":
					nextWidth = MIN_SIDEBAR_WIDTH;
					break;
				case "End":
					nextWidth = MAX_SIDEBAR_WIDTH;
					break;
				default:
					return;
			}
			event.preventDefault();
			applySidebarWidth(nextWidth, true);
		},
		[applySidebarWidth],
	);
	const resetSidebarWidth = useCallback(() => {
		applySidebarWidth(DEFAULT_SIDEBAR_WIDTH, true);
	}, [applySidebarWidth]);
	const handleResumeTurn = useCallback(
		(entryId: string) => {
			void resumeTerminalTurn(entryId).catch(reportActionError);
		},
		[reportActionError, resumeTerminalTurn],
	);
	const handleStop = useCallback(() => {
		void stopActiveTurn();
	}, [stopActiveTurn]);
	const handlePromoteQueued = useCallback(
		(inputId: string) => {
			void promoteQueuedInput(inputId).catch((error) => pushErrorNotice(errorMessage(error)));
		},
		[promoteQueuedInput, pushErrorNotice],
	);
	const handleUpdateQueued = useCallback(
		(inputId: string, text: string) => {
			void updateQueuedInput(inputId, text).catch((error) => pushErrorNotice(errorMessage(error)));
		},
		[pushErrorNotice, updateQueuedInput],
	);
	const handleCancelQueued = useCallback(
		(inputId: string) => {
			void cancelQueuedInput(inputId).catch((error) => pushErrorNotice(errorMessage(error)));
		},
		[cancelQueuedInput, pushErrorNotice],
	);
	const handleMoveQueued = useCallback(
		(inputId: string, direction: "up" | "down") => {
			void reorderQueuedInput(inputId, direction).catch((error) => pushErrorNotice(errorMessage(error)));
		},
		[pushErrorNotice, reorderQueuedInput],
	);
	const validatedRoute =
		workspaceRouteResult.kind === "route" && routeRemoteReadsEnabled
			? workspaceRouteResult.route
			: null;
	const conversationVisible =
		(validatedRoute?.destination === "conversation") ||
		(workspaceRouteResult.kind === "none" && routeValidation.kind === "idle");
	const executionRoute =
		validatedRoute?.destination === "execution" ? validatedRoute : null;
	const unavailableState =
		routeValidation.kind === "unavailable"
			? routeValidation.state
			: workspaceRouteResult.kind === "unavailable"
				? workspaceRouteResult
				: null;
	const routePending =
		routeValidation.kind === "pending" &&
		(workspaceRouteResult.kind === "route" || legacyMigrationPendingRef.current);
	const retryRouteValidation = useCallback(() => {
		setRouteValidationRetry((current) => current + 1);
	}, []);
	const openRouteRecovery = useCallback(
		(url: string) => {
			const parsed = parseWorkspaceRoute(url);
			if (parsed.kind !== "route") return;
			applyNavigation({
				kind: "route",
				history: "push",
				route: parsed.route,
				url: parsed.canonicalUrl,
			});
		},
		[applyNavigation],
	);
	const persistentRouteWarnings =
		workspaceRouteResult.kind === "route"
			? workspaceRouteResult.warnings.filter((warning) => warning.persistent)
			: [];
	const preparingWorkspaces =
		workspacePreparationProjectId !== null &&
		workspacePreparationProjectId === selectedProjectId;
	const mobileTitle = selectedChatSession
		? sessionTitle(selectedChatSession)
		: selectedProject
			? projectTitle(selectedProject)
			: "pi relay";
	const mobileActivity = selectedChatSession
		? sessionStatusWithDelegations(loadedSnapshot?.activity ?? selectedChatSession.activity, hasRunningDelegations)
		: null;
	const mobileArchived = selectedChatSession ? isArchivedSession(selectedChatSession) : false;
	const mobileSessionStatus = selectedChatSession ? (mobileArchived ? "archived" : mobileActivity) : null;
	const mobileSessionStatusLabel = mobileSessionStatus ? `${mobileSessionStatus} session` : null;
	const mobileParentSessionId = loadedSnapshot?.parent_session_id ?? null;
	const sidebarIsOverlay = panelMode !== "wide";
	const inspectorIsOverlay = panelMode === "compact";
	const sidebarOverlayOpen = sidebarIsOverlay && sidebarOpen;
	const inspectorOverlayOpen = inspectorIsOverlay && rightOpen;
	const sidebarInert = sidebarIsOverlay && !sidebarOpen;
	const inspectorInert = inspectorIsOverlay && !rightOpen;
	// Overlay launches close/inert the sidebar, so they return to its visible
	// topbar toggle. Static-sidebar launches retain their opener when possible,
	// with New session as the stable fallback if a deleted row disappears.
	const sidebarDialogReturnFocusRef = sidebarIsOverlay
		? mobileSidebarToggleRef
		: sidebarNewSessionButtonRef;
	const composerDialogReturnFocusRef = useMemo<RefObject<HTMLElement | null>>(
		() => ({
			get current() {
				return composerHandleRef.current?.focusTarget() ?? null;
			},
		}),
		[],
	);
	const appClassName = `app-shell ${sidebarOpen ? "sidebar-open" : ""} ${rightOpen ? "inspector-open" : ""} ${sidebarResizing ? "sidebar-resizing" : ""}`;
	const appStyle = { "--sidebar-width": `${sidebarWidth}px` } as CSSProperties;

	return (
		<div ref={appShellRef} className={appClassName} style={appStyle}>
			<div className="mobile-topbar">
				<button
					ref={mobileSidebarToggleRef}
					className="icon-button"
					type="button"
					onClick={handleToggleSidebar}
					aria-label={sidebarOpen ? "close projects and sessions" : "open projects and sessions"}
					aria-expanded={sidebarOpen}
				>
					<Menu size={17} />
				</button>
				<div className="mobile-topbar-title">
					<div className="mobile-topbar-title-main">
						{mobileSessionStatus ? (
							<span
								className={`session-status-icon ${mobileSessionStatus}`}
								role="img"
								aria-label={mobileSessionStatusLabel ?? undefined}
								title={mobileSessionStatusLabel ?? undefined}
							>
								<Bot size={12} aria-hidden />
							</span>
						) : null}
						<span className="mobile-topbar-title-text">{mobileTitle}</span>
						{mobileParentSessionId ? (
							<button
								className="parent-session-link"
								type="button"
								onClick={() => selectSession(mobileParentSessionId)}
								title="Open parent conversation"
								aria-label="Open parent conversation"
							>
								<ArrowUp size={13} aria-hidden />
							</button>
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
				projects={projects}
				projectsLoading={projectsQuery.isLoading}
				projectsFetching={projectsQuery.isFetching}
				projectsError={projectsError}
				projectsHasCachedData={projectsQuery.data !== undefined}
				selectedProjectId={selectedProjectId}
				query={query}
				showArchived={showArchived}
				filteredSessions={filteredSessions}
				selectedId={selectedId}
				sessionsLoading={sessionsQuery.isLoading}
				sessionsFetching={sessionListRequestState.busy}
				inert={sidebarInert}
				newSessionButtonRef={sidebarNewSessionButtonRef}
				onRetryProjects={retryProjects}
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
				mutationBlockedReason={connectionRemoteActionBlockedReason}
				remoteReadBlockedReason={connectionRemoteActionBlockedReason}
			/>
			{panelMode === "wide" ? (
				<div
					className="sidebar-resize-handle"
					role="separator"
					aria-label="Resize sidebar"
					aria-orientation="vertical"
					aria-valuemin={MIN_SIDEBAR_WIDTH}
					aria-valuemax={MAX_SIDEBAR_WIDTH}
					aria-valuenow={sidebarWidth}
					aria-valuetext={`${sidebarWidth} pixels`}
					tabIndex={0}
					title="Drag to resize sidebar. Double-click to reset."
					onDoubleClick={resetSidebarWidth}
					onKeyDown={handleSidebarResizeKeyDown}
					onPointerDown={handleSidebarResizePointerDown}
					onPointerMove={handleSidebarResizePointerMove}
					onPointerUp={handleSidebarResizePointerEnd}
					onPointerCancel={handleSidebarResizePointerEnd}
					onLostPointerCapture={handleSidebarResizePointerEnd}
				/>
			) : null}

			{conversationVisible ? (
				<ChatPane
						session={selectedChatSession}
						snapshot={loadedSnapshot}
						entries={loadedEntries}
						turnCards={turnCardViews}
						transcriptLoading={transcriptLoading}
						transcriptError={selectedError}
						transcriptErrorHasUsableCache={selectedErrorHasUsableCache}
						transcriptRetrying={selectedRetrying}
						hasRunningDelegations={hasRunningDelegations}
						modelOptions={MODEL_OPTIONS}
						modelValue={providerModelKey(activeProvider)}
						modelLocked={modelLocked}
						modelControlsDisabled={modelControlsDisabled}
						reasoningControlsDisabled={reasoningControlsDisabled}
						mutationBlockedReason={selectedId ? connectionRemoteActionBlockedReason : null}
						remoteReadBlockedReason={selectedId ? connectionRemoteActionBlockedReason : null}
						reasoningEfforts={reasoningEfforts}
						reasoningEffort={providerReasoningEffort(activeProvider)}
						rightOpen={rightOpen}
						selectedId={selectedId}
						resumingTurnId={resumingTurnId}
						onModelChange={handleModelChange}
						onReasoningEffortChange={handleReasoningEffortChange}
						onSelectSession={openConversation}
						onToggleRight={handleToggleRight}
						onNewSession={handleSidebarNew}
						onResumeTurn={handleResumeTurn}
						onExpandTurn={expandTurn}
						onCollapseTurn={collapseTurn}
						loadingTurnId={loadingTurnId ?? autoLoadingTurnId}
						hasOlderTurns={selectedCache.sessionId === selectedId && selectedCache.turnHasMoreBefore}
						loadingOlderTurns={loadingOlderTurns}
						onLoadOlderTurns={loadOlderTranscriptTurns}
						transcriptDestination={transcriptDestination}
						transcriptTurnPageIdentity={transcriptTurnPageIdentity}
						onAcknowledgeTranscriptDestination={acknowledgeTranscriptDestination}
						onRetryTranscript={retrySelected}
						emptySessionContent={
							!selectedId ? (
								<NewSessionSetup
									workspaceConfiguration={workspaceConfiguration}
									onWorkspaceScopeChange={handleWorkspaceScopeChange}
									mcpInventory={mcpInventory}
									mcpSelection={mcpSelection}
									onMcpSelectionChange={handleMcpSelectionChange}
									mcpLoading={mcpInventoryQuery.isFetching || mcpStatusQuery.isFetching}
									mcpReady={mcpInventoryReady}
									mcpError={
										mcpInventoryQuery.error || mcpStatusQuery.error
											? errorMessage(mcpInventoryQuery.error ?? mcpStatusQuery.error)
											: null
									}
									onRetryMcp={retryMcpInventory}
									mcpAuthStatus={mcpAuthStatus}
									mcpAuthStatusReady={mcpAuthStatusReady}
									onMcpLogin={(server) => void loginMcp(server)}
									onMcpLogout={cancelOrLogoutMcp}
									mcpAuthBusyServer={mcpAuthBusyServer}
									disabled={sending}
									preparingWorkspaces={preparingWorkspaces}
									mcpAuthMutationBlockedReason={connectionRemoteActionBlockedReason}
								/>
							) : null
						}
						routeNotice={
							persistentRouteWarnings.length > 0 ? (
								<div className="workspace-route-warning" role="alert">
									{persistentRouteWarnings.map((warning) => (
										<span key={`${warning.kind}:${warning.message}`}>{warning.message}</span>
									))}
								</div>
							) : null
						}
					/>
			) : executionRoute ? (
				<main className="workspace-route-state execution-route-state" data-slot="execution-placeholder">
					<p className="workspace-route-eyebrow">Execution · {executionRoute.view}</p>
					{persistentRouteWarnings.length > 0 ? (
						<div className="workspace-route-warning" role="alert">
							{persistentRouteWarnings.map((warning) => (
								<span key={`${warning.kind}:${warning.message}`}>{warning.message}</span>
							))}
						</div>
					) : null}
					<h1>Execution workspace is not available in this step</h1>
					<p>
						This URL retains root <code>{executionRoute.rootSessionId}</code> and Conversation{" "}
						<code>{routeConversationSessionId(executionRoute)}</code>. The visual Execution
						overview, activity, and handoffs workspace comes next.
					</p>
					<button
						type="button"
						className="primary-button workspace-route-action"
						onClick={() => applyNavigation(showConversation(executionRoute))}
					>
						Open effective Conversation
					</button>
				</main>
			) : unavailableState ? (
				<main className="workspace-route-state unavailable-route-state" data-slot="route-unavailable">
					<p className="workspace-route-eyebrow">Workspace unavailable</p>
					<h1>
						{unavailableState.issue === "invalid-conversation"
							? "Couldn’t load session"
							: "Couldn’t open this workspace"}
					</h1>
					<p role="alert">{unavailableState.message}</p>
					<div className="workspace-route-actions">
						{unavailableState.backTo ? (
							<button
								type="button"
								className="primary-button workspace-route-action"
								onClick={() => openRouteRecovery(unavailableState.backTo!.url)}
							>
								{unavailableState.backTo.label === "root-conversation"
									? "Open root Conversation"
									: "Back to root Outline"}
							</button>
						) : null}
						{routeValidation.kind === "unavailable" && routeValidation.retryable ? (
							<>
								<button
									type="button"
									className="secondary-button workspace-route-action"
									disabled={!!connectionRemoteActionBlockedReason}
									onClick={retryRouteValidation}
								>
									Retry
								</button>
								<span>{connectionRemoteActionBlockedReason}</span>
							</>
						) : null}
					</div>
				</main>
			) : routePending ? <LoadingConversation /> : null}

			<footer className="chat-dock" data-slot="chat-box">
				<ConnectionRecoveryBanner
					disconnected={disconnected}
					retrying={retryingConnection}
					onRetry={retryConnection}
				/>
				{conversationVisible ? (
					<Composer
						selectedId={selectedId}
						selectedIsSubagent={
							loadedSnapshot?.session_id === selectedId && !!loadedSnapshot.parent_session_id
						}
						composerHandleRef={composerHandleRef}
						sending={sending}
						canStop={canStop}
						stopping={stopping}
						queuedInputs={queuedInputs}
						mutationBlockedReason={connectionRemoteActionBlockedReason}
						cachedHistoryAvailable={cachedHistoryAvailable}
						newSessionSetupGeneration={newSessionSetupGeneration}
						onSubmit={submitComposer}
						onStop={handleStop}
						onPromoteQueued={handlePromoteQueued}
						onUpdateQueued={handleUpdateQueued}
						onCancelQueued={handleCancelQueued}
						onMoveQueued={handleMoveQueued}
					/>
				) : null}
			</footer>

			<aside className="inspector" data-slot="inspector" inert={inspectorInert}>
				{executionRoute || unavailableState ? (
					<div className="workspace-inspector-placeholder">
						<p>
							{executionRoute
								? "Execution details are intentionally deferred."
								: "Conversation details are unavailable."}
						</p>
					</div>
				) : (
				<Inspector
					snapshot={loadedSnapshot}
					runBoardParentSessionId={delegationParentSessionId}
					delegations={delegations}
					subagentNames={delegationSubagentNames}
					hasMoreDelegations={hasMoreDelegations}
					delegationsLoading={delegationsLoading}
					delegationsError={delegationsError}
					showAllDelegations={showAllDelegations}
					expandedDelegationsAvailable={expandedDelegationsAvailable}
					onToggleShowAllDelegations={() => setShowAllDelegations((current) => !current)}
					onRetryDelegations={retryDelegations}
					delegationsRetrying={delegationsRetrying}
					selectedSessionId={selectedId}
					boundedExpansionHasMore={
						showAllDelegations &&
						!!expandedDelegationsQuery.data?.has_more
					}
					onCancelDelegation={cancelDelegation}
					mutationBlockedReason={connectionRemoteActionBlockedReason}
					remoteReadBlockedReason={connectionRemoteActionBlockedReason}
					tools={tools}
					onSelectSession={(sessionId) => {
						openConversation(sessionId);
						if (inspectorIsOverlay) setRightOpen(false);
					}}
					onClose={() => setRightOpen(false)}
				/>
				)}
			</aside>

			{renameSessionId ? (
				<RenameSessionDialog
					value={renameValue}
					onChange={setRenameValue}
					returnFocusFallbackRef={sidebarDialogReturnFocusRef}
					onClose={closeRenameDialog}
					onSubmit={() => {
						return renameSession().catch((error) => {
							pushErrorNotice(errorMessage(error));
							throw error;
						});
					}}
					mutationBlockedReason={connectionRemoteActionBlockedReason}
				/>
			) : null}

			{deleteDialog ? (
				<DeleteSessionDialog
					session={deleteDialog.session}
					deleting={deleteDialog.deleting}
					returnFocusFallbackRef={sidebarDialogReturnFocusRef}
					onClose={closeDeleteDialog}
					onConfirm={() => {
						return deleteSession().catch((error) => {
							pushErrorNotice(errorMessage(error));
							throw error;
						});
					}}
					mutationBlockedReason={connectionRemoteActionBlockedReason}
				/>
			) : null}

			{projectDialog ? (
				<ProjectDialog
					state={projectDialog}
					onChange={(patch) => setProjectDialog((current) => (current ? { ...current, ...patch } : current))}
					returnFocusFallbackRef={sidebarDialogReturnFocusRef}
					onClose={closeProjectDialog}
					onSubmit={() => {
						return saveProjectDialog().catch((error) => {
							pushErrorNotice(errorMessage(error));
							throw error;
						});
					}}
					mutationBlockedReason={connectionRemoteActionBlockedReason}
				/>
			) : null}

			{promptDialog ? (
				<SystemPromptDialog
					state={promptDialog}
					onChangeView={(view) => setPromptDialog((current) => (current ? { ...current, view } : current))}
					onClose={() => setPromptDialog(null)}
					returnFocusFallbackRef={composerDialogReturnFocusRef}
				/>
			) : null}

			{mcpLoginDialog ? (
				<McpOAuthDialog
					server={mcpLoginDialog.server}
					login={mcpLoginDialog.login}
					onComplete={completeMcpLogin}
					onCancel={cancelMcpLogin}
					mutationBlockedReason={connectionRemoteActionBlockedReason}
					returnFocusFallbackRef={composerDialogReturnFocusRef}
				/>
			) : null}

			{historyDialog ? (
				<CompactHistoryPickerDialog
					nodes={historyDialog.nodes}
					activeLeafId={historyDialog.activeLeafId}
					loading={historyDialog.loading}
					error={historyDialog.error}
					onClose={() => setHistoryDialog(null)}
					onSwitch={handleSwitchHistoryTarget}
					mutationBlockedReason={connectionRemoteActionBlockedReason}
					returnFocusFallbackRef={composerDialogReturnFocusRef}
				/>
			) : null}
			{exportDialog ? (
				<ExportDialog
					entries={exportDialog.entries}
					blocks={exportDialog.blocks}
					onClose={() => setExportDialog(null)}
					onError={(error) => pushErrorNotice(errorMessage(error))}
					returnFocusFallbackRef={composerDialogReturnFocusRef}
				/>
			) : null}
			<NoticeStack notices={notices} rightOpen={rightOpen} onDismiss={dismissNotice} />
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

function compactionErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "compaction failed";
	const label = data.trigger === "auto" ? "auto-compaction error" : "compaction error";
	return `${label}: ${truncate(error, 420)}`;
}

function subagentLabel(data: Record<string, unknown>): string {
	return typeof data.role === "string" && data.role.trim() ? data.role.trim() : "Agent";
}

function subagentFailureNotice(data: Record<string, unknown>): string {
	const preview =
		typeof data.summary_preview === "string" && data.summary_preview.trim()
			? `: ${truncate(data.summary_preview.trim(), 180)}`
			: "";
	return `${subagentLabel(data)} crashed${preview}`;
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
	targetCache: SelectedSessionCache,
	selectedCacheRef: RefObject<SelectedSessionCache>,
	updateSelectedCache: (updater: (current: SelectedSessionCache) => SelectedSessionCache) => SelectedSessionCache,
	assertServerReadAllowed: () => void,
): Promise<string | null> {
	if (!target.restoreEntryId) return target.restoreText ?? null;
	const cached = targetCache.entriesById.get(target.restoreEntryId);
	if (cached?.item.type === "user_message") return contentBlocksToText(cached.item.content);
	assertServerReadAllowed();
	const result = await api.getTranscriptEntries(sessionId, [target.restoreEntryId]);
	if (selectedCacheRef.current.sessionId === sessionId) {
		updateSelectedCache((current) =>
			current.sessionId === sessionId
				? applyEntryBodies(current, sessionId, result.entries)
				: current,
		);
	}
	const entry = result.entries.find((candidate) => candidate.id === target.restoreEntryId);
	if (entry?.item.type === "user_message") return contentBlocksToText(entry.item.content);
	throw new Error("could not load the full user message for editing");
}
