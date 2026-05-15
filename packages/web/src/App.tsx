import { useCallback, useEffect, useMemo, useRef, useState, type Dispatch, type SetStateAction } from "react";
import { createAgentApi } from "./agentApi.ts";
import { ChatPane } from "./chatPane.tsx";
import { Composer, type ComposerHandle } from "./composer.tsx";
import { HistoryPickerDialog } from "./historyPicker.tsx";
import { branchEntriesFor, type HistoryTargetOption } from "./historyTargets.ts";
import { ExportDialog } from "./exportDialog.tsx";
import { randomId } from "./ids.ts";
import { Inspector, NoticeStack, Sidebar } from "./panels.tsx";
import { approximateJsonSize, perfEnabled, perfLog, perfNow } from "./perf.ts";
import type { ConnectionStatus } from "./rpc.ts";
import { COMMANDS, findCommand, parseSlash, type ParsedSlash } from "./slash.ts";
import { reduceSessionEvent, type SessionPatchOperation } from "./sessionEvents.ts";
import {
	DEFAULT_PROVIDER,
	MODEL_OPTIONS,
	providerFromModelKey,
	providerModelKey,
	providerReasoningEffort,
	reasoningEffortsForProvider,
	textContent,
	withReasoningEffort
} from "./sessionDefaults.ts";
import { projectTitle, sessionTitle, isArchivedSession, tallyActivities, type SessionListItem } from "./sessionList.ts";
import { firstLine, truncate } from "./text.ts";
import type {
	DaemonConfig,
	EventFrame,
	Notice,
	Project,
	ProviderConfig,
	ReasoningEffort,
	SessionSnapshot,
	SessionSummary,
	ToolListing,
	TranscriptEntry
} from "./types.ts";

const MAX_NOTICES = 24;
const NOTICE_TTL_MS = 4000;
const SESSION_LIST_REFRESH_DEBOUNCE_MS = 250;
const MAX_CACHED_SESSIONS = 20;
const MAX_CACHED_ENTRIES = 8_000;

type EntryScope = "active_branch" | "full_tree";
type CachedSnapshot = Omit<SessionSnapshot, "entries">;

type CachedSession = {
	snapshot: CachedSnapshot;
	entries: TranscriptEntry[];
	entryScope: EntryScope;
	cachedAt: number;
	lastEventId: number;
	stale: boolean;
};

type ExportDialogState = {
	entries: TranscriptEntry[];
};

type HistoryDialogState = {
	mode: "fork" | "switch";
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	initialForkTitle?: string;
};

type DeleteDialogState = {
	session: SessionListItem;
	deleting: boolean;
};

type ProjectDialogState = {
	mode: "create" | "edit";
	projectId?: string;
	name: string;
	startingCwd: string;
	saving: boolean;
};

export function App() {
	const api = useMemo(() => createAgentApi(), []);
	const [connection, setConnection] = useState<ConnectionStatus>("connecting");
	const [projects, setProjects] = useState<Project[]>([]);
	const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
	const [sessions, setSessions] = useState<SessionSummary[]>([]);
	const [selectedId, setSelectedId] = useState<string | null>(null);
	const selectedRef = useRef<string | null>(null);
	const [snapshot, setSnapshot] = useState<SessionSnapshot | null>(null);
	const [entries, setEntries] = useState<TranscriptEntry[]>([]);
	const [notices, setNotices] = useState<Notice[]>([]);
	const [config, setConfig] = useState<DaemonConfig>({ system_prompt: null });
	const [tools, setTools] = useState<ToolListing[]>([]);
	const [query, setQuery] = useState("");
	const [newSessionProvider, setNewSessionProvider] = useState<ProviderConfig>(DEFAULT_PROVIDER);
	const [sending, setSending] = useState(false);
	const [stopping, setStopping] = useState(false);
	const [resumingTurnId, setResumingTurnId] = useState<string | null>(null);
	const [rightOpen, setRightOpen] = useState(true);
	const [showArchived, setShowArchived] = useState(false);
	const [historyDialog, setHistoryDialog] = useState<HistoryDialogState | null>(null);
	const [exportDialog, setExportDialog] = useState<ExportDialogState | null>(null);
	const [renameSessionId, setRenameSessionId] = useState<string | null>(null);
	const [renameValue, setRenameValue] = useState("");
	const [deleteDialog, setDeleteDialog] = useState<DeleteDialogState | null>(null);
	const [projectDialog, setProjectDialog] = useState<ProjectDialogState | null>(null);

	const refreshTimer = useRef<number | null>(null);
	const sessionListRefreshTimer = useRef<number | null>(null);
	const composerHandleRef = useRef<ComposerHandle | null>(null);
	const nextSessionTitleRef = useRef<string | null>(null);
	const selectedProjectRef = useRef<string | null>(null);
	const sessionsRef = useRef(new Map<string, SessionSummary>());
	const lastEventIds = useRef(new Map<string, number>());
	const subscribedEventSessionIds = useRef(new Set<string>());
	const sessionCacheRef = useRef(new Map<string, CachedSession>());
	const selectedRequestGeneration = useRef(0);
	const sessionListGeneration = useRef(0);

	const writeCachedSession = useCallback((sessionId: string, nextSnapshot: SessionSnapshot, nextEntries: TranscriptEntry[], entryScope: EntryScope) => {
		const { entries: _ignored, ...snapshotWithoutEntries } = nextSnapshot;
		sessionCacheRef.current.set(sessionId, {
			snapshot: snapshotWithoutEntries,
			entries: nextEntries,
			entryScope,
			cachedAt: Date.now(),
			lastEventId: nextSnapshot.last_event_id,
			stale: false
		});
		trimSessionCache(sessionCacheRef.current);
	}, []);

	const patchCachedSession = useCallback((sessionId: string, patcher: (cached: CachedSession) => CachedSession) => {
		const cached = sessionCacheRef.current.get(sessionId);
		if (!cached) return;
		sessionCacheRef.current.set(sessionId, patcher(cached));
	}, []);

	const markCachedSessionStale = useCallback((sessionId: string) => {
		patchCachedSession(sessionId, (cached) => ({ ...cached, stale: true }));
	}, [patchCachedSession]);

	const deleteCachedSession = useCallback((sessionId: string) => {
		sessionCacheRef.current.delete(sessionId);
	}, []);

	const applySessionSummaries = useCallback(
		(nextSessions: SessionSummary[]) => {
			const nextSessionMap = new Map(nextSessions.map((session) => [session.session_id, session]));
			sessionsRef.current = nextSessionMap;
			setSessions(nextSessions);
			setSnapshot((current) => {
				if (!current) return current;
				const summary = nextSessionMap.get(current.session_id);
				return summary ? { ...current, ...snapshotPatchFromSummary(summary) } : current;
			});
			for (const summary of nextSessions) {
				patchCachedSession(summary.session_id, (cached) => ({
					...cached,
					snapshot: { ...cached.snapshot, ...snapshotPatchFromSummary(summary) }
				}));
			}
		},
		[patchCachedSession]
	);

	const clearSelectedSessionState = useCallback(() => {
		selectedRequestGeneration.current += 1;
		setSnapshot(null);
		setEntries([]);
	}, []);

	const patchSessionSummary = useCallback(
		(sessionId: string, patch: Partial<SessionSummary>) => {
			setSessions((current) => {
				const nextSessions = patchSessionSummaryList(current, sessionId, patch);
				if (nextSessions !== current) sessionsRef.current = new Map(nextSessions.map((session) => [session.session_id, session]));
				return nextSessions;
			});
			setSnapshot((current) => {
				if (!current || current.session_id !== sessionId) return current;
				return { ...current, ...snapshotPatchFromSummaryPatch(patch) };
			});
			patchCachedSession(sessionId, (cached) => ({
				...cached,
				snapshot: { ...cached.snapshot, ...snapshotPatchFromSummaryPatch(patch) }
			}));
		},
		[patchCachedSession]
	);

	const patchSessionMetadata = useCallback(
		(sessionId: string, patch: Record<string, unknown>, removeKeys: string[] = []) => {
			setSessions((current) => {
				const nextSessions = patchSessionMetadataInList(current, sessionId, patch, removeKeys);
				if (nextSessions !== current) sessionsRef.current = new Map(nextSessions.map((session) => [session.session_id, session]));
				return nextSessions;
			});
			setSnapshot((current) => {
				if (!current || current.session_id !== sessionId) return current;
				return { ...current, metadata: mergeMetadata(current.metadata, patch, removeKeys) };
			});
			patchCachedSession(sessionId, (cached) => ({
				...cached,
				snapshot: {
					...cached.snapshot,
					metadata: mergeMetadata(cached.snapshot.metadata, patch, removeKeys)
				}
			}));
		},
		[patchCachedSession]
	);

	const patchProvider = useCallback(
		(sessionId: string, provider: ProviderConfig) => patchSessionSummary(sessionId, { provider }),
		[patchSessionSummary]
	);

	const patchQueuedInput = useCallback(
		(event: EventFrame) => {
			applyQueuedInputEvent(event, setSnapshot);
			patchCachedSession(event.session_id, (cached) => ({
				...cached,
				snapshot: applyQueuedInputEventToSnapshot(event, cached.snapshot)
			}));
		},
		[patchCachedSession]
	);

	const selectSession = useCallback(
		(sessionId: string | null) => {
			const previousSessionId = selectedRef.current;
			if (sessionId === previousSessionId) {
				if (sessionId === null) nextSessionTitleRef.current = null;
				return;
			}
			if (sessionId === null) nextSessionTitleRef.current = null;
			selectedRequestGeneration.current += 1;
			selectedRef.current = sessionId;
			setSelectedId(sessionId);
			if (!sessionId) {
				setSnapshot(null);
				setEntries([]);
				return;
			}
			const cached = sessionCacheRef.current.get(sessionId);
			if (cached && !cached.stale) {
				setSnapshot(snapshotWithEntries(cached.snapshot));
				setEntries(cached.entries);
				return;
			}
			setSnapshot(null);
			setEntries([]);
		},
		[]
	);

	useEffect(() => {
		selectedRef.current = selectedId;
	}, [selectedId]);

	useEffect(() => {
		selectedProjectRef.current = selectedProjectId;
	}, [selectedProjectId]);

	useEffect(() => {
		sessionsRef.current = new Map(sessions.map((session) => [session.session_id, session]));
	}, [sessions]);

	const pushNotice = useCallback((tone: Notice["tone"], text: string) => {
		setNotices((current) => [
			...current.slice(Math.max(0, current.length - MAX_NOTICES + 1)),
			{ id: randomId("notice"), tone, text }
		]);
	}, []);

	useEffect(() => {
		if (notices.length === 0) return;
		const timer = window.setTimeout(() => {
			setNotices((current) => current.slice(1));
		}, NOTICE_TTL_MS);
		return () => window.clearTimeout(timer);
	}, [notices.length]);

	const loadSessions = useCallback(async (projectId = selectedProjectRef.current) => {
		const generation = ++sessionListGeneration.current;
		const expectedProjectId = projectId;
		if (!expectedProjectId) {
			applySessionSummaries([]);
			return [];
		}
		const nextSessions = await api.listSessions(100, expectedProjectId);
		if (generation !== sessionListGeneration.current || selectedProjectRef.current !== expectedProjectId) return nextSessions;
		applySessionSummaries(nextSessions);
		return nextSessions;
	}, [api, applySessionSummaries]);

	const scheduleSessionListRefresh = useCallback(
		(delayMs = SESSION_LIST_REFRESH_DEBOUNCE_MS) => {
			if (sessionListRefreshTimer.current !== null) return;
			sessionListRefreshTimer.current = window.setTimeout(() => {
				sessionListRefreshTimer.current = null;
				void loadSessions().catch(() => undefined);
			}, delayMs);
		},
		[loadSessions]
	);

	const loadProjects = useCallback(async () => {
		const nextProjects = await api.listProjects();
		setProjects(nextProjects);
		const currentProjectId = selectedProjectRef.current;
		const nextSelected =
			currentProjectId && nextProjects.some((project) => project.project_id === currentProjectId)
				? currentProjectId
				: nextProjects[0]?.project_id ?? null;
		if (nextSelected !== currentProjectId) {
			selectedProjectRef.current = nextSelected;
			setSelectedProjectId(nextSelected);
			selectSession(null);
			setQuery("");
			composerHandleRef.current?.setValue("");
		}
		return nextProjects;
	}, [api, selectSession]);

	const loadGlobal = useCallback(async () => {
		const nextConfig = await api.getConfig();
		setConfig(nextConfig);
	}, [api]);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current, options: { entryScope?: EntryScope; force?: boolean } = {}) => {
			if (!sessionId) return null;
			const generation = ++selectedRequestGeneration.current;
			const shouldLogPerf = perfEnabled();
			const startedAt = perfNow();
			if (shouldLogPerf) perfLog("session.get start", { sessionId, source: options.force ? "force" : "refresh" });
			const nextSnapshot = await api.getSession(sessionId, { includeEntries: true });
			const nextEntries = nextSnapshot.entries ?? [];
			if (shouldLogPerf) {
				const rpcMs = perfNow() - startedAt;
				perfLog("session.get end", {
					sessionId,
					entries: nextEntries.length,
					approxBytes: approximateJsonSize(nextSnapshot),
					rpcMs: Math.round(rpcMs),
					entryScope: options.entryScope ?? "full_tree"
				});
			}
			if (selectedRef.current !== sessionId || generation !== selectedRequestGeneration.current) return null;
			lastEventIds.current.set(sessionId, nextSnapshot.last_event_id);
			writeCachedSession(sessionId, nextSnapshot, nextEntries, options.entryScope ?? "full_tree");
			setSnapshot(nextSnapshot);
			setEntries(nextEntries);
			setSessions((current) => {
				const nextSessions = mergeSnapshotIntoSessionList(current, nextSnapshot);
				sessionsRef.current = new Map(nextSessions.map((session) => [session.session_id, session]));
				return nextSessions;
			});
			if (nextSnapshot.project_id !== selectedProjectRef.current) {
				selectedProjectRef.current = nextSnapshot.project_id;
				setSelectedProjectId(nextSnapshot.project_id);
			}
			return { snapshot: nextSnapshot, entries: nextEntries };
		},
		[api, writeCachedSession]
	);

	const scheduleSelectedRefresh = useCallback((sessionId = selectedRef.current, delayMs = 80) => {
		if (!sessionId || sessionId !== selectedRef.current) return;
		if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current);
		refreshTimer.current = window.setTimeout(() => {
			refreshTimer.current = null;
			void refreshSelected(sessionId).catch((error) => pushNotice("error", errorMessage(error)));
		}, delayMs);
	}, [pushNotice, refreshSelected]);

	const applySessionOperation = useCallback(
		(operation: SessionPatchOperation) => {
			if (operation.type === "metadata") {
				patchSessionMetadata(operation.sessionId, operation.patch, operation.remove);
				return;
			}
			if (operation.type === "provider") {
				patchProvider(operation.sessionId, operation.provider);
				return;
			}
			if (operation.type === "activity") {
				patchSessionSummary(operation.sessionId, { activity: operation.activity });
				return;
			}
			if (operation.type === "queued_inputs") {
				patchQueuedInput(operation.event);
				return;
			}
			if (operation.type === "mark_stale") {
				markCachedSessionStale(operation.sessionId);
				return;
			}
			if (operation.type === "refresh_selected") {
				if (operation.sessionId === selectedRef.current) scheduleSelectedRefresh(operation.sessionId);
				return;
			}
			scheduleSessionListRefresh();
		},
		[markCachedSessionStale, patchProvider, patchQueuedInput, patchSessionMetadata, patchSessionSummary, scheduleSelectedRefresh, scheduleSessionListRefresh]
	);

	const handleSessionEvent = useCallback(
		(event: EventFrame) => {
			const eventSession = sessionsRef.current.get(event.session_id);
			if (eventSession?.project_id && eventSession.project_id !== selectedProjectRef.current) return;
			lastEventIds.current.set(event.session_id, Math.max(lastEventIds.current.get(event.session_id) ?? 0, event.event_id));
			patchCachedSession(event.session_id, (cached) => ({
				...cached,
				lastEventId: Math.max(cached.lastEventId, event.event_id)
			}));

			for (const operation of reduceSessionEvent(event)) applySessionOperation(operation);

			if (event.session_id === selectedRef.current) {
				if (event.event === "model.error") pushNotice("error", modelErrorNotice(event.data));
				if (event.event === "turn.finished") {
					const outcome = typeof event.data.outcome === "string" ? event.data.outcome : null;
					if (outcome === "Interrupted") pushNotice("info", "turn interrupted");
					if (outcome === "Crashed") pushNotice("error", "turn crashed");
				}
			}
		},
		[applySessionOperation, patchCachedSession, pushNotice]
	);

	useEffect(() => {
		const offStatus = api.onStatus((status) => {
			setConnection(status);
			if (status !== "open") {
				subscribedEventSessionIds.current.clear();
				for (const sessionId of sessionCacheRef.current.keys()) markCachedSessionStale(sessionId);
				return;
			}
			void loadProjects()
				.then(() => Promise.all([loadSessions(), loadGlobal()]))
				.then(() => {
					const sessionId = selectedRef.current;
					if (!sessionId) return undefined;
					return refreshSelected(sessionId);
				})
				.catch((error) => pushNotice("error", errorMessage(error)));
		});
		const offEvent = api.onEvent(handleSessionEvent);
		void api
			.connect()
			.catch((error) => pushNotice("error", errorMessage(error)));
		return () => {
			offStatus();
			offEvent();
			if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current);
			if (sessionListRefreshTimer.current !== null) window.clearTimeout(sessionListRefreshTimer.current);
			api.close();
		};
	}, [api, handleSessionEvent, loadGlobal, loadProjects, loadSessions, markCachedSessionStale, pushNotice, refreshSelected]);

	useEffect(() => {
		void loadSessions().catch((error) => pushNotice("error", errorMessage(error)));
	}, [loadSessions, pushNotice, selectedProjectId]);

	useEffect(() => {
		if (!selectedId) {
			clearSelectedSessionState();
			return;
		}
		const cached = sessionCacheRef.current.get(selectedId);
		if (cached && !cached.stale) {
			setSnapshot(snapshotWithEntries(cached.snapshot));
			setEntries(cached.entries);
			return;
		}
		setSnapshot(null);
		setEntries([]);
		void refreshSelected(selectedId).catch((error) => pushNotice("error", errorMessage(error)));
	}, [clearSelectedSessionState, pushNotice, refreshSelected, selectedId]);

	useEffect(() => {
		if (connection !== "open") return;
		const desiredSessionIds = new Set(sessions.map((session) => session.session_id));
		if (selectedId) desiredSessionIds.add(selectedId);
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
			void api
				.subscribeEvents(sessionId, lastEventIds.current.get(sessionId) ?? null)
				.then((replayed) => {
					if (!subscribedEventSessionIds.current.has(sessionId)) return undefined;
					for (const event of replayed) handleSessionEvent(event);
					if (selectedRef.current === sessionId) return refreshSelected(sessionId);
					return undefined;
				})
				.catch((error) => {
					subscribedEventSessionIds.current.delete(sessionId);
					pushNotice("error", errorMessage(error));
				});
		}
	}, [api, connection, handleSessionEvent, pushNotice, refreshSelected, selectedId, sessions]);

	const sessionItems: SessionListItem[] = sessions;
	const selectedProject = useMemo(
		() => projects.find((project) => project.project_id === selectedProjectId) ?? null,
		[projects, selectedProjectId]
	);

	const selectedSession = useMemo(
		() => sessionItems.find((session) => session.session_id === selectedId) ?? null,
		[sessionItems, selectedId]
	);

	useEffect(() => {
		if (!selectedId) return;
		if (sessionItems.some((session) => session.session_id === selectedId)) return;
		selectSession(null);
	}, [selectSession, selectedId, sessionItems]);

	const loadedSnapshot = snapshot?.session_id === selectedId ? snapshot : null;
	const loadedEntries = loadedSnapshot ? entries : [];
	const transcriptLoading = !!selectedId && !loadedSnapshot;

	const snapshotChatSession = useMemo(() => {
		if (!selectedId || !loadedSnapshot) return null;
		return {
			session_id: selectedId,
			project_id: loadedSnapshot.project_id,
			activity: loadedSnapshot.activity,
			active_leaf_id: loadedSnapshot.active_leaf_id,
			provider: loadedSnapshot.provider,
			metadata: loadedSnapshot.metadata
		};
	}, [loadedSnapshot, selectedId]);
	const selectedListChatSession = useMemo(() => {
		if (!selectedId) return null;
		return {
			session_id: selectedId,
			project_id: selectedSession?.project_id ?? selectedProjectId ?? "",
			activity: selectedSession?.activity ?? "idle",
			active_leaf_id: selectedSession?.active_leaf_id ?? null,
			provider: selectedSession?.provider ?? newSessionProvider,
			metadata: selectedSession?.metadata ?? {}
		};
	}, [newSessionProvider, selectedId, selectedProjectId, selectedSession]);
	const selectedChatSession = snapshotChatSession ?? selectedListChatSession;

	const activeProvider = loadedSnapshot?.provider ?? selectedSession?.provider ?? newSessionProvider;
	const activeProviderKind = activeProvider.kind;
	const reasoningEfforts = reasoningEffortsForProvider(activeProvider);
	const modelLocked = !!selectedId && !!loadedSnapshot && (loadedEntries.length > 0 || loadedSnapshot.active_leaf_id !== null);
	const modelControlsDisabled = !!selectedId && (!loadedSnapshot || loadedSnapshot.activity !== "idle");

	const configureProvider = useCallback(
		async (provider: ProviderConfig) => {
			const sessionId = selectedRef.current;
			if (!sessionId) {
				setNewSessionProvider(provider);
				return;
			}
			await api.configureSession({ sessionId, provider });
			patchProvider(sessionId, provider);
			scheduleSessionListRefresh();
		},
		[api, patchProvider, scheduleSessionListRefresh]
	);

	useEffect(() => {
		if (connection !== "open") return;
		let cancelled = false;
		void api
			.listTools(activeProviderKind)
			.then((nextTools) => {
				if (!cancelled) setTools(nextTools);
			})
			.catch((error) => {
				if (!cancelled) pushNotice("error", errorMessage(error));
			});
		return () => {
			cancelled = true;
		};
	}, [activeProviderKind, api, connection, pushNotice]);

	const changeModel = useCallback(
		async (modelKey: string) => {
			if (modelLocked) {
				pushNotice("info", "model is locked after the first transcript entry");
				return;
			}
			await configureProvider(providerFromModelKey(modelKey, activeProvider));
		},
		[activeProvider, configureProvider, modelLocked, pushNotice]
	);

	const changeReasoningEffort = useCallback(
		async (effort: ReasoningEffort) => {
			await configureProvider(withReasoningEffort(activeProvider, effort));
		},
		[activeProvider, configureProvider]
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
		await api.renameSession(renameSessionId, title);
		patchSessionMetadata(renameSessionId, { title });
		scheduleSessionListRefresh();
		pushNotice("success", `renamed session to “${truncate(title, 80)}”`);
		closeRenameDialog();
	}, [api, closeRenameDialog, patchSessionMetadata, pushNotice, renameSessionId, renameValue, scheduleSessionListRefresh]);

	const setSessionArchived = useCallback(
		async (session: SessionListItem, archived: boolean) => {
			const sessionId = session.session_id;
			const currentSnapshot = loadedSnapshot?.session_id === sessionId ? loadedSnapshot : null;
			const activity = currentSnapshot?.activity ?? session.activity;
			if (activity !== "idle") throw new Error("only idle sessions can be archived");
			const metadata = { ...(currentSnapshot?.metadata ?? session.metadata) };
			if (archived) metadata.archived = true;
			else delete metadata.archived;
			await api.configureSession({
				sessionId,
				provider: currentSnapshot?.provider ?? session.provider,
				metadata
			});
			patchSessionMetadata(sessionId, archived ? { archived: true } : {}, archived ? [] : ["archived"]);
			scheduleSessionListRefresh();
			pushNotice("success", archived ? `archived “${truncate(sessionTitle(session), 80)}”` : `unarchived “${truncate(sessionTitle(session), 80)}”`);
		},
		[api, loadedSnapshot, patchSessionMetadata, pushNotice, scheduleSessionListRefresh]
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
			if (refreshTimer.current !== null) {
				window.clearTimeout(refreshTimer.current);
				refreshTimer.current = null;
			}
			lastEventIds.current.delete(sessionId);
			deleteCachedSession(sessionId);
			composerHandleRef.current?.clearSession(sessionId);
			setSessions((currentSessions) => {
				const nextSessions = currentSessions.filter((candidate) => candidate.session_id !== sessionId);
				sessionsRef.current = new Map(nextSessions.map((candidate) => [candidate.session_id, candidate]));
				return nextSessions;
			});

			if (selectedRef.current === sessionId) {
				selectSession(null);
				composerHandleRef.current?.setValue("");
			}

			closeDeleteDialog();
			scheduleSessionListRefresh(0);
			pushNotice("success", `deleted “${truncate(title, 80)}”`);
		} catch (error) {
			setDeleteDialog((current) => (current?.session.session_id === sessionId ? { ...current, deleting: false } : current));
			throw error;
		}
	}, [api, closeDeleteDialog, deleteCachedSession, deleteDialog, pushNotice, refreshSelected, scheduleSessionListRefresh, selectSession]);

	const createSession = useCallback(
		(title?: string) => {
			if (!selectedProjectRef.current) {
				pushNotice("info", "select a project first");
				return null;
			}
			nextSessionTitleRef.current = title?.trim() || null;
			selectSession(null);
			composerHandleRef.current?.setValue("");
			requestAnimationFrame(() => composerHandleRef.current?.focus());
			return null;
		},
		[pushNotice, selectSession]
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
				await api.configureSession({
					sessionId,
					provider: current?.provider ?? selectedSession.provider,
					metadata
				});
				patchSessionMetadata(sessionId, {}, ["archived"]);
				scheduleSessionListRefresh();
			}
			const clientInputId = randomId("web_input");
			await api.queueFollowUp({
				sessionId,
				clientInputId,
				expectedActiveLeafId: loadedSnapshot?.activity === "idle" ? (loadedSnapshot.active_leaf_id ?? null) : undefined,
				content: textContent(text)
			});
		},
		[api, loadedSnapshot, patchSessionMetadata, refreshSelected, requireSelected, scheduleSessionListRefresh, selectedSession]
	);

	const startNewSession = useCallback(
		async (text: string) => {
			const projectId = selectedProjectRef.current;
			if (!projectId) throw new Error("select a project first");
			const sessionId = randomId("session");
			const title = nextSessionTitleRef.current || titleFromText(text);
			nextSessionTitleRef.current = null;
			const result = await api.startSession({
				sessionId,
				projectId,
				provider: newSessionProvider,
				metadata: { title, created_by: "web" },
				clientInputId: randomId("web_start"),
				priority: "follow_up",
				content: textContent(text)
			});
			await loadSessions();
			selectSession(result.session_id);
			return result.session_id;
		},
		[api, loadSessions, newSessionProvider, selectSession]
	);

	const forkFromTarget = useCallback(
		async (target: HistoryTargetOption, title?: string) => {
			const sessionId = requireSelected();
			const fork = await api.forkHistory({
				sessionId,
				leafId: target.sourceEntryId ?? target.id,
				placement: target.placement ?? "at"
			});
			const normalizedTitle = title?.trim();
			if (normalizedTitle) {
				await api.configureSession({
					sessionId: fork.session_id,
					provider: loadedSnapshot?.provider ?? selectedSession?.provider ?? DEFAULT_PROVIDER,
					metadata: {
						...(loadedSnapshot?.metadata ?? selectedSession?.metadata ?? {}),
						title: normalizedTitle
					}
				});
			}
			if (target.restoreText !== undefined) {
				composerHandleRef.current?.setValueForSession(fork.session_id, target.restoreText);
			}
			await loadSessions();
			selectSession(fork.session_id);
			pushNotice("success", `forked ${fork.session_id}`);
			return fork.session_id;
		},
		[api, loadSessions, loadedSnapshot, pushNotice, requireSelected, selectedSession, selectSession]
	);

	const switchToTarget = useCallback(
		async (target: HistoryTargetOption) => {
			const sessionId = requireSelected();
			const current = await refreshSelected(sessionId);
			if ((current?.snapshot.activity ?? loadedSnapshot?.activity) !== "idle") {
				throw new Error("stop the active turn before switching history");
			}
			await api.rewindHistory({
				sessionId,
				leafId: target.actionLeafId,
				expectedActiveLeafId: target.expectedActiveLeafId ?? current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null
			});
			markCachedSessionStale(sessionId);
			await refreshSelected(sessionId);
			if (target.restoreText !== undefined) {
				composerHandleRef.current?.setValue(target.restoreText);
			}
			scheduleSessionListRefresh();
			pushNotice("success", target.restoreText !== undefined ? "message restored for editing" : "switched to selected history point");
		},
		[api, loadedSnapshot?.active_leaf_id, loadedSnapshot?.activity, markCachedSessionStale, pushNotice, refreshSelected, requireSelected, scheduleSessionListRefresh]
	);

	const promoteQueuedInput = useCallback(
		async (inputId: string) => {
			const sessionId = requireSelected();
			const result = await api.promoteQueuedInput(sessionId, inputId);
			await Promise.all([refreshSelected(sessionId), loadSessions()]);
			if (!result.promoted && result.status !== "queued") {
				pushNotice("info", "message is already being processed");
			}
		},
		[api, loadSessions, pushNotice, refreshSelected, requireSelected]
	);

	const stopActiveTurn = useCallback(async () => {
		const sessionId = requireSelected();
		setStopping(true);
		try {
			await api.interrupt(sessionId);
			await Promise.all([refreshSelected(sessionId), loadSessions()]);
		} catch (error) {
			pushNotice("error", errorMessage(error));
		} finally {
			setStopping(false);
		}
	}, [api, loadSessions, pushNotice, refreshSelected, requireSelected]);

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
					expectedActiveLeafId: current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null
				});
				await Promise.all([refreshSelected(sessionId), loadSessions()]);
				pushNotice("success", result.outcome === "Interrupted" ? "continued turn" : "retry started");
			} finally {
				setResumingTurnId(null);
			}
		},
		[api, loadSessions, loadedSnapshot?.active_leaf_id, loadedSnapshot?.activity, pushNotice, refreshSelected, requireSelected]
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
				if (!args) {
					const next = await api.getConfig();
					setConfig(next);
					pushActionNotice("info", next.system_prompt ? `system: ${truncate(next.system_prompt, 320)}` : "system prompt is empty");
					return;
				}
				const systemPrompt = args === "clear" ? null : args;
				const next = await api.setConfig(systemPrompt);
				setConfig(next);
				pushActionNotice("success", systemPrompt ? "global system prompt updated" : "global system prompt cleared");
				return;
			}

			const sessionId = requireSelected();
			if (name === "fork") {
				const refreshed = loadedSnapshot?.activity === "running" ? null : await refreshSelected(sessionId);
				setHistoryDialog({
					mode: "fork",
					entries: refreshed?.entries ?? loadedEntries,
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null,
					initialForkTitle: args
				});
				return;
			}
			if (name === "switch") {
				const refreshed = await refreshSelected(sessionId);
				if ((refreshed?.snapshot.activity ?? loadedSnapshot?.activity) !== "idle") {
					throw new Error("stop the active turn before switching history");
				}
				setHistoryDialog({
					mode: "switch",
					entries: refreshed?.entries ?? loadedEntries,
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null
				});
				return;
			}
			if (name === "export") {
				const refreshed = await refreshSelected(sessionId);
				setExportDialog({
					entries: branchEntriesFor(
						refreshed?.entries ?? loadedEntries,
						refreshed?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null
					)
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
		[
			api,
			loadedEntries,
			loadedSnapshot,
			pushNotice,
			refreshSelected,
			requireSelected
		]
	);

	const submitComposer = useCallback(async (text: string) => {
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
	}, [executeSlash, pushNotice, queueUserInput, sending, startNewSession]);

	const layoutStyle = {
		gridTemplateColumns: rightOpen ? "320px minmax(0,1fr) minmax(320px,380px)" : "320px minmax(0,1fr)"
	};
	const canStop = !!selectedId && loadedSnapshot?.activity === "running";
	const queuedInputs = loadedSnapshot?.queued_inputs ?? [];
	const handleToggleArchived = useCallback(() => {
		setShowArchived((show) => !show);
	}, []);
	const handleSelectProject = useCallback((projectId: string) => {
		if (projectId === selectedProjectRef.current) return;
		selectedProjectRef.current = projectId;
		setSelectedProjectId(projectId);
		selectSession(null);
		setQuery("");
		composerHandleRef.current?.setValue("");
	}, [selectSession]);
	const openCreateProjectDialog = useCallback(() => {
		setProjectDialog({
			mode: "create",
			name: "",
			startingCwd: selectedProject?.starting_cwd ?? "",
			saving: false
		});
	}, [selectedProject?.starting_cwd]);
	const openEditProjectDialog = useCallback((project: Project) => {
		setProjectDialog({
			mode: "edit",
			projectId: project.project_id,
			name: projectTitle(project),
			startingCwd: project.starting_cwd,
			saving: false
		});
	}, []);
	const closeProjectDialog = useCallback(() => {
		setProjectDialog(null);
	}, []);
	const saveProjectDialog = useCallback(async () => {
		if (!projectDialog || projectDialog.saving) return;
		const name = projectDialog.name.trim();
		const startingCwd = projectDialog.startingCwd.trim();
		if (!name) throw new Error("project name is required");
		if (!startingCwd) throw new Error("starting cwd is required");
		setProjectDialog((current) => (current ? { ...current, saving: true } : current));
		try {
			const saved =
				projectDialog.mode === "create"
					? await api.createProject({ name, startingCwd, metadata: { created_by: "web" } })
					: await api.updateProject({
							projectId: projectDialog.projectId ?? "",
							name,
							startingCwd
						});
			await loadProjects();
			selectedProjectRef.current = saved.project_id;
			setSelectedProjectId(saved.project_id);
			selectSession(null);
			pushNotice("success", `${projectDialog.mode === "create" ? "created" : "updated"} project “${truncate(saved.name, 80)}”`);
			closeProjectDialog();
		} catch (error) {
			setProjectDialog((current) => (current ? { ...current, saving: false } : current));
			throw error;
		}
	}, [api, closeProjectDialog, loadProjects, projectDialog, pushNotice, selectSession]);
	const handleSidebarNew = useCallback(() => {
		void createSession();
	}, [createSession]);
	const handleArchiveToggle = useCallback(
		(session: SessionListItem) => {
			void setSessionArchived(session, !isArchivedSession(session)).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[pushNotice, setSessionArchived]
	);
	const handleSidebarDelete = useCallback((session: SessionListItem) => {
		setDeleteDialog({ session, deleting: false });
	}, []);
	const handleModelChange = useCallback(
		(value: string) => {
			void changeModel(value).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[changeModel, pushNotice]
	);
	const handleReasoningEffortChange = useCallback(
		(value: ReasoningEffort) => {
			void changeReasoningEffort(value).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[changeReasoningEffort, pushNotice]
	);
	const handleToggleRight = useCallback(() => {
		setRightOpen((open) => !open);
	}, []);
	const handleResumeTurn = useCallback(
		(entryId: string) => {
			void resumeTerminalTurn(entryId).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[pushNotice, resumeTerminalTurn]
	);
	const handleStop = useCallback(() => {
		void stopActiveTurn();
	}, [stopActiveTurn]);
	const handlePromoteQueued = useCallback(
		(inputId: string) => {
			void promoteQueuedInput(inputId).catch((error) => pushNotice("error", errorMessage(error)));
		},
		[promoteQueuedInput, pushNotice]
	);

	return (
		<div className="app-shell" style={layoutStyle}>
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
				onQueryChange={setQuery}
				onToggleArchived={handleToggleArchived}
				onNew={handleSidebarNew}
				onSelectProject={handleSelectProject}
				onNewProject={openCreateProjectDialog}
				onEditProject={openEditProjectDialog}
				onSelectSession={selectSession}
				onRename={openRenameDialog}
				onArchiveToggle={handleArchiveToggle}
				onDelete={handleSidebarDelete}
			/>

			<ChatPane
				session={selectedChatSession}
				snapshot={loadedSnapshot}
				entries={loadedEntries}
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
			/>

			<footer className="chat-dock" data-slot="chat-box">
				<Composer
					selectedId={selectedId}
					hasProject={!!selectedProjectId}
					composerHandleRef={composerHandleRef}
					sending={sending}
					canStop={canStop}
					stopping={stopping}
					queuedInputs={queuedInputs}
					onSubmit={submitComposer}
					onStop={handleStop}
					onPromoteQueued={handlePromoteQueued}
				/>
			</footer>

			{rightOpen ? (
				<aside className="inspector" data-slot="inspector">
					<Inspector snapshot={loadedSnapshot} config={config} tools={tools} />
				</aside>
			) : null}

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

			{historyDialog ? (
				<HistoryPickerDialog
					mode={historyDialog.mode}
					entries={historyDialog.entries}
					activeLeafId={historyDialog.activeLeafId}
					initialForkTitle={historyDialog.initialForkTitle}
					onClose={() => setHistoryDialog(null)}
					onFork={(target, title) => {
						void forkFromTarget(target, title)
							.then(() => setHistoryDialog(null))
							.catch((error) => pushNotice("error", errorMessage(error)));
					}}
					onSwitch={(target) => {
						void switchToTarget(target)
							.then(() => setHistoryDialog(null))
							.catch((error) => pushNotice("error", errorMessage(error)));
					}}
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

function RenameSessionDialog({
	value,
	onChange,
	onClose,
	onSubmit
}: {
	value: string;
	onChange: (value: string) => void;
	onClose: () => void;
	onSubmit: () => void;
}) {
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={onClose}>
			<div className="rename-dialog" role="dialog" aria-modal="true" aria-labelledby="rename-dialog-title" onMouseDown={(event) => event.stopPropagation()}>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="rename-dialog-title">Rename session</h2>
					</div>
					<button className="icon-button tiny" type="button" onClick={onClose} aria-label="close rename dialog">
						×
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
						<button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
						<button type="submit" className="primary-button">Save</button>
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
	onConfirm
}: {
	session: SessionListItem;
	deleting: boolean;
	onClose: () => void;
	onConfirm: () => void;
}) {
	const title = sessionTitle(session);
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={deleting ? undefined : onClose}>
			<div className="rename-dialog" role="dialog" aria-modal="true" aria-labelledby="delete-dialog-title" onMouseDown={(event) => event.stopPropagation()}>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="delete-dialog-title">Delete session</h2>
					</div>
					<button className="icon-button tiny" type="button" onClick={onClose} aria-label="close delete dialog" disabled={deleting}>
						×
					</button>
				</div>
				<div className="delete-dialog-body">
					<p>
						Delete <strong>{title}</strong> permanently?
					</p>
					<p className="muted">This removes the transcript, queued inputs, actions, and events for this session. This cannot be undone.</p>
				</div>
				<div className="rename-actions">
					<button type="button" className="secondary-button" onClick={onClose} disabled={deleting}>Cancel</button>
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
	onSubmit
}: {
	state: ProjectDialogState;
	onChange: (patch: Partial<ProjectDialogState>) => void;
	onClose: () => void;
	onSubmit: () => void;
}) {
	const title = state.mode === "create" ? "New project" : "Project settings";
	return (
		<div className="modal-scrim" role="presentation" onMouseDown={state.saving ? undefined : onClose}>
			<div className="rename-dialog project-dialog" role="dialog" aria-modal="true" aria-labelledby="project-dialog-title" onMouseDown={(event) => event.stopPropagation()}>
				<div className="rename-dialog-head">
					<div className="rename-dialog-copy">
						<h2 id="project-dialog-title">{title}</h2>
					</div>
					<button className="icon-button tiny" type="button" onClick={onClose} aria-label="close project dialog" disabled={state.saving}>
						×
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
					<label className="rename-field">
						<span>Starting cwd</span>
						<input
							value={state.startingCwd}
							onChange={(event) => onChange({ startingCwd: event.target.value })}
							placeholder="/path/to/project"
							required
							disabled={state.saving}
						/>
					</label>
					<div className="rename-actions">
						<button type="button" className="secondary-button" onClick={onClose} disabled={state.saving}>Cancel</button>
						<button type="submit" className="primary-button" disabled={state.saving}>
							{state.saving ? "Saving..." : "Save"}
						</button>
					</div>
				</form>
			</div>
		</div>
	);
}

function mergeSnapshotIntoSessionList(sessions: SessionSummary[], snapshot: SessionSnapshot): SessionSummary[] {
	let found = false;
	const nextSessions = sessions.map((session) => {
		if (session.session_id !== snapshot.session_id) return session;
		found = true;
		return {
			...session,
			project_id: snapshot.project_id,
			starting_cwd: snapshot.starting_cwd,
			activity: snapshot.activity,
			active_leaf_id: snapshot.active_leaf_id,
			provider: snapshot.provider,
			metadata: snapshot.metadata
		};
	});
	return found ? nextSessions : sessions;
}

function patchSessionSummaryList(sessions: SessionSummary[], sessionId: string, patch: Partial<SessionSummary>): SessionSummary[] {
	let changed = false;
	const nextSessions = sessions.map((session) => {
		if (session.session_id !== sessionId) return session;
		changed = true;
		return { ...session, ...patch };
	});
	return changed ? nextSessions : sessions;
}

function snapshotPatchFromSummary(summary: SessionSummary): Partial<CachedSnapshot> {
	return {
		project_id: summary.project_id,
		starting_cwd: summary.starting_cwd,
		activity: summary.activity,
		active_leaf_id: summary.active_leaf_id,
		provider: summary.provider,
		metadata: summary.metadata
	};
}

function snapshotPatchFromSummaryPatch(patch: Partial<SessionSummary>): Partial<CachedSnapshot> {
	const { created_at: _createdAt, updated_at: _updatedAt, ...snapshotPatch } = patch;
	void _createdAt;
	void _updatedAt;
	return snapshotPatch;
}

function mergeMetadata(metadata: Record<string, unknown>, patch: Record<string, unknown>, removeKeys: string[] = []): Record<string, unknown> {
	const next = { ...metadata, ...patch };
	for (const key of removeKeys) delete next[key];
	return next;
}

function patchSessionMetadataInList(
	sessions: SessionSummary[],
	sessionId: string,
	patch: Record<string, unknown>,
	removeKeys: string[] = []
): SessionSummary[] {
	let changed = false;
	const nextSessions = sessions.map((session) => {
		if (session.session_id !== sessionId) return session;
		changed = true;
		return { ...session, metadata: mergeMetadata(session.metadata, patch, removeKeys) };
	});
	return changed ? nextSessions : sessions;
}

function titleFromText(text: string): string {
	return truncate(firstLine(text).trim() || "New session", 64);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function isQueuedInputPatchEvent(event: string): boolean {
	return event === "input.consumed" || event === "input.promoted";
}

function applyQueuedInputEvent(
	event: EventFrame,
	setSnapshot: Dispatch<SetStateAction<SessionSnapshot | null>>
) {
	if (!isQueuedInputPatchEvent(event.event)) return;
	setSnapshot((current) => applyQueuedInputEventToSnapshot(event, current));
}

function applyQueuedInputEventToSnapshot<T extends Pick<SessionSnapshot, "session_id" | "queued_inputs">>(event: EventFrame, current: T): T;
function applyQueuedInputEventToSnapshot<T extends Pick<SessionSnapshot, "session_id" | "queued_inputs">>(event: EventFrame, current: T | null): T | null;
function applyQueuedInputEventToSnapshot<T extends Pick<SessionSnapshot, "session_id" | "queued_inputs">>(event: EventFrame, current: T | null): T | null {
	if (!current || current.session_id !== event.session_id) return current;
	const inputId = typeof event.data.input_id === "string" ? event.data.input_id : null;
	if (!inputId) return current;
	if (event.event === "input.consumed") {
		const queuedInputs = current.queued_inputs.filter((input) => input.input_id !== inputId);
		return queuedInputs.length === current.queued_inputs.length ? current : { ...current, queued_inputs: queuedInputs };
	}
	if (event.event !== "input.promoted") return current;
	const promotedAt = typeof event.data.promoted_at === "string" ? event.data.promoted_at : null;
	let changed = false;
	const queuedInputs = current.queued_inputs.map((input) => {
		if (input.input_id !== inputId) return input;
		changed = true;
		return { ...input, priority: "steer" as const, status: "queued" as const, promoted_at: promotedAt };
	});
	return changed ? { ...current, queued_inputs: queuedInputs } : current;
}

function modelErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "model request failed";
	return `model error: ${truncate(error, 420)}`;
}

function snapshotWithEntries(snapshot: CachedSnapshot): SessionSnapshot {
	return { ...snapshot, entries: undefined };
}

function trimSessionCache(cache: Map<string, CachedSession>) {
	while (cache.size > MAX_CACHED_SESSIONS) {
		const oldest = oldestCachedSessionId(cache);
		if (!oldest) break;
		cache.delete(oldest);
	}
	while (totalCachedEntryCount(cache) > MAX_CACHED_ENTRIES) {
		const oldest = oldestCachedSessionId(cache);
		if (!oldest) break;
		cache.delete(oldest);
	}
}

function oldestCachedSessionId(cache: Map<string, CachedSession>): string | null {
	let oldestId: string | null = null;
	let oldestAt = Number.POSITIVE_INFINITY;
	for (const [sessionId, cached] of cache) {
		if (cached.cachedAt >= oldestAt) continue;
		oldestId = sessionId;
		oldestAt = cached.cachedAt;
	}
	return oldestId;
}

function totalCachedEntryCount(cache: Map<string, CachedSession>): number {
	let total = 0;
	for (const cached of cache.values()) total += cached.entries.length;
	return total;
}
