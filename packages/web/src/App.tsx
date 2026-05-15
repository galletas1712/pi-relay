import { useQuery, useQueryClient } from "@tanstack/react-query";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { createAgentApi } from "./agentApi.ts";
import { ChatPane } from "./chatPane.tsx";
import { Composer, type ComposerHandle } from "./composer.tsx";
import { HistoryPickerDialog } from "./historyPicker.tsx";
import { branchEntriesFor, type HistoryTargetOption } from "./historyTargets.ts";
import { ExportDialog } from "./exportDialog.tsx";
import { randomId } from "./ids.ts";
import { Inspector, NoticeStack, Sidebar } from "./panels.tsx";
import { approximateJsonSize, perfEnabled, perfLog, perfNow } from "./perf.ts";
import { queryKeys } from "./queryKeys.ts";
import type { ConnectionStatus } from "./rpc.ts";
import { COMMANDS, findCommand, parseSlash, type ParsedSlash } from "./slash.ts";
import { reduceSessionEvent, type SessionPatchOperation } from "./sessionEvents.ts";
import {
	mergeSnapshotIntoSessionList,
	patchQueuedInputsInSnapshot,
	patchSessionActivityEverywhere,
	patchSessionMetadataEverywhere,
	patchSessionProviderEverywhere,
} from "./sessionQueryCache.ts";
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
import { projectTitle, sessionTitle, isArchivedSession, tallyActivities, type SessionListItem } from "./sessionList.ts";
import { firstLine, truncate } from "./text.ts";
import type {
	DaemonConfig,
	EventFrame,
	Notice,
	Project,
	ProviderConfig,
	ReasoningEffort,
	SessionSummary,
	ToolListing,
	TranscriptEntry,
} from "./types.ts";

const MAX_NOTICES = 24;
const NOTICE_TTL_MS = 4000;
const SESSION_LIST_REFRESH_DEBOUNCE_MS = 250;
const SELECTED_SESSION_REFRESH_DEBOUNCE_MS = 80;
const SELECTED_SESSION_QUERY_DISABLED_KEY = ["session", null] as const;

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
	const queryClient = useQueryClient();
	const [connection, setConnection] = useState<ConnectionStatus>("connecting");
	const [selectedProjectId, setSelectedProjectId] = useState<string | null>(null);
	const [selectedId, setSelectedId] = useState<string | null>(null);
	const selectedRef = useRef<string | null>(null);
	const [notices, setNotices] = useState<Notice[]>([]);
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
	const lastEventIds = useRef(new Map<string, number>());
	const subscribedEventSessionIds = useRef(new Set<string>());

	const pushNotice = useCallback((tone: Notice["tone"], text: string) => {
		setNotices((current) => [...current.slice(Math.max(0, current.length - MAX_NOTICES + 1)), { id: randomId("notice"), tone, text }]);
	}, []);

	useEffect(() => {
		selectedRef.current = selectedId;
	}, [selectedId]);

	useEffect(() => {
		selectedProjectRef.current = selectedProjectId;
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
		enabled: connection === "open" && !!selectedProjectId,
	});
	const sessions = sessionsQuery.data ?? [];

	const selectedSessionQuery = useQuery({
		queryKey: selectedId ? queryKeys.session(selectedId, "full_tree") : SELECTED_SESSION_QUERY_DISABLED_KEY,
		queryFn: async () => {
			if (!selectedId) throw new Error("missing selected session id");
			const shouldLogPerf = perfEnabled();
			const startedAt = perfNow();
			if (shouldLogPerf)
				perfLog("session.get start", {
					sessionId: selectedId,
					source: "query",
				});
			const nextSnapshot = await api.getSession(selectedId, {
				includeEntries: true,
			});
			if (shouldLogPerf) {
				const rpcMs = perfNow() - startedAt;
				perfLog("session.get end", {
					sessionId: selectedId,
					entries: nextSnapshot.entries?.length ?? 0,
					approxBytes: approximateJsonSize(nextSnapshot),
					rpcMs: Math.round(rpcMs),
					entryScope: "full_tree",
				});
			}
			return nextSnapshot;
		},
		enabled: connection === "open" && !!selectedId,
		placeholderData: undefined,
	});

	const configQuery = useQuery({
		queryKey: queryKeys.config,
		queryFn: () => api.getConfig(),
		enabled: connection === "open",
	});
	const config: DaemonConfig = configQuery.data ?? { system_prompt: null };

	const sessionItems: SessionListItem[] = sessions;
	const selectedProject = useMemo(
		() => projects.find((project) => project.project_id === selectedProjectId) ?? null,
		[projects, selectedProjectId],
	);

	const selectedSession = useMemo(
		() => sessionItems.find((session) => session.session_id === selectedId) ?? null,
		[sessionItems, selectedId],
	);

	const rawSnapshot = selectedSessionQuery.data ?? null;
	const loadedSnapshot = rawSnapshot?.session_id === selectedId ? rawSnapshot : null;
	const loadedEntries = loadedSnapshot ? (loadedSnapshot.entries ?? []) : [];
	const transcriptLoading = !!selectedId && !loadedSnapshot && selectedSessionQuery.isFetching;

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
			project_id: selectedSession?.project_id ?? selectedProjectId ?? "",
			activity: selectedSession?.activity ?? "idle",
			active_leaf_id: selectedSession?.active_leaf_id ?? null,
			provider: selectedSession?.provider ?? newSessionProvider,
			metadata: selectedSession?.metadata ?? {},
		};
	}, [newSessionProvider, selectedId, selectedProjectId, selectedSession]);
	const selectedChatSession = snapshotChatSession ?? selectedListChatSession;

	const activeProvider = loadedSnapshot?.provider ?? selectedSession?.provider ?? newSessionProvider;
	const activeProviderKind = activeProvider.kind;
	const toolsQuery = useQuery({
		queryKey: queryKeys.tools(activeProviderKind),
		queryFn: () => api.listTools(activeProviderKind),
		enabled: connection === "open",
	});
	const tools: ToolListing[] = toolsQuery.data ?? [];
	const reasoningEfforts = reasoningEffortsForProvider(activeProvider);
	const modelLocked = !!selectedId && !!loadedSnapshot && (loadedEntries.length > 0 || loadedSnapshot.active_leaf_id !== null);
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
	}, []);

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

	const invalidateSelectedSession = useCallback(
		(sessionId = selectedRef.current) => {
			if (!sessionId) return;
			void queryClient.invalidateQueries({
				queryKey: queryKeys.session(sessionId, "full_tree"),
			});
		},
		[queryClient],
	);

	const scheduleSelectedRefresh = useCallback(
		(sessionId = selectedRef.current, delayMs = SELECTED_SESSION_REFRESH_DEBOUNCE_MS) => {
			if (!sessionId || sessionId !== selectedRef.current) return;
			if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current);
			refreshTimer.current = window.setTimeout(() => {
				refreshTimer.current = null;
				invalidateSelectedSession(sessionId);
			}, delayMs);
		},
		[invalidateSelectedSession],
	);

	const getFreshSession = useCallback(
		async (sessionId: string) => {
			const snapshot = await queryClient.fetchQuery({
				queryKey: queryKeys.session(sessionId, "full_tree"),
				queryFn: async () => {
					const shouldLogPerf = perfEnabled();
					const startedAt = perfNow();
					if (shouldLogPerf) perfLog("session.get start", { sessionId, source: "fetch" });
					const nextSnapshot = await api.getSession(sessionId, {
						includeEntries: true,
					});
					if (shouldLogPerf) {
						const rpcMs = perfNow() - startedAt;
						perfLog("session.get end", {
							sessionId,
							entries: nextSnapshot.entries?.length ?? 0,
							approxBytes: approximateJsonSize(nextSnapshot),
							rpcMs: Math.round(rpcMs),
							entryScope: "full_tree",
						});
					}
					return nextSnapshot;
				},
				staleTime: 0,
			});
			lastEventIds.current.set(sessionId, snapshot.last_event_id);
			queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(snapshot.project_id), (current) =>
				mergeSnapshotIntoSessionList(current, snapshot),
			);
			if (snapshot.project_id !== selectedProjectRef.current) {
				selectedProjectRef.current = snapshot.project_id;
				setSelectedProjectId(snapshot.project_id);
			}
			return { snapshot, entries: snapshot.entries ?? [] };
		},
		[api, queryClient],
	);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current) => {
			if (!sessionId) return null;
			return getFreshSession(sessionId);
		},
		[getFreshSession],
	);

	const applySessionOperation = useCallback(
		(operation: SessionPatchOperation) => {
			if (operation.type === "metadata") {
				patchSessionMetadataEverywhere(queryClient, selectedProjectRef.current, operation.sessionId, operation.patch, operation.remove);
				return;
			}
			if (operation.type === "provider") {
				patchSessionProviderEverywhere(queryClient, selectedProjectRef.current, operation.sessionId, operation.provider);
				return;
			}
			if (operation.type === "activity") {
				patchSessionActivityEverywhere(queryClient, selectedProjectRef.current, operation.sessionId, operation.activity);
				return;
			}
			if (operation.type === "queued_inputs") {
				patchQueuedInputsInSnapshot(queryClient, operation.event);
				return;
			}
			if (operation.type === "invalidate_session") {
				if (operation.sessionId === selectedRef.current) scheduleSelectedRefresh(operation.sessionId);
				else
					void queryClient.invalidateQueries({
						queryKey: queryKeys.session(operation.sessionId, "full_tree"),
					});
				return;
			}
			scheduleSessionListRefresh();
		},
		[queryClient, scheduleSelectedRefresh, scheduleSessionListRefresh],
	);

	const handleSessionEvent = useCallback(
		(event: EventFrame) => {
			const currentSessions = queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(selectedProjectRef.current));
			const eventSession = currentSessions?.find((session) => session.session_id === event.session_id);
			if (eventSession?.project_id && eventSession.project_id !== selectedProjectRef.current) return;
			lastEventIds.current.set(event.session_id, Math.max(lastEventIds.current.get(event.session_id) ?? 0, event.event_id));

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
		[applySessionOperation, pushNotice, queryClient],
	);

	useEffect(() => {
		const offStatus = api.onStatus((status) => {
			setConnection(status);
			if (status !== "open") {
				subscribedEventSessionIds.current.clear();
				return;
			}
			void Promise.all([
				queryClient.invalidateQueries({ queryKey: queryKeys.projects }),
				queryClient.invalidateQueries({ queryKey: queryKeys.config }),
				queryClient.invalidateQueries({
					queryKey: queryKeys.sessions(selectedProjectRef.current),
				}),
			]).catch((error) => pushNotice("error", errorMessage(error)));
		});
		const offEvent = api.onEvent(handleSessionEvent);
		void api.connect().catch((error) => pushNotice("error", errorMessage(error)));
		return () => {
			offStatus();
			offEvent();
			if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current);
			if (sessionListRefreshTimer.current !== null) window.clearTimeout(sessionListRefreshTimer.current);
			api.close();
		};
	}, [api, handleSessionEvent, pushNotice, queryClient]);

	useEffect(() => {
		if (projectsQuery.error) pushNotice("error", errorMessage(projectsQuery.error));
	}, [projectsQuery.error, pushNotice]);
	useEffect(() => {
		if (sessionsQuery.error) pushNotice("error", errorMessage(sessionsQuery.error));
	}, [sessionsQuery.error, pushNotice]);
	useEffect(() => {
		if (selectedSessionQuery.error) pushNotice("error", errorMessage(selectedSessionQuery.error));
	}, [selectedSessionQuery.error, pushNotice]);
	useEffect(() => {
		if (configQuery.error) pushNotice("error", errorMessage(configQuery.error));
	}, [configQuery.error, pushNotice]);
	useEffect(() => {
		if (toolsQuery.error) pushNotice("error", errorMessage(toolsQuery.error));
	}, [toolsQuery.error, pushNotice]);

	useEffect(() => {
		if (projectsQuery.status !== "success") return;
		const currentProjectId = selectedProjectRef.current;
		const nextSelected =
			currentProjectId && projects.some((project) => project.project_id === currentProjectId)
				? currentProjectId
				: (projects[0]?.project_id ?? null);
		if (nextSelected === currentProjectId) return;
		selectedProjectRef.current = nextSelected;
		setSelectedProjectId(nextSelected);
		selectSession(null);
		setQuery("");
		composerHandleRef.current?.setValue("");
	}, [projects, projectsQuery.status, selectSession]);

	useEffect(() => {
		if (!selectedId) return;
		if (sessionItems.some((session) => session.session_id === selectedId)) return;
		if (selectedSessionQuery.fetchStatus === "fetching") return;
		if (loadedSnapshot?.session_id === selectedId) return;
		selectSession(null);
	}, [loadedSnapshot?.session_id, selectSession, selectedId, selectedSessionQuery.fetchStatus, sessionItems]);

	useEffect(() => {
		if (!loadedSnapshot) return;
		lastEventIds.current.set(loadedSnapshot.session_id, loadedSnapshot.last_event_id);
		queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(loadedSnapshot.project_id), (current) =>
			mergeSnapshotIntoSessionList(current, loadedSnapshot),
		);
	}, [loadedSnapshot, queryClient]);

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
					if (selectedRef.current === sessionId)
						return queryClient.invalidateQueries({
							queryKey: queryKeys.session(sessionId, "full_tree"),
						});
					return undefined;
				})
				.catch((error) => {
					subscribedEventSessionIds.current.delete(sessionId);
					pushNotice("error", errorMessage(error));
				});
		}
	}, [api, connection, handleSessionEvent, pushNotice, queryClient, selectedId, sessions]);

	const configureProvider = useCallback(
		async (provider: ProviderConfig) => {
			const sessionId = selectedRef.current;
			if (!sessionId) {
				setNewSessionProvider(provider);
				return;
			}
			await api.configureSession({ sessionId, provider });
			patchSessionProviderEverywhere(queryClient, selectedProjectRef.current, sessionId, provider);
			invalidateSessionList();
		},
		[api, invalidateSessionList, queryClient],
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
		await api.renameSession(renameSessionId, title);
		patchSessionMetadataEverywhere(queryClient, selectedProjectRef.current, renameSessionId, { title });
		invalidateSessionList();
		pushNotice("success", `renamed session to “${truncate(title, 80)}”`);
		closeRenameDialog();
	}, [api, closeRenameDialog, invalidateSessionList, pushNotice, queryClient, renameSessionId, renameValue]);

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
				metadata,
			});
			patchSessionMetadataEverywhere(
				queryClient,
				selectedProjectRef.current,
				sessionId,
				archived ? { archived: true } : {},
				archived ? [] : ["archived"],
			);
			invalidateSessionList();
			pushNotice(
				"success",
				archived ? `archived “${truncate(sessionTitle(session), 80)}”` : `unarchived “${truncate(sessionTitle(session), 80)}”`,
			);
		},
		[api, invalidateSessionList, loadedSnapshot, pushNotice, queryClient],
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
			queryClient.removeQueries({
				queryKey: queryKeys.session(sessionId, "full_tree"),
			});
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
	}, [api, closeDeleteDialog, deleteDialog, invalidateSessionList, pushNotice, queryClient, refreshSelected, selectSession]);

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
		[pushNotice, selectSession],
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
					metadata,
				});
				patchSessionMetadataEverywhere(queryClient, selectedProjectRef.current, sessionId, {}, ["archived"]);
				invalidateSessionList();
			}
			const clientInputId = randomId("web_input");
			await api.queueFollowUp({
				sessionId,
				clientInputId,
				expectedActiveLeafId: loadedSnapshot?.activity === "idle" ? (loadedSnapshot.active_leaf_id ?? null) : undefined,
				content: textContent(text),
			});
		},
		[api, invalidateSessionList, loadedSnapshot, queryClient, refreshSelected, requireSelected, selectedSession],
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
				content: textContent(text),
			});
			await queryClient.invalidateQueries({
				queryKey: queryKeys.sessions(projectId),
			});
			selectSession(result.session_id);
			return result.session_id;
		},
		[api, newSessionProvider, queryClient, selectSession],
	);

	const forkFromTarget = useCallback(
		async (target: HistoryTargetOption, title?: string) => {
			const sessionId = requireSelected();
			const fork = await api.forkHistory({
				sessionId,
				leafId: target.sourceEntryId ?? target.id,
				placement: target.placement ?? "at",
			});
			const normalizedTitle = title?.trim();
			if (normalizedTitle) {
				await api.configureSession({
					sessionId: fork.session_id,
					provider: loadedSnapshot?.provider ?? selectedSession?.provider ?? DEFAULT_PROVIDER,
					metadata: {
						...(loadedSnapshot?.metadata ?? selectedSession?.metadata ?? {}),
						title: normalizedTitle,
					},
				});
			}
			if (target.restoreText !== undefined) {
				composerHandleRef.current?.setValueForSession(fork.session_id, target.restoreText);
			}
			invalidateSessionList();
			selectSession(fork.session_id);
			pushNotice("success", `forked ${fork.session_id}`);
			return fork.session_id;
		},
		[api, invalidateSessionList, loadedSnapshot, pushNotice, requireSelected, selectedSession, selectSession],
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
				expectedActiveLeafId: target.expectedActiveLeafId ?? current?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null,
			});
			await queryClient.invalidateQueries({
				queryKey: queryKeys.session(sessionId, "full_tree"),
			});
			if (target.restoreText !== undefined) {
				composerHandleRef.current?.setValue(target.restoreText);
			}
			invalidateSessionList();
			pushNotice("success", target.restoreText !== undefined ? "message restored for editing" : "switched to selected history point");
		},
		[
			api,
			invalidateSessionList,
			loadedSnapshot?.active_leaf_id,
			loadedSnapshot?.activity,
			pushNotice,
			queryClient,
			refreshSelected,
			requireSelected,
		],
	);

	const promoteQueuedInput = useCallback(
		async (inputId: string) => {
			const sessionId = requireSelected();
			const result = await api.promoteQueuedInput(sessionId, inputId);
			patchQueuedInputsInSnapshot(queryClient, {
				event_id: 0,
				event: "input.promoted",
				session_id: sessionId,
				data: { input_id: inputId },
			});
			await Promise.all([
				queryClient.invalidateQueries({
					queryKey: queryKeys.session(sessionId, "full_tree"),
				}),
				queryClient.invalidateQueries({
					queryKey: queryKeys.sessions(selectedProjectRef.current),
				}),
			]);
			if (!result.promoted && result.status !== "queued") {
				pushNotice("info", "message is already being processed");
			}
		},
		[api, pushNotice, queryClient, requireSelected],
	);

	const stopActiveTurn = useCallback(async () => {
		const sessionId = requireSelected();
		setStopping(true);
		try {
			await api.interrupt(sessionId);
			await Promise.all([
				queryClient.invalidateQueries({
					queryKey: queryKeys.session(sessionId, "full_tree"),
				}),
				queryClient.invalidateQueries({
					queryKey: queryKeys.sessions(selectedProjectRef.current),
				}),
			]);
		} catch (error) {
			pushNotice("error", errorMessage(error));
		} finally {
			setStopping(false);
		}
	}, [api, pushNotice, queryClient, requireSelected]);

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
					queryClient.invalidateQueries({
						queryKey: queryKeys.session(sessionId, "full_tree"),
					}),
					queryClient.invalidateQueries({
						queryKey: queryKeys.sessions(selectedProjectRef.current),
					}),
				]);
				pushNotice("success", result.outcome === "Interrupted" ? "continued turn" : "retry started");
			} finally {
				setResumingTurnId(null);
			}
		},
		[api, loadedSnapshot?.active_leaf_id, loadedSnapshot?.activity, pushNotice, queryClient, refreshSelected, requireSelected],
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
					const next = await queryClient.fetchQuery({
						queryKey: queryKeys.config,
						queryFn: () => api.getConfig(),
						staleTime: 0,
					});
					pushActionNotice("info", next.system_prompt ? `system: ${truncate(next.system_prompt, 320)}` : "system prompt is empty");
					return;
				}
				const systemPrompt = args === "clear" ? null : args;
				const next = await api.setConfig(systemPrompt);
				queryClient.setQueryData(queryKeys.config, next);
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
					initialForkTitle: args,
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
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null,
				});
				return;
			}
			if (name === "export") {
				const refreshed = await refreshSelected(sessionId);
				setExportDialog({
					entries: branchEntriesFor(
						refreshed?.entries ?? loadedEntries,
						refreshed?.snapshot.active_leaf_id ?? loadedSnapshot?.active_leaf_id ?? null,
					),
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
		[api, loadedEntries, loadedSnapshot, pushNotice, queryClient, refreshSelected, requireSelected],
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

	const layoutStyle = {
		gridTemplateColumns: rightOpen ? "320px minmax(0,1fr) minmax(320px,380px)" : "320px minmax(0,1fr)",
	};
	const canStop = !!selectedId && loadedSnapshot?.activity === "running";
	const queuedInputs = loadedSnapshot?.queued_inputs ?? [];
	const handleToggleArchived = useCallback(() => {
		setShowArchived((show) => !show);
	}, []);
	const handleSelectProject = useCallback(
		(projectId: string) => {
			if (projectId === selectedProjectRef.current) return;
			selectedProjectRef.current = projectId;
			setSelectedProjectId(projectId);
			selectSession(null);
			setQuery("");
			composerHandleRef.current?.setValue("");
		},
		[selectSession],
	);
	const openCreateProjectDialog = useCallback(() => {
		setProjectDialog({
			mode: "create",
			name: "",
			startingCwd: selectedProject?.starting_cwd ?? "",
			saving: false,
		});
	}, [selectedProject?.starting_cwd]);
	const openEditProjectDialog = useCallback((project: Project) => {
		setProjectDialog({
			mode: "edit",
			projectId: project.project_id,
			name: projectTitle(project),
			startingCwd: project.starting_cwd,
			saving: false,
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
					? await api.createProject({
							name,
							startingCwd,
							metadata: { created_by: "web" },
						})
					: await api.updateProject({
							projectId: projectDialog.projectId ?? "",
							name,
							startingCwd,
						});
			await queryClient.invalidateQueries({ queryKey: queryKeys.projects });
			selectedProjectRef.current = saved.project_id;
			setSelectedProjectId(saved.project_id);
			selectSession(null);
			pushNotice("success", `${projectDialog.mode === "create" ? "created" : "updated"} project “${truncate(saved.name, 80)}”`);
			closeProjectDialog();
		} catch (error) {
			setProjectDialog((current) => (current ? { ...current, saving: false } : current));
			throw error;
		}
	}, [api, closeProjectDialog, projectDialog, pushNotice, queryClient, selectSession]);
	const handleSidebarNew = useCallback(() => {
		void createSession();
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

function titleFromText(text: string): string {
	return truncate(firstLine(text).trim() || "New session", 64);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function modelErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "model request failed";
	return `model error: ${truncate(error, 420)}`;
}

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
