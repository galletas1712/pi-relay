import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import { createAgentApi } from "./agentApi.ts";
import { Composer } from "./composer.tsx";
import { HistoryPickerDialog } from "./historyPicker.tsx";
import type { HistoryTargetOption } from "./historyTargets.ts";
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
import { DEFAULT_PROVIDER, textContent } from "./sessionDefaults.ts";
import { sessionTitle, tallyActivities, type SessionListItem } from "./sessionList.ts";
import { firstLine, truncate } from "./text.ts";
import { MessageList } from "./transcript.tsx";
import type {
	DaemonConfig,
	EventFrame,
	Notice,
	SessionSnapshot,
	SessionSummary,
	ToolDefinition,
	TranscriptEntry
} from "./types.ts";

const MAX_NOTICES = 24;
const NOTICE_TTL_MS = 4000;
const OLD_DRAFT_STORAGE_KEYS = ["pi-relay.web.draft-sessions.v1", "pi-relay.web.composer-drafts.v1"];

type HistoryDialogState = {
	mode: "fork" | "switch";
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	initialForkTitle?: string;
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
	const [tools, setTools] = useState<ToolDefinition[]>([]);
	const [query, setQuery] = useState("");
	const [composer, setComposer] = useState("");
	const [slashIndex, setSlashIndex] = useState(0);
	const [sending, setSending] = useState(false);
	const [stopping, setStopping] = useState(false);
	const [loading, setLoading] = useState(true);
	const [rightOpen, setRightOpen] = useState(true);
	const [historyDialog, setHistoryDialog] = useState<HistoryDialogState | null>(null);
	const [renameSessionId, setRenameSessionId] = useState<string | null>(null);
	const [renameValue, setRenameValue] = useState("");

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
		const [nextConfig, nextTools] = await Promise.all([api.getConfig(), api.listTools()]);
		setConfig(nextConfig);
		setTools(nextTools);
	}, [api]);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current) => {
			if (!sessionId) return null;
			const nextSnapshot = await api.getSession(sessionId, { includeEntries: true });
			if (selectedRef.current !== sessionId) return null;
			lastEventIds.current.set(sessionId, nextSnapshot.last_event_id);
			setSnapshot(nextSnapshot);
			setEntries(nextSnapshot.entries ?? []);
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
				if (event.event === "model.error") pushNotice("error", modelErrorNotice(event.data));
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
				const [loadedSessions] = await Promise.all([loadSessions(), loadGlobal()]);
				if (!selectedRef.current && loadedSessions[0]) {
					selectSession(loadedSessions[0].session_id);
				}
			})
			.catch((error) => pushNotice("error", errorMessage(error)))
			.finally(() => setLoading(false));
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
		() => sessions,
		[sessions]
	);

	const selectedSession = useMemo(
		() => sessionItems.find((session) => session.session_id === selectedId) ?? null,
		[sessionItems, selectedId]
	);

	const filteredSessions = useMemo(() => {
		const q = query.trim().toLowerCase();
		if (!q) return sessionItems;
		return sessionItems.filter((session) => {
			const title = sessionTitle(session).toLowerCase();
			return title.includes(q) || session.session_id.toLowerCase().includes(q);
		});
	}, [query, sessionItems]);

	const counts = useMemo(() => tallyActivities(sessionItems), [sessionItems]);

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
		await api.renameSession(renameSessionId, title || null);
		await Promise.all([loadSessions(), renameSessionId === selectedRef.current ? refreshSelected(renameSessionId) : Promise.resolve(null)]);
		pushNotice("success", title ? `renamed session to “${truncate(title, 80)}”` : "cleared session title");
		closeRenameDialog();
	}, [api, closeRenameDialog, loadSessions, pushNotice, refreshSelected, renameSessionId, renameValue]);

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
			const clientInputId = randomId("web_input");
			await api.queueFollowUp({
				sessionId,
				clientInputId,
				expectedActiveLeafId: snapshot?.activity === "idle" ? (snapshot.active_leaf_id ?? null) : undefined,
				content: textContent(text)
			});
		},
		[api, requireSelected, snapshot?.active_leaf_id, snapshot?.activity]
	);

	const startNewSession = useCallback(
		async (text: string) => {
			const sessionId = randomId("session");
			const title = nextSessionTitleRef.current || titleFromText(text);
			nextSessionTitleRef.current = null;
			const result = await api.startSession({
				sessionId,
				provider: DEFAULT_PROVIDER,
				metadata: { title, created_by: "web" },
				clientInputId: randomId("web_start"),
				priority: "follow_up",
				content: textContent(text)
			});
			await loadSessions();
			selectSession(result.session_id);
			return result.session_id;
		},
		[api, loadSessions, selectSession]
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
			await api.promoteQueuedInput(sessionId, inputId);
			await Promise.all([refreshSelected(sessionId), loadSessions()]);
		},
		[api, loadSessions, refreshSelected, requireSelected]
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
			if (name === "new") {
				await createSession(args);
				pushActionNotice("success", args ? `created session “${truncate(args, 80)}”` : "created session");
				return;
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
			if (name === "compact") {
				const result = await api.requestCompaction(sessionId);
				pushActionNotice("success", `compaction requested ${result.action_row_id ?? ""}`.trim());
				return;
			}
			if (name === "rename") {
				if (!args) {
					const session = selectedSession ?? sessionItems.find((item) => item.session_id === sessionId);
					if (session) openRenameDialog(session);
					return;
				}
				const title = args === "clear" ? null : args;
				await api.renameSession(sessionId, title);
				await Promise.all([loadSessions(), refreshSelected(sessionId)]);
				pushActionNotice("success", title ? `renamed session to “${truncate(title, 80)}”` : "cleared session title");
				return;
			}
			if (name === "provider") {
				if (!args) {
					const provider = snapshot?.provider ?? selectedSession?.provider;
					pushActionNotice("info", provider ? `${provider.kind} ${provider.model}` : "no provider loaded");
					return;
				}
				const [kind, model] = args.split(/\s+/).filter(Boolean);
				if (!kind || !model) throw new Error("usage: /provider <kind> <model>");
				const previous = snapshot?.provider ?? selectedSession?.provider ?? DEFAULT_PROVIDER;
				const provider = { ...previous, kind, model };
				await api.configureSession({
					sessionId,
					provider
				});
				await Promise.all([loadSessions(), refreshSelected(sessionId)]);
				pushActionNotice("success", `provider set to ${kind} ${model}`);
				return;
			}
			throw new Error(`unknown command: /${name}`);
		},
		[
			api,
			createSession,
			entries,
			loadGlobal,
			loadSessions,
			openRenameDialog,
			pushNotice,
			refreshSelected,
			requireSelected,
			selectedSession,
			sessionItems,
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
				<SidebarHeader counts={counts} total={sessionItems.length} connection={connection} />
				<SidebarToolbar
					query={query}
					onQueryChange={setQuery}
					onNew={() => void createSession()}
					onRefresh={() => void Promise.all([loadSessions(), loadGlobal(), refreshSelected()])}
					loading={loading}
				/>
				<div className="session-list" role="listbox" aria-label="sessions">
					{filteredSessions.map((session) => (
						<SessionRow
							key={session.session_id}
							session={session}
							selected={session.session_id === selectedId}
							onSelect={() => selectSession(session.session_id)}
							onRename={() => openRenameDialog(session)}
						/>
					))}
					{filteredSessions.length === 0 ? <div className="empty-list">No sessions</div> : null}
				</div>
			</aside>

			<main className="log-pane" data-slot="agent-log">
				<LogHeader
					session={selectedSession}
					snapshot={snapshot}
					rightOpen={rightOpen}
					onToggleRight={() => setRightOpen((open) => !open)}
				/>
				<MessageList
					entries={entries}
					activeLeafId={snapshot?.active_leaf_id ?? null}
					isRunning={snapshot?.activity === "running"}
					hasSession={!!selectedId}
					notices={notices}
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
			<div className="history-dialog" role="dialog" aria-modal="true" aria-labelledby="rename-dialog-title" onMouseDown={(event) => event.stopPropagation()}>
				<div className="history-dialog-head">
					<div className="history-dialog-copy">
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
					<label className="history-title-field">
						<span>Session title</span>
						<input value={value} onChange={(event) => onChange(event.target.value)} autoFocus placeholder="Empty clears custom title" />
					</label>
					<div className="history-actions">
						<button type="button" className="secondary-button" onClick={onClose}>Cancel</button>
						<button type="submit" className="primary-button">Save</button>
					</div>
				</form>
			</div>
		</div>
	);
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

function modelErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "model request failed";
	return `model error: ${truncate(error, 420)}`;
}
