import { useCallback, useEffect, useMemo, useRef, useState, type Dispatch, type KeyboardEvent, type SetStateAction } from "react";
import { createAgentApi } from "./agentApi.ts";
import { Composer } from "./composer.tsx";
import { HistoryPickerDialog } from "./historyPicker.tsx";
import { branchEntriesFor, type HistoryTargetOption } from "./historyTargets.ts";
import { ExportDialog } from "./exportDialog.tsx";
import { randomId } from "./ids.ts";
import {
	Inspector,
	LogHeader,
	NoticeStack,
	SidebarHeader,
	SidebarToolbar,
	SessionRow
} from "./panels.tsx";
import type { ConnectionStatus } from "./rpc.ts";
import { COMMANDS, filterCommands, findCommand, matchSlashPrefix, parseSlash, type ParsedSlash } from "./slash.ts";
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
import { sessionTitle, isArchivedSession, tallyActivities, type SessionListItem } from "./sessionList.ts";
import { firstLine, truncate } from "./text.ts";
import { MessageList } from "./transcript.tsx";
import type {
	DaemonConfig,
	EventFrame,
	Notice,
	ProviderConfig,
	ReasoningEffort,
	SessionSnapshot,
	SessionSummary,
	ToolListing,
	TranscriptEntry
} from "./types.ts";

const MAX_NOTICES = 24;
const NOTICE_TTL_MS = 4000;
const OLD_DRAFT_STORAGE_KEYS = ["pi-relay.web.draft-sessions.v1", "pi-relay.web.composer-drafts.v1"];

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

export function App() {
	const api = useMemo(() => createAgentApi(), []);
	const [connection, setConnection] = useState<ConnectionStatus>("connecting");
	const [sessions, setSessions] = useState<SessionSummary[]>([]);
	const [selectedId, setSelectedId] = useState<string | null>(null);
	const selectedRef = useRef<string | null>(null);
	const [snapshot, setSnapshot] = useState<SessionSnapshot | null>(null);
	const [entries, setEntries] = useState<TranscriptEntry[]>([]);
	const [notices, setNotices] = useState<Notice[]>([]);
	const [config, setConfig] = useState<DaemonConfig>({ system_prompt: null });
	const [tools, setTools] = useState<ToolListing[]>([]);
	const [query, setQuery] = useState("");
	const [composer, setComposer] = useState("");
	const [newSessionProvider, setNewSessionProvider] = useState<ProviderConfig>(DEFAULT_PROVIDER);
	const [slashIndex, setSlashIndex] = useState(0);
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

	const refreshTimer = useRef<number | null>(null);
	const composerRef = useRef<HTMLTextAreaElement | null>(null);
	const nextSessionTitleRef = useRef<string | null>(null);
	const pendingComposerBySession = useRef(new Map<string, string>());
	const lastEventIds = useRef(new Map<string, number>());

	useEffect(() => {
		try {
			for (const key of OLD_DRAFT_STORAGE_KEYS) window.localStorage.removeItem(key);
		} catch {
			// Local draft cleanup is best-effort; the app no longer reads these keys.
		}
	}, []);

	const selectSession = useCallback(
		(sessionId: string | null) => {
			if (sessionId === null) nextSessionTitleRef.current = null;
			selectedRef.current = sessionId;
			setSelectedId(sessionId);
		},
		[]
	);

	useEffect(() => {
		selectedRef.current = selectedId;
	}, [selectedId]);

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

	const loadSessions = useCallback(async () => {
		const nextSessions = await api.listSessions(100);
		setSessions(nextSessions);
		return nextSessions;
	}, [api]);

	const loadGlobal = useCallback(async () => {
		const nextConfig = await api.getConfig();
		setConfig(nextConfig);
	}, [api]);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current) => {
			if (!sessionId) return null;
			const nextSnapshot = await api.getSession(sessionId, { includeEntries: true });
			if (selectedRef.current !== sessionId) return null;
			lastEventIds.current.set(sessionId, nextSnapshot.last_event_id);
			setSnapshot(nextSnapshot);
			setEntries(nextSnapshot.entries ?? []);
			setSessions((current) => mergeSnapshotIntoSessionList(current, nextSnapshot));
			return { snapshot: nextSnapshot, entries: nextSnapshot.entries ?? [] };
		},
		[api]
	);

	const scheduleSelectedRefresh = useCallback((sessionId = selectedRef.current, delayMs = 80) => {
		if (!sessionId || sessionId !== selectedRef.current) return;
		if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current);
		refreshTimer.current = window.setTimeout(() => {
			refreshTimer.current = null;
			void refreshSelected(sessionId).catch((error) => pushNotice("error", errorMessage(error)));
		}, delayMs);
	}, [pushNotice, refreshSelected]);

	const handleSessionEvent = useCallback(
		(event: EventFrame) => {
			lastEventIds.current.set(event.session_id, Math.max(lastEventIds.current.get(event.session_id) ?? 0, event.event_id));
			if (event.session_id === selectedRef.current) {
				applyQueuedInputEvent(event, setSnapshot);
				if (event.event === "model.error") pushNotice("error", modelErrorNotice(event.data));
				if (event.event === "turn.finished") {
					const outcome = typeof event.data.outcome === "string" ? event.data.outcome : null;
					if (outcome === "Interrupted") pushNotice("info", "turn interrupted");
					if (outcome === "Crashed") pushNotice("error", "turn crashed");
				}
				scheduleSelectedRefresh(event.session_id);
				if (isTerminalActivityEvent(event.event)) {
					window.setTimeout(() => {
						void refreshSelected(event.session_id).catch((error) => pushNotice("error", errorMessage(error)));
						void loadSessions().catch(() => undefined);
					}, 350);
				}
			}
			if (event.event === "session.created" || event.event === "session.configured" || event.event === "history.forked" || isTerminalActivityEvent(event.event)) {
				void loadSessions().catch(() => undefined);
			}
		},
		[loadSessions, pushNotice, refreshSelected, scheduleSelectedRefresh]
	);

	useEffect(() => {
		const offStatus = api.onStatus((status) => {
			setConnection(status);
			if (status !== "open") return;
			const sessionId = selectedRef.current;
			if (!sessionId) return;
			void api
				.subscribeEvents(sessionId, lastEventIds.current.get(sessionId) ?? null)
				.then((replayed) => {
					for (const event of replayed) handleSessionEvent(event);
					return refreshSelected(sessionId);
				})
				.catch((error) => pushNotice("error", errorMessage(error)));
		});
		const offEvent = api.onEvent(handleSessionEvent);
		void api
			.connect()
			.then(async () => {
				await Promise.all([loadSessions(), loadGlobal()]);
			})
			.catch((error) => pushNotice("error", errorMessage(error)));
		return () => {
			offStatus();
			offEvent();
			api.close();
		};
	}, [api, handleSessionEvent, loadGlobal, loadSessions, pushNotice, refreshSelected, selectSession]);

	useEffect(() => {
		if (!selectedId) {
			setSnapshot(null);
			setEntries([]);
			return;
		}
		setComposer(pendingComposerBySession.current.get(selectedId) ?? "");
		pendingComposerBySession.current.delete(selectedId);
		let cancelled = false;
		void api
			.subscribeEvents(selectedId, lastEventIds.current.get(selectedId) ?? null)
			.then((replayed) => {
				if (cancelled) return undefined;
				for (const event of replayed) handleSessionEvent(event);
				return refreshSelected(selectedId);
			})
			.catch((error) => {
				if (!cancelled) pushNotice("error", errorMessage(error));
			});
		return () => {
			cancelled = true;
			if (api.isOpen()) {
				void api.unsubscribeEvents(selectedId).catch(() => undefined);
			}
		};
	}, [api, handleSessionEvent, pushNotice, refreshSelected, selectedId]);

	const sessionItems = useMemo<SessionListItem[]>(
		() => [...sessions].sort(compareSessionsForSidebar),
		[sessions]
	);

	const selectedSession = useMemo(
		() => sessionItems.find((session) => session.session_id === selectedId) ?? null,
		[sessionItems, selectedId]
	);

	const activeProvider = snapshot?.provider ?? selectedSession?.provider ?? newSessionProvider;
	const activeProviderKind = activeProvider.kind;
	const reasoningEfforts = reasoningEffortsForProvider(activeProvider);
	const modelLocked = !!selectedId && (entries.length > 0 || snapshot?.active_leaf_id !== null);
	const modelControlsDisabled = !!selectedId && snapshot?.activity !== "idle";

	const configureProvider = useCallback(
		async (provider: ProviderConfig) => {
			const sessionId = selectedRef.current;
			if (!sessionId) {
				setNewSessionProvider(provider);
				return;
			}
			await api.configureSession({ sessionId, provider });
			await Promise.all([loadSessions(), refreshSelected(sessionId)]);
		},
		[api, loadSessions, refreshSelected]
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
		await Promise.all([loadSessions(), renameSessionId === selectedRef.current ? refreshSelected(renameSessionId) : Promise.resolve(null)]);
		pushNotice("success", `renamed session to “${truncate(title, 80)}”`);
		closeRenameDialog();
	}, [api, closeRenameDialog, loadSessions, pushNotice, refreshSelected, renameSessionId, renameValue]);

	const setSessionArchived = useCallback(
		async (session: SessionListItem, archived: boolean) => {
			const current = session.session_id === selectedRef.current ? await refreshSelected(session.session_id) : null;
			const activity = current?.snapshot.activity ?? session.activity;
			if (activity !== "idle") throw new Error("only idle sessions can be archived");
			const metadata = { ...(current?.snapshot.metadata ?? session.metadata) };
			if (archived) metadata.archived = true;
			else delete metadata.archived;
			await api.configureSession({
				sessionId: session.session_id,
				provider: current?.snapshot.provider ?? session.provider,
				metadata
			});
			await Promise.all([loadSessions(), session.session_id === selectedRef.current ? refreshSelected(session.session_id) : Promise.resolve(null)]);
			pushNotice("success", archived ? `archived “${truncate(sessionTitle(session), 80)}”` : `unarchived “${truncate(sessionTitle(session), 80)}”`);
		},
		[api, loadSessions, pushNotice, refreshSelected]
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
			pendingComposerBySession.current.delete(sessionId);
			setSessions((currentSessions) => currentSessions.filter((session) => session.session_id !== sessionId));

			if (selectedRef.current === sessionId) {
				selectSession(null);
				setSnapshot(null);
				setEntries([]);
				setComposer("");
			}

			closeDeleteDialog();
			await loadSessions();
			pushNotice("success", `deleted “${truncate(title, 80)}”`);
		} catch (error) {
			setDeleteDialog((current) => (current?.session.session_id === sessionId ? { ...current, deleting: false } : current));
			throw error;
		}
	}, [api, closeDeleteDialog, deleteDialog, loadSessions, pushNotice, refreshSelected, selectSession]);

	const slashState = useMemo<{ visible: boolean; commands: typeof COMMANDS }>(() => {
		const prefix = matchSlashPrefix(composer);
		if (prefix === null) return { visible: false, commands: [] };
		return { visible: true, commands: filterCommands(prefix) };
	}, [composer]);

	useEffect(() => {
		setSlashIndex(0);
	}, [slashState.commands, slashState.visible]);

	const createSession = useCallback(
		(title?: string) => {
			nextSessionTitleRef.current = title?.trim() || null;
			selectSession(null);
			setSnapshot(null);
			setEntries([]);
			setComposer("");
			requestAnimationFrame(() => composerRef.current?.focus());
			return null;
		},
		[selectSession]
	);

	const requireSelected = useCallback(() => {
		if (!selectedRef.current) throw new Error("select a session first");
		return selectedRef.current;
	}, []);

	const queueUserInput = useCallback(
		async (text: string) => {
			const sessionId = requireSelected();
			if (selectedSession && isArchivedSession(selectedSession)) {
				const current = snapshot ?? (await refreshSelected(sessionId))?.snapshot;
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
			}
			const clientInputId = randomId("web_input");
			await api.queueFollowUp({
				sessionId,
				clientInputId,
				expectedActiveLeafId: snapshot?.activity === "idle" ? (snapshot.active_leaf_id ?? null) : undefined,
				content: textContent(text)
			});
		},
		[api, refreshSelected, requireSelected, selectedSession, snapshot]
	);

	const startNewSession = useCallback(
		async (text: string) => {
			const sessionId = randomId("session");
			const title = nextSessionTitleRef.current || titleFromText(text);
			nextSessionTitleRef.current = null;
				const result = await api.startSession({
					sessionId,
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
					provider: snapshot?.provider ?? selectedSession?.provider ?? DEFAULT_PROVIDER,
					metadata: {
						...(snapshot?.metadata ?? selectedSession?.metadata ?? {}),
						title: normalizedTitle
					}
				});
			}
			if (target.restoreText !== undefined) {
				pendingComposerBySession.current.set(fork.session_id, target.restoreText);
			}
			await loadSessions();
			selectSession(fork.session_id);
			pushNotice("success", `forked ${fork.session_id}`);
			return fork.session_id;
		},
		[api, loadSessions, pushNotice, requireSelected, selectedSession, selectSession, snapshot]
	);

	const switchToTarget = useCallback(
		async (target: HistoryTargetOption) => {
			const sessionId = requireSelected();
			const current = await refreshSelected(sessionId);
			if ((current?.snapshot.activity ?? snapshot?.activity) !== "idle") {
				throw new Error("stop the active turn before switching history");
			}
			await api.rewindHistory({
				sessionId,
				leafId: target.actionLeafId,
				expectedActiveLeafId: target.expectedActiveLeafId ?? current?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null
			});
			await refreshSelected(sessionId);
			if (target.restoreText !== undefined) {
				setComposer(target.restoreText);
			}
			void loadSessions().catch(() => undefined);
			pushNotice("success", target.restoreText !== undefined ? "message restored for editing" : "switched to selected history point");
		},
		[api, loadSessions, pushNotice, refreshSelected, requireSelected, snapshot?.active_leaf_id, snapshot?.activity]
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
			const activeLeafId = leafId ?? current?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null;
			if (!activeLeafId) throw new Error("no terminal turn to resume");
			if ((current?.snapshot.activity ?? snapshot?.activity) !== "idle") {
				throw new Error("stop the active turn before retrying");
			}
			setResumingTurnId(activeLeafId);
			try {
				const result = await api.resumeTurn({
					sessionId,
					leafId: activeLeafId,
					expectedActiveLeafId: current?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null
				});
				await Promise.all([refreshSelected(sessionId), loadSessions()]);
				pushNotice("success", result.outcome === "Interrupted" ? "continued turn" : "retry started");
			} finally {
				setResumingTurnId(null);
			}
		},
		[api, loadSessions, pushNotice, refreshSelected, requireSelected, snapshot?.active_leaf_id, snapshot?.activity]
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
				const refreshed = snapshot?.activity === "running" ? null : await refreshSelected(sessionId);
				setHistoryDialog({
					mode: "fork",
					entries: refreshed?.entries ?? entries,
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null,
					initialForkTitle: args
				});
				return;
			}
			if (name === "switch") {
				const refreshed = await refreshSelected(sessionId);
				if ((refreshed?.snapshot.activity ?? snapshot?.activity) !== "idle") {
					throw new Error("stop the active turn before switching history");
				}
				setHistoryDialog({
					mode: "switch",
					entries: refreshed?.entries ?? entries,
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null
				});
				return;
			}
			if (name === "export") {
				const refreshed = await refreshSelected(sessionId);
				setExportDialog({
					entries: branchEntriesFor(
						refreshed?.entries ?? entries,
						refreshed?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null
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
			entries,
			pushNotice,
			refreshSelected,
			requireSelected,
			snapshot
		]
	);

	const sendComposer = useCallback(async () => {
		const text = composer.trim();
		if (!text || sending) return;
		const slash = parseSlash(text);
		if (slash) {
			const command = findCommand(slash.name);
			if (command?.requiresArgs && !slash.args) {
				setComposer(`/${command.name} `);
				pushNotice("info", `usage: /${command.name} ${command.argumentHint ?? "<args>"}`);
				requestAnimationFrame(() => composerRef.current?.focus());
				return;
			}
		}
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
			setComposer("");
			requestAnimationFrame(() => composerRef.current?.focus());
		} catch (error) {
			pushNotice("error", errorMessage(error));
		} finally {
			setSending(false);
		}
	}, [composer, executeSlash, pushNotice, queueUserInput, sending, startNewSession]);

	const onComposerKeyDown = useCallback(
		(event: KeyboardEvent<HTMLTextAreaElement>) => {
			if (slashState.visible && slashState.commands.length > 0) {
				if (event.key === "ArrowDown") {
					event.preventDefault();
					setSlashIndex((index) => (index + 1) % slashState.commands.length);
					return;
				}
				if (event.key === "ArrowUp") {
					event.preventDefault();
					setSlashIndex((index) => (index - 1 + slashState.commands.length) % slashState.commands.length);
					return;
				}
				if (event.key === "Tab") {
					event.preventDefault();
					const command = slashState.commands[Math.min(slashIndex, slashState.commands.length - 1)];
					setComposer(`/${command.name} `);
					return;
				}
				if (event.key === "Enter" && !event.shiftKey) {
					event.preventDefault();
					const command = slashState.commands[Math.min(slashIndex, slashState.commands.length - 1)];
					const typedCommand = matchSlashPrefix(composer) ?? "";
					if (command.name === typedCommand && !command.requiresArgs) {
						void sendComposer();
					} else {
						setComposer(`/${command.name} `);
					}
					return;
				}
			}
			if (event.key === "Enter" && !event.shiftKey) {
				event.preventDefault();
				void sendComposer();
			}
		},
		[composer, sendComposer, slashIndex, slashState.commands, slashState.visible]
	);

	const layoutStyle = {
		gridTemplateColumns: rightOpen ? "280px minmax(0,1fr) minmax(320px,380px)" : "280px minmax(0,1fr)"
	};
	const canStop = !!selectedId && snapshot?.activity === "running";
	const queuedInputs = snapshot?.queued_inputs ?? [];

	return (
		<div className="app-shell" style={layoutStyle}>
			<aside className="sidebar" data-slot="sidebar">
				<SidebarHeader counts={counts} total={activeSessionItems.length} archived={archivedCount} connection={connection} />
				<SidebarToolbar
					query={query}
					onQueryChange={setQuery}
					showArchived={showArchived}
					onToggleArchived={() => setShowArchived((show) => !show)}
					onNew={() => void createSession()}
				/>
				<div className="session-list" role="listbox" aria-label="sessions">
					{filteredSessions.map((session) => (
						<SessionRow
							key={session.session_id}
							session={session}
							selected={session.session_id === selectedId}
							onSelect={() => selectSession(session.session_id)}
							onRename={() => openRenameDialog(session)}
							onArchiveToggle={() => {
								void setSessionArchived(session, !isArchivedSession(session)).catch((error) => pushNotice("error", errorMessage(error)));
							}}
							onDelete={() => setDeleteDialog({ session, deleting: false })}
						/>
					))}
					{filteredSessions.length === 0 ? <div className="empty-list">No sessions</div> : null}
				</div>
			</aside>

			<main className="log-pane" data-slot="agent-log">
				<LogHeader
					session={selectedSession}
					snapshot={snapshot}
					modelOptions={MODEL_OPTIONS}
					modelValue={providerModelKey(activeProvider)}
					modelLocked={modelLocked}
					modelControlsDisabled={modelControlsDisabled}
					reasoningEfforts={reasoningEfforts}
					reasoningEffort={providerReasoningEffort(activeProvider)}
					onModelChange={(value) => {
						void changeModel(value).catch((error) => pushNotice("error", errorMessage(error)));
					}}
					onReasoningEffortChange={(value) => {
						void changeReasoningEffort(value).catch((error) => pushNotice("error", errorMessage(error)));
					}}
					rightOpen={rightOpen}
					onToggleRight={() => setRightOpen((open) => !open)}
				/>
				<MessageList
					entries={entries}
					activeLeafId={snapshot?.active_leaf_id ?? null}
					isRunning={snapshot?.activity === "running"}
					hasSession={!!selectedId}
					onResumeTurn={(entryId) => {
						void resumeTerminalTurn(entryId).catch((error) => pushNotice("error", errorMessage(error)));
					}}
					resumingTurnId={resumingTurnId}
				/>
			</main>

			<footer className="chat-dock" data-slot="chat-box">
				<Composer
					value={composer}
					selectedId={selectedId}
					textAreaRef={composerRef}
					sending={sending}
					canStop={canStop}
					stopping={stopping}
					slashCommands={slashState.commands}
					slashVisible={slashState.visible}
					slashIndex={slashIndex}
					queuedInputs={queuedInputs}
					onChange={setComposer}
					onKeyDown={onComposerKeyDown}
					onSend={() => void sendComposer()}
					onStop={() => void stopActiveTurn()}
					onSetSlashIndex={setSlashIndex}
					onSelectSlash={(command) => setComposer(`/${command.name} `)}
					onPromoteQueued={(inputId) => {
						void promoteQueuedInput(inputId).catch((error) => pushNotice("error", errorMessage(error)));
					}}
				/>
			</footer>

			{rightOpen ? (
				<aside className="inspector" data-slot="inspector">
					<Inspector snapshot={snapshot} config={config} tools={tools} />
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

function compareSessionsForSidebar(left: SessionListItem, right: SessionListItem): number {
	const archivedDelta = Number(isArchivedSession(left)) - Number(isArchivedSession(right));
	if (archivedDelta !== 0) return archivedDelta;
	return Date.parse(right.updated_at) - Date.parse(left.updated_at);
}

function mergeSnapshotIntoSessionList(sessions: SessionSummary[], snapshot: SessionSnapshot): SessionSummary[] {
	let found = false;
	const nextSessions = sessions.map((session) => {
		if (session.session_id !== snapshot.session_id) return session;
		found = true;
		return {
			...session,
			activity: snapshot.activity,
			active_leaf_id: snapshot.active_leaf_id,
			provider: snapshot.provider,
			metadata: snapshot.metadata
		};
	});
	return found ? nextSessions : sessions;
}

function titleFromText(text: string): string {
	return truncate(firstLine(text).trim() || "New session", 64);
}

function isTerminalActivityEvent(event: string): boolean {
	return (
		event === "session.idle" ||
		event === "model.completed" ||
		event === "model.error" ||
		event === "tool.completed" ||
		event === "tool.error" ||
		event === "compaction.completed" ||
		event === "compaction.error"
	);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function applyQueuedInputEvent(
	event: EventFrame,
	setSnapshot: Dispatch<SetStateAction<SessionSnapshot | null>>
) {
	if (event.event !== "input.consumed" && event.event !== "input.promoted") return;
	const inputId = typeof event.data.input_id === "string" ? event.data.input_id : null;
	if (!inputId) return;
	setSnapshot((current) => {
		if (!current || current.session_id !== event.session_id) return current;
		if (event.event === "input.consumed") {
			const queuedInputs = current.queued_inputs.filter((input) => input.input_id !== inputId);
			return queuedInputs.length === current.queued_inputs.length ? current : { ...current, queued_inputs: queuedInputs };
		}
		const promotedAt = typeof event.data.promoted_at === "string" ? event.data.promoted_at : null;
		let changed = false;
		const queuedInputs = current.queued_inputs.map((input) => {
			if (input.input_id !== inputId) return input;
			changed = true;
			return { ...input, priority: "steer" as const, status: "queued" as const, promoted_at: promotedAt };
		});
		return changed ? { ...current, queued_inputs: queuedInputs } : current;
	});
}

function modelErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "model request failed";
	return `model error: ${truncate(error, 420)}`;
}
