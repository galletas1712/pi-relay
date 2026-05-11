import { useCallback, useEffect, useMemo, useRef, useState, type KeyboardEvent } from "react";
import {
	AlertTriangle,
	Check,
	ChevronDown,
	Copy,
	GitFork,
	Loader2,
	MoveUp,
	PanelRightClose,
	PanelRightOpen,
	Plus,
	RefreshCw,
	RotateCcw,
	Search,
	Send,
	Settings,
	Square,
	Terminal,
	Wrench,
	X
} from "lucide-react";
import rehypeRaw from "rehype-raw";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import { AgentRpcClient, defaultWsUrl } from "./rpc.ts";
import { COMMANDS, filterCommands, findCommand, parseSlash, type ParsedSlash } from "./slash.ts";
import type {
	Activity,
	ContentBlock,
	DaemonConfig,
	EventFrame,
	HistoryTree,
	Notice,
	ProviderConfig,
	QueuedInput,
	SessionSnapshot,
	SessionSummary,
	ToolDefinition,
	TranscriptEntry,
	TranscriptItem
} from "./types.ts";

const DEFAULT_PROVIDER: ProviderConfig = {
	kind: "codex",
	model: "gpt-5.5",
	prompt_cache: { key: "pi-relay-web" }
};

const MAX_NOTICES = 24;
const NOTICE_TTL_MS = 4000;
const DRAFT_SESSIONS_KEY = "pi-relay.web.draft-sessions.v1";
const COMPOSER_DRAFTS_KEY = "pi-relay.web.composer-drafts.v1";

type ToolResultItem = Extract<TranscriptItem, { type: "tool_result" }>;
type HistoryDialogState = {
	mode: "rewind" | "fork";
	entries: TranscriptEntry[];
	activeLeafId: string | null;
};
type DurableComposerSource = "manual" | "rewind" | "fork";
type HistoryPlacement = "at" | "before";

interface DraftSession {
	draft_id: string;
	session_id: string;
	title: string;
	provider: ProviderConfig;
	composer: string;
	created_at: number;
	updated_at: number;
}

interface DurableComposerDraft {
	session_id: string;
	text: string;
	content: ContentBlock[];
	source: DurableComposerSource;
	source_session_id?: string;
	source_entry_id?: string;
	base_active_leaf_id: string | null;
	updated_at: number;
}

type SessionListItem =
	| (SessionSummary & { local?: false })
	| {
			local: true;
			session_id: string;
			draft_id: string;
			activity: Activity;
			active_leaf_id: null;
			provider: ProviderConfig;
			metadata: Record<string, unknown>;
			updated_at: string;
	  };

interface HistoryTargetOption {
	id: string | null;
	actionLeafId: string | null;
	expectedActiveLeafId?: string | null;
	sourceEntryId?: string;
	placement?: HistoryPlacement;
	restoreText?: string;
	turnLabel: string;
	title: string;
	preview: string;
	meta: string;
	isActive: boolean;
}

export function App() {
	const rpc = useMemo(() => new AgentRpcClient(defaultWsUrl()), []);
	const [connection, setConnection] = useState<"connecting" | "open" | "closed" | "error">(
		"connecting"
	);
	const [sessions, setSessions] = useState<SessionSummary[]>([]);
	const [draftSessions, setDraftSessions] = useState<DraftSession[]>(() => loadDraftSessions());
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
	const [forkTitleDraft, setForkTitleDraft] = useState("");

	const refreshTimer = useRef<number | null>(null);
	const composerRef = useRef<HTMLTextAreaElement | null>(null);
	const pendingInputIds = useRef(new Map<string, string>());
	const composerHydrating = useRef(false);
	const draftSessionsRef = useRef(draftSessions);
	const composerValueRef = useRef("");

	const persistDraftSessions = useCallback((next: DraftSession[]) => {
		draftSessionsRef.current = next;
		setDraftSessions(next);
		saveDraftSessions(next);
	}, []);

	const removeEmptyDraftOnLeave = useCallback(
		(draftId: string | null, nextSelectedId: string | null) => {
			if (!draftId || draftId === nextSelectedId || !isDraftSessionId(draftId)) return;
			const storedDraft = draftSessionsRef.current.find((draft) => draft.draft_id === draftId);
			const text = selectedRef.current === draftId ? composerValueRef.current : (storedDraft?.composer ?? "");
			if (text.trim()) return;
			const next = draftSessionsRef.current.filter((draft) => draft.draft_id !== draftId);
			if (next.length === draftSessionsRef.current.length) return;
			persistDraftSessions(next);
		},
		[persistDraftSessions]
	);

	const selectSession = useCallback(
		(sessionId: string | null) => {
			removeEmptyDraftOnLeave(selectedRef.current, sessionId);
			selectedRef.current = sessionId;
			setSelectedId(sessionId);
		},
		[removeEmptyDraftOnLeave]
	);

	useEffect(() => {
		draftSessionsRef.current = draftSessions;
	}, [draftSessions]);

	useEffect(() => {
		composerValueRef.current = composer;
	}, [composer]);

	useEffect(() => {
		const collapsed = collapseEmptyDraftSessions(draftSessionsRef.current, selectedRef.current);
		if (collapsed.length !== draftSessionsRef.current.length) {
			persistDraftSessions(collapsed);
		}
	}, [persistDraftSessions]);

	useEffect(() => {
		selectedRef.current = selectedId;
	}, [selectedId]);

	const pushNotice = useCallback((tone: Notice["tone"], text: string) => {
		setNotices((current) => [
			...current.slice(Math.max(0, current.length - MAX_NOTICES + 1)),
			{ id: `notice_${Date.now()}_${Math.random().toString(16).slice(2)}`, tone, text }
		]);
	}, []);

	useEffect(() => {
		if (notices.length === 0) return;
		const timer = window.setTimeout(() => {
			setNotices((current) => current.slice(1));
		}, NOTICE_TTL_MS);
		return () => window.clearTimeout(timer);
	}, [notices]);

	const loadSessions = useCallback(async () => {
		const result = await rpc.request<{ sessions: SessionSummary[] }>("session.list", { limit: 100 });
		setSessions(result.sessions);
		return result.sessions;
	}, [rpc]);

	const upsertDraftSession = useCallback(
		(draft: DraftSession) => {
			persistDraftSessions([
				draft,
				...draftSessionsRef.current.filter((current) => current.draft_id !== draft.draft_id)
			]);
		},
		[persistDraftSessions]
	);

	const removeDraftSession = useCallback(
		(draftId: string) => {
			persistDraftSessions(draftSessionsRef.current.filter((draft) => draft.draft_id !== draftId));
		},
		[persistDraftSessions]
	);

	const loadGlobal = useCallback(async () => {
		const [nextConfig, toolResult] = await Promise.all([
			rpc.request<DaemonConfig>("config.get"),
			rpc.request<{ tools: ToolDefinition[] }>("tools.list")
		]);
		setConfig(nextConfig);
		setTools(toolResult.tools);
	}, [rpc]);

	const refreshSelected = useCallback(
		async (sessionId = selectedRef.current) => {
			if (!sessionId) return null;
			if (isDraftSessionId(sessionId)) {
				if (selectedRef.current === sessionId) {
					setSnapshot(null);
					setEntries([]);
				}
				return null;
			}
			const [nextSnapshot, tree] = await Promise.all([
				rpc.request<SessionSnapshot>("session.get", { session_id: sessionId }),
				rpc.request<HistoryTree>("history.tree", { session_id: sessionId })
			]);
			if (selectedRef.current !== sessionId) return null;
			setSnapshot(nextSnapshot);
			setEntries(tree.entries);
			return { snapshot: nextSnapshot, entries: tree.entries };
		},
		[rpc]
	);

	const scheduleSelectedRefresh = useCallback(() => {
		if (refreshTimer.current !== null) window.clearTimeout(refreshTimer.current);
		refreshTimer.current = window.setTimeout(() => {
			refreshTimer.current = null;
			void refreshSelected().catch((error) => pushNotice("error", errorMessage(error)));
			void loadSessions().catch(() => undefined);
		}, 80);
	}, [loadSessions, pushNotice, refreshSelected]);

	useEffect(() => {
		const offStatus = rpc.onStatus((status) => {
			setConnection(status);
			if (status !== "open") return;
			const sessionId = selectedRef.current;
			if (!sessionId) return;
			if (isDraftSessionId(sessionId)) return;
			void rpc
				.request("events.subscribe", { session_id: sessionId, after_event_id: null })
				.then(() => refreshSelected(sessionId))
				.catch((error) => pushNotice("error", errorMessage(error)));
		});
		const offEvent = rpc.onEvent((event: EventFrame) => {
			if (event.session_id === selectedRef.current) {
				if (event.event === "model.error") pushNotice("error", modelErrorNotice(event.data));
				scheduleSelectedRefresh();
			}
			if (event.event === "session.created" || event.event === "history.forked") {
				void loadSessions().catch(() => undefined);
			}
		});
		void rpc
			.connect()
			.then(async () => {
				const [loadedSessions] = await Promise.all([loadSessions(), loadGlobal()]);
				const firstDraft = loadDraftSessions()[0];
				if (!selectedRef.current && firstDraft) {
					selectSession(firstDraft.draft_id);
				} else if (!selectedRef.current && loadedSessions[0]) {
					selectSession(loadedSessions[0].session_id);
				}
			})
			.catch((error) => pushNotice("error", errorMessage(error)))
			.finally(() => setLoading(false));
		return () => {
			offStatus();
			offEvent();
			rpc.close();
		};
	}, [loadGlobal, loadSessions, pushNotice, refreshSelected, rpc, scheduleSelectedRefresh, selectSession]);

	useEffect(() => {
		if (!selectedId) {
			setSnapshot(null);
			setEntries([]);
			setComposer("");
			return;
		}
		composerHydrating.current = true;
		if (isDraftSessionId(selectedId)) {
			const draft = draftSessions.find((item) => item.draft_id === selectedId);
			setSnapshot(null);
			setEntries([]);
			setComposer(draft?.composer ?? "");
			requestAnimationFrame(() => {
				composerHydrating.current = false;
			});
			return;
		}
		const durableDraft = loadComposerDrafts()[selectedId];
		setComposer(durableDraft?.text ?? "");
		requestAnimationFrame(() => {
			composerHydrating.current = false;
		});
		let cancelled = false;
		void rpc
			.request<{ replayed: EventFrame[] }>("events.subscribe", {
				session_id: selectedId,
				after_event_id: null
			})
			.then(() => {
				if (!cancelled) return refreshSelected(selectedId);
			})
			.catch((error) => {
				if (!cancelled) pushNotice("error", errorMessage(error));
			});
		return () => {
			cancelled = true;
			if (rpc.isOpen()) {
				void rpc.request("events.unsubscribe", { session_id: selectedId }).catch(() => undefined);
			}
		};
	}, [draftSessions, pushNotice, refreshSelected, rpc, selectedId]);

	const draftItems = useMemo<SessionListItem[]>(
		() =>
			draftSessions.map((draft) => ({
				local: true,
				session_id: draft.draft_id,
				draft_id: draft.draft_id,
				activity: "idle",
				active_leaf_id: null,
				provider: draft.provider,
				metadata: { title: draft.title, local_draft: true },
				updated_at: new Date(draft.updated_at).toISOString()
			})),
		[draftSessions]
	);

	const sessionItems = useMemo<SessionListItem[]>(
		() => [...draftItems, ...sessions.map((session) => ({ ...session, local: false as const }))],
		[draftItems, sessions]
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

	useEffect(() => {
		if (!selectedId || composerHydrating.current) return;
		if (isDraftSessionId(selectedId)) {
			if (composer.trim().startsWith("/")) return;
			const draft = draftSessions.find((item) => item.draft_id === selectedId);
			if (!draft || draft.composer === composer) return;
			upsertDraftSession({ ...draft, composer, updated_at: Date.now() });
			return;
		}
		if (composer.trim().startsWith("/")) return;
		if (composer.trim()) {
			const existing = loadComposerDrafts()[selectedId];
			upsertComposerDraft({
				session_id: selectedId,
				text: composer,
				content: [{ type: "text", text: composer }],
				source: "manual",
				base_active_leaf_id: existing?.base_active_leaf_id ?? snapshot?.active_leaf_id ?? null,
				updated_at: Date.now()
			});
		} else {
			deleteComposerDraft(selectedId);
		}
	}, [composer, draftSessions, selectedId, snapshot?.active_leaf_id, upsertDraftSession]);

	const slashState = useMemo<{ visible: boolean; commands: typeof COMMANDS }>(() => {
		const match = composer.match(/^\/(\S*)$/);
		if (!match) return { visible: false, commands: [] };
		return { visible: true, commands: filterCommands(match[1] ?? "") };
	}, [composer]);

	useEffect(() => {
		setSlashIndex(0);
	}, [slashState.commands, slashState.visible]);

	const createSession = useCallback(
		(title?: string) => {
			const now = Date.now();
			const draft: DraftSession = {
				draft_id: `draft_${randomToken()}`,
				session_id: `session_${randomToken()}`,
				title: title?.trim() || "New session",
				provider: DEFAULT_PROVIDER,
				composer: "",
				created_at: now,
				updated_at: now
			};
			persistDraftSessions(
				collapseEmptyDraftSessions(
					[
						draft,
						...draftSessionsRef.current.filter((current) => current.draft_id !== draft.draft_id)
					],
					draft.draft_id
				)
			);
			selectSession(draft.draft_id);
			return draft.draft_id;
		},
		[persistDraftSessions, selectSession]
	);

	const requireSelected = useCallback(() => {
		if (!selectedRef.current) throw new Error("select a session first");
		return selectedRef.current;
	}, []);

	const clientInputIdFor = useCallback((text: string) => {
		const key = `input.follow_up\0${text}`;
		const existing = pendingInputIds.current.get(key);
		if (existing) return existing;
		const next = `web_${Date.now()}_${Math.random().toString(16).slice(2)}`;
		pendingInputIds.current.set(key, next);
		return next;
	}, []);

	const forgetClientInputId = useCallback((text: string) => {
		pendingInputIds.current.delete(`input.follow_up\0${text}`);
	}, []);

	const queueUserInput = useCallback(
		async (text: string) => {
			const sessionId = requireSelected();
			if (isDraftSessionId(sessionId)) throw new Error("send the draft first before queueing another message");
			const clientInputId = clientInputIdFor(text);
			const draft = loadComposerDrafts()[sessionId];
			await rpc.request("input.follow_up", {
				session_id: sessionId,
				client_input_id: clientInputId,
				expected_active_leaf_id: draft?.base_active_leaf_id,
				content: [{ type: "text", text }]
			});
			forgetClientInputId(text);
		},
		[clientInputIdFor, forgetClientInputId, requireSelected, rpc]
	);

	const startDraftSession = useCallback(
		async (draft: DraftSession, text: string) => {
			const clientInputId = `web_start_${draft.draft_id}`;
			const result = await rpc.request<{ session_id: string; activity: Activity; replayed?: boolean }>("session.start", {
				session_id: draft.session_id,
				provider: draft.provider,
				metadata: { title: draft.title, created_by: "web" },
				client_input_id: clientInputId,
				priority: "follow_up",
				content: [{ type: "text", text }]
			});
			removeDraftSession(draft.draft_id);
			await loadSessions();
			selectSession(result.session_id);
			return result.session_id;
		},
		[loadSessions, removeDraftSession, rpc, selectSession]
	);

	const rewindToTarget = useCallback(
		async (target: HistoryTargetOption) => {
			const sessionId = requireSelected();
			let expectedActiveLeafId = target.expectedActiveLeafId ?? snapshot?.active_leaf_id ?? null;
			const current = await refreshSelected(sessionId);
			if ((current?.snapshot.activity ?? snapshot?.activity) !== "idle") {
				await rpc.request("input.interrupt", { session_id: sessionId });
				const idleSnapshot = await waitForIdleSession(rpc, sessionId);
				expectedActiveLeafId = idleSnapshot.active_leaf_id;
			}
			await rpc.request("history.rewind", {
				session_id: sessionId,
				leaf_id: target.actionLeafId,
				expected_active_leaf_id: expectedActiveLeafId
			});
			await refreshSelected(sessionId);
			if (target.restoreText !== undefined) {
				upsertComposerDraft({
					session_id: sessionId,
					text: target.restoreText,
					content: [{ type: "text", text: target.restoreText }],
					source: "rewind",
					source_entry_id: target.sourceEntryId,
					base_active_leaf_id: target.actionLeafId,
					updated_at: Date.now()
				});
				setComposer(target.restoreText);
			}
			void loadSessions().catch(() => undefined);
			pushNotice("success", target.restoreText !== undefined ? "message restored for editing" : "rewound to selected point");
		},
		[loadSessions, pushNotice, refreshSelected, requireSelected, rpc, snapshot?.active_leaf_id, snapshot?.activity]
	);

	const forkFromTarget = useCallback(
		async (target: HistoryTargetOption, title?: string) => {
			const sessionId = requireSelected();
			const fork = await rpc.request<{ session_id: string }>("history.fork", {
				session_id: sessionId,
				leaf_id: target.sourceEntryId ?? target.id,
				placement: target.placement ?? "at"
			});
			const normalizedTitle = title?.trim();
			if (normalizedTitle) {
				await rpc.request("session.configure", {
					session_id: fork.session_id,
					provider: snapshot?.provider ?? selectedSession?.provider ?? DEFAULT_PROVIDER,
					metadata: {
						...(snapshot?.metadata ?? selectedSession?.metadata ?? {}),
						title: normalizedTitle
					}
				});
			}
			if (target.restoreText !== undefined) {
				upsertComposerDraft({
					session_id: fork.session_id,
					text: target.restoreText,
					content: [{ type: "text", text: target.restoreText }],
					source: "fork",
					source_session_id: sessionId,
					source_entry_id: target.sourceEntryId,
					base_active_leaf_id: target.actionLeafId,
					updated_at: Date.now()
				});
			}
			await loadSessions();
			selectSession(fork.session_id);
			pushNotice("success", `forked ${fork.session_id}`);
			return fork.session_id;
		},
		[loadSessions, pushNotice, requireSelected, rpc, selectedSession, selectSession, snapshot]
	);

	const promoteQueuedInput = useCallback(
		async (inputId: string) => {
			const sessionId = requireSelected();
			await rpc.request("input.promote_queued", {
				session_id: sessionId,
				input_id: inputId
			});
			await Promise.all([refreshSelected(sessionId), loadSessions()]);
		},
		[loadSessions, refreshSelected, requireSelected, rpc]
	);

	const stopActiveTurn = useCallback(async () => {
		const sessionId = requireSelected();
		if (isDraftSessionId(sessionId)) return;
		setStopping(true);
		try {
			await rpc.request("input.interrupt", { session_id: sessionId });
			await Promise.all([refreshSelected(sessionId), loadSessions()]);
		} catch (error) {
			pushNotice("error", errorMessage(error));
		} finally {
			setStopping(false);
		}
	}, [loadSessions, pushNotice, refreshSelected, requireSelected, rpc]);

	const executeSlash = useCallback(
		async (parsed: ParsedSlash) => {
			const name = parsed.name;
			const args = parsed.args;
			if (!name || name === "help") {
				pushNotice("info", `commands: ${COMMANDS.map((command) => `/${command.name}`).join(", ")}`);
				return;
			}
			if (name === "clear") {
				setNotices([]);
				return;
			}
			if (name === "new") {
				await createSession(args);
				return;
			}
			if (name === "refresh") {
				await Promise.all([loadSessions(), loadGlobal(), refreshSelected()]);
				pushNotice("success", "refreshed");
				return;
			}
			if (name === "tools") {
				const result = await rpc.request<{ tools: ToolDefinition[] }>("tools.list");
				setTools(result.tools);
				pushNotice("info", `tools: ${result.tools.map((tool) => tool.name).join(", ") || "(none)"}`);
				return;
			}
			if (name === "system") {
				if (!args) {
					const next = await rpc.request<DaemonConfig>("config.get");
					setConfig(next);
					pushNotice("info", next.system_prompt ? `system: ${truncate(next.system_prompt, 320)}` : "system prompt is empty");
					return;
				}
				const systemPrompt = args === "clear" ? null : args;
				const next = await rpc.request<DaemonConfig>("config.set", { system_prompt: systemPrompt });
				setConfig(next);
				pushNotice("success", systemPrompt ? "global system prompt updated" : "global system prompt cleared");
				return;
			}

			const sessionId = requireSelected();
			if (name === "status") {
				const next = await rpc.request<SessionSnapshot>("session.get", { session_id: sessionId });
				setSnapshot(next);
				pushNotice("info", `${next.activity}; leaf ${next.active_leaf_id ?? "root"}; ${next.pending_actions.length} pending`);
				return;
			}
			if (name === "rewind") {
				if (args) pushNotice("info", "rewind uses the picker; choose a turn in the dialog");
				const refreshed = await refreshSelected(sessionId);
				setHistoryDialog({
					mode: "rewind",
					entries: refreshed?.entries ?? entries,
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null
				});
				return;
			}
			if (name === "fork") {
				setForkTitleDraft(args);
				const refreshed = await refreshSelected(sessionId);
				setHistoryDialog({
					mode: "fork",
					entries: refreshed?.entries ?? entries,
					activeLeafId: refreshed?.snapshot.active_leaf_id ?? snapshot?.active_leaf_id ?? null
				});
				return;
			}
			if (name === "compact") {
				const result = await rpc.request<{ action_row_id: string | null }>("compaction.request", {
					session_id: sessionId
				});
				pushNotice("success", `compaction requested ${result.action_row_id ?? ""}`.trim());
				return;
			}
			if (name === "context") {
				const result = await rpc.request<{ items: TranscriptItem[] }>("history.context", {
					session_id: sessionId,
					leaf_id: args || undefined
				});
				pushNotice("info", summarizeContext(result.items));
				return;
			}
			if (name === "tree") {
				const tree = await rpc.request<HistoryTree>("history.tree", { session_id: sessionId });
				setEntries(tree.entries);
				pushNotice("info", summarizeTree(tree));
				return;
			}
			if (name === "provider") {
				if (!args) {
					const provider = snapshot?.provider ?? selectedSession?.provider;
					pushNotice("info", provider ? `${provider.kind} ${provider.model}` : "no provider loaded");
					return;
				}
				const [kind, model] = args.split(/\s+/).filter(Boolean);
				if (!kind || !model) throw new Error("usage: /provider <kind> <model>");
				const previous = snapshot?.provider ?? selectedSession?.provider ?? DEFAULT_PROVIDER;
				const provider = { ...previous, kind, model };
				await rpc.request("session.configure", {
					session_id: sessionId,
					provider
				});
				await Promise.all([loadSessions(), refreshSelected(sessionId)]);
				pushNotice("success", `provider set to ${kind} ${model}`);
				return;
			}
			throw new Error(`unknown command: /${name}`);
		},
		[
			createSession,
			forkFromTarget,
			entries,
			loadGlobal,
			loadSessions,
			pushNotice,
			refreshSelected,
			requireSelected,
			rewindToTarget,
			rpc,
			selectedSession,
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
				const selected = selectedRef.current;
				const draft = selected
					? draftSessions.find((item) => item.draft_id === selected)
					: undefined;
				if (draft) {
					await startDraftSession(draft, text);
				} else {
					await queueUserInput(text);
					if (selected) deleteComposerDraft(selected);
				}
			}
			setComposer("");
			requestAnimationFrame(() => composerRef.current?.focus());
		} catch (error) {
			pushNotice("error", errorMessage(error));
		} finally {
			setSending(false);
		}
	}, [composer, draftSessions, executeSlash, pushNotice, queueUserInput, sending, startDraftSession]);

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
					const typedCommand = composer.match(/^\/(\S*)$/)?.[1]?.toLowerCase() ?? "";
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
	const canStop = !!selectedId && !isDraftSessionId(selectedId) && snapshot?.activity === "running";
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
				<MessageList entries={entries} snapshot={snapshot} />
			</main>

			<footer className="chat-dock" data-slot="chat-box">
				<div className="composer-wrap">
					<SlashMenu
						commands={slashState.commands}
						visible={slashState.visible}
						selectedIndex={slashIndex}
						onSetIndex={setSlashIndex}
						onSelect={(command) => setComposer(`/${command.name} `)}
					/>
					<QueuedInputPane
						inputs={queuedInputs}
						visible={queuedInputs.length > 0 && !slashState.visible}
						onPromote={(inputId) => {
							void promoteQueuedInput(inputId).catch((error) => pushNotice("error", errorMessage(error)));
						}}
					/>
					<textarea
						ref={composerRef}
						value={composer}
						onChange={(event) => {
							composerValueRef.current = event.target.value;
							setComposer(event.target.value);
						}}
						onKeyDown={onComposerKeyDown}
						placeholder={selectedId ? "Message the session or type /" : "Create or select a session"}
						className="composer"
						rows={1}
					/>
					<button
						className="stop-button"
						type="button"
						onClick={() => void stopActiveTurn()}
						disabled={!canStop || stopping}
						title="stop active turn"
						aria-label="stop active turn"
					>
						{stopping ? <Loader2 className="spin" size={15} /> : <Square size={14} />}
					</button>
					<button className="send-button" type="button" onClick={() => void sendComposer()} disabled={sending || !composer.trim()}>
						{sending ? <Loader2 className="spin" size={16} /> : <Send size={16} />}
					</button>
				</div>
			</footer>

			{rightOpen ? (
				<aside className="inspector" data-slot="inspector">
					<Inspector snapshot={snapshot} config={config} tools={tools} />
				</aside>
			) : null}

			{historyDialog ? (
				<HistoryPickerDialog
					mode={historyDialog.mode}
					entries={historyDialog.entries}
					activeLeafId={historyDialog.activeLeafId}
					forkTitle={forkTitleDraft}
					onForkTitleChange={setForkTitleDraft}
					onClose={() => setHistoryDialog(null)}
					onRewind={(target) => {
						void rewindToTarget(target)
							.then(() => setHistoryDialog(null))
							.catch((error) => pushNotice("error", errorMessage(error)));
					}}
					onFork={(target, title) => {
						void forkFromTarget(target, title)
							.then(() => setHistoryDialog(null))
							.catch((error) => pushNotice("error", errorMessage(error)));
					}}
				/>
			) : null}
			<NoticeStack notices={notices} rightOpen={rightOpen} />
		</div>
	);
}

function SidebarHeader({
	counts,
	total,
	connection
}: {
	counts: Record<Activity, number>;
	total: number;
	connection: string;
}) {
	return (
		<div className="sidebar-header">
			<div className="masthead">
				<span className={`dot ${connection === "open" ? "ok" : "warn"}`} />
				<span className="masthead-title">sessions</span>
				<span className="masthead-count">{total}</span>
			</div>
			<div className="activity-counts">
				{(["running", "queued", "idle"] as Activity[]).map((activity) => (
					<span className="activity-chip" key={activity}>
						<span className={`dot ${activity}`} />
						{activity}
						<span className="count">{counts[activity] ?? 0}</span>
					</span>
				))}
			</div>
		</div>
	);
}

function SidebarToolbar({
	query,
	onQueryChange,
	onNew,
	onRefresh,
	loading
}: {
	query: string;
	onQueryChange: (query: string) => void;
	onNew: () => void;
	onRefresh: () => void;
	loading: boolean;
}) {
	return (
		<div className="sidebar-toolbar">
			<div className="toolbar-actions">
				<button className="primary-button" type="button" onClick={onNew}>
					<Plus size={14} />
					New session
				</button>
				<button className="icon-button" type="button" onClick={onRefresh} aria-label="refresh" title="refresh">
					<RefreshCw size={14} className={loading ? "spin" : ""} />
				</button>
			</div>
			<label className="search-box">
				<Search size={14} />
				<input value={query} onChange={(event) => onQueryChange(event.target.value)} placeholder="filter sessions..." />
			</label>
		</div>
	);
}

function SessionRow({
	session,
	selected,
	onSelect
}: {
	session: SessionListItem;
	selected: boolean;
	onSelect: () => void;
}) {
	return (
		<button className={`session-row ${selected ? "selected" : ""}`} type="button" onClick={onSelect}>
			<span className={`status-rail ${session.activity}`} />
			<span className="session-main">
				<span className="session-title">{sessionTitle(session)}</span>
				<span className="session-sub">
					{session.local ? "draft - " : ""}
					{session.provider.kind} - {session.provider.model}
				</span>
			</span>
			<span className="session-leaf">
				{session.local ? "local" : session.active_leaf_id ? session.active_leaf_id.slice(0, 6) : "root"}
			</span>
		</button>
	);
}

function LogHeader({
	session,
	snapshot,
	rightOpen,
	onToggleRight
}: {
	session: SessionListItem | null;
	snapshot: SessionSnapshot | null;
	rightOpen: boolean;
	onToggleRight: () => void;
}) {
	return (
		<div className="log-header">
			<span className="dot ok" />
			<span>agent-log</span>
			{session ? (
				<span className="log-session">
					{sessionTitle(session)} - {session.local ? "local draft" : snapshot?.activity ?? session.activity}
				</span>
			) : (
				<span className="log-session">no session selected</span>
			)}
			<button className="icon-button tiny" type="button" onClick={onToggleRight} title={rightOpen ? "close inspector" : "open inspector"}>
				{rightOpen ? <PanelRightClose size={14} /> : <PanelRightOpen size={14} />}
			</button>
		</div>
	);
}

function HistoryPickerDialog({
	mode,
	entries,
	activeLeafId,
	forkTitle,
	onForkTitleChange,
	onClose,
	onRewind,
	onFork
}: {
	mode: "rewind" | "fork";
	entries: TranscriptEntry[];
	activeLeafId: string | null;
	forkTitle: string;
	onForkTitleChange: (title: string) => void;
	onClose: () => void;
	onRewind: (target: HistoryTargetOption) => void;
	onFork: (target: HistoryTargetOption, title: string) => void;
}) {
	const options = useMemo(
		() =>
			mode === "rewind"
				? historyRewindOptions(entries, { includeRoot: false, activeLeafId })
				: historyForkOptions(entries, activeLeafId),
		[activeLeafId, entries, mode]
	);
	const targetCount = options.filter((option) => option.id).length;
	const title = mode === "rewind" ? "Rewind history" : "Fork session";
	const description =
		mode === "rewind"
			? "Pick the user message to restore into the composer."
			: "Pick the transcript point the new session should branch from.";
	const Icon = mode === "rewind" ? RotateCcw : GitFork;

	return (
		<div className="modal-scrim" role="presentation" onMouseDown={onClose}>
			<div
				className="history-dialog"
				role="dialog"
				aria-modal="true"
				aria-labelledby="history-dialog-title"
				onMouseDown={(event) => event.stopPropagation()}
			>
				<div className="history-dialog-head">
					<span className="history-dialog-icon">
						<Icon size={15} />
					</span>
					<div className="history-dialog-copy">
						<h2 id="history-dialog-title">{title}</h2>
						<p>{description}</p>
					</div>
					<button className="icon-button tiny" type="button" onClick={onClose} aria-label="close picker">
						<X size={14} />
					</button>
				</div>

				{mode === "fork" ? (
					<label className="history-title-field">
						<span>Fork title</span>
						<input
							value={forkTitle}
							onChange={(event) => onForkTitleChange(event.target.value)}
							placeholder="Optional title"
							autoFocus
						/>
					</label>
				) : null}

				<div className="history-options" role="listbox" aria-label={`${mode} targets`}>
					{options.map((option) => (
						<button
							key={option.id ?? "root"}
							className="history-option"
							type="button"
							onClick={() => {
								if (mode === "rewind") {
									onRewind(option);
								} else if (option.id) {
									onFork(option, forkTitle);
								}
							}}
						>
							<span className={`history-option-icon ${option.id ? "" : "root"}`}>
								{option.id ? option.turnLabel : "root"}
							</span>
							<span className="history-option-main">
								<span className="history-option-title">
									{option.title}
									{option.isActive ? <span className="history-badge">current</span> : null}
								</span>
								<span className="history-option-preview">{option.preview}</span>
							</span>
							<span className="history-option-meta">{option.meta}</span>
						</button>
					))}
					{targetCount === 0 ? (
						<div className="history-empty">
							{mode === "rewind" ? "No user messages to edit yet." : "No transcript entries yet."}
						</div>
					) : null}
				</div>
			</div>
		</div>
	);
}

function MessageList({
	entries,
	snapshot
}: {
	entries: TranscriptEntry[];
	snapshot: SessionSnapshot | null;
}) {
	const scrollRef = useRef<HTMLDivElement | null>(null);
	const visibleEntries = useMemo(
		() => (snapshot ? branchEntriesFor(entries, snapshot.active_leaf_id) : entries),
		[entries, snapshot?.active_leaf_id]
	);
	useEffect(() => {
		const node = scrollRef.current;
		if (!node) return;
		node.scrollTop = node.scrollHeight;
	}, [visibleEntries.length]);
	const toolIndex = useMemo(() => indexToolEntries(visibleEntries), [visibleEntries]);

	if (!snapshot) {
		return (
			<div className="message-scroll" ref={scrollRef}>
				<div className="empty-state">
					<Terminal size={34} />
					<span>Select or create a session</span>
				</div>
			</div>
		);
	}

	return (
		<div className="message-scroll" ref={scrollRef}>
			{visibleEntries.map((entry) => (
				<TranscriptEntryView entry={entry} key={entry.id} toolIndex={toolIndex} />
			))}
			{snapshot.activity === "running" ? (
				<div className="activity-indicator">
					<Loader2 className="spin" size={14} />
					Agent active
				</div>
			) : null}
		</div>
	);
}

function TranscriptEntryView({
	entry,
	toolIndex
}: {
	entry: TranscriptEntry;
	toolIndex: ReturnType<typeof indexToolEntries>;
}) {
	const item = entry.item;
	if (item.type === "turn_started") {
		return null;
	}
	if (item.type === "turn_finished") {
		if (item.outcome === "Graceful") return null;
		return (
			<SystemMessage
				tone={item.outcome === "Interrupted" ? "info" : "error"}
				text={`turn ${item.turn_id} ${item.outcome.toLowerCase()}`}
			/>
		);
	}
	if (item.type === "user_message") {
		return <UserBubble item={item} entryId={entry.id} />;
	}
	if (item.type === "assistant_message") {
		return <AssistantBlock item={item} entryId={entry.id} toolResults={toolIndex.results} />;
	}
	if (item.type === "tool_result") {
		if (toolIndex.calls.has(item.tool_call_id)) return null;
		return <ToolResultCard item={item} entryId={entry.id} />;
	}
	if (item.type === "tool_call_started") {
		return null;
	}
	if (item.type === "injected") {
		return <SystemMessage tone="info" text={`${item.kind}: ${item.content}`} />;
	}
	return null;
}

function UserBubble({ item, entryId }: { item: Extract<TranscriptItem, { type: "user_message" }>; entryId: string }) {
	return (
		<div className="message-row user-row">
			<EntryId entryId={entryId} />
			<div className="user-bubble">{contentBlocksToText(item.content)}</div>
		</div>
	);
}

function AssistantBlock({
	item,
	entryId,
	toolResults
}: {
	item: Extract<TranscriptItem, { type: "assistant_message" }>;
	entryId: string;
	toolResults: Map<string, ToolResultItem>;
}) {
	return (
		<div className="message-row assistant-row">
			<EntryId entryId={entryId} />
			<div className="assistant-block">
				{item.items.map((assistantItem, index) => {
					if (assistantItem.type === "text") {
						return <MarkdownText text={assistantItem.text} key={index} />;
					}
					if (assistantItem.type === "thinking_redacted") {
						return <div className="thinking" key={index}>thinking redacted</div>;
					}
					return (
						<ToolCard
							key={assistantItem.id}
							toolName={assistantItem.tool_name}
							toolId={assistantItem.id}
							argsJson={assistantItem.args_json}
							result={toolResults.get(assistantItem.id)}
						/>
					);
				})}
			</div>
		</div>
	);
}

function MarkdownText({ text }: { text: string }) {
	return (
		<div className="assistant-markdown">
			<ReactMarkdown
				rehypePlugins={[rehypeRaw]}
				remarkPlugins={[remarkGfm]}
				components={{
					a: ({ href, children, ...props }) => (
						<a href={href} target="_blank" rel="noreferrer" {...props}>
							{children}
						</a>
					)
				}}
			>
				{text}
			</ReactMarkdown>
		</div>
	);
}

function ToolResultCard({ item, entryId }: { item: Extract<TranscriptItem, { type: "tool_result" }>; entryId: string }) {
	return (
		<div className="message-row tool-row">
			<EntryId entryId={entryId} />
			<ToolCard toolName={item.tool_name} toolId={item.tool_call_id} result={item} />
		</div>
	);
}

function ToolCard({
	toolName,
	toolId,
	argsJson,
	result
}: {
	toolName: string;
	toolId: string;
	argsJson?: string;
	result?: ToolResultItem;
}) {
	const [expanded, setExpanded] = useState(false);
	const input = parseToolInput(argsJson);
	const status = result ? result.status : "Running";
	const ok = result?.status === "Success";
	const failed = !!result && !ok;
	const header = formatToolHeader(toolName, input);

	return (
		<div className={`tool-card ${failed ? "error" : ok ? "ok" : "running"}`}>
			<button className="tool-card-toggle" type="button" onClick={() => setExpanded((open) => !open)}>
				<span className="tool-status-icon" aria-hidden="true">
					{!result ? (
						<Loader2 className="spin" size={14} />
					) : failed ? (
						<AlertTriangle size={14} />
					) : (
						<Check size={14} />
					)}
				</span>
				<Wrench size={13} className="tool-wrench" />
				<span className="tool-title">{header}</span>
				<span className="tool-status">{status.toLowerCase()}</span>
				<ChevronDown size={14} className={`tool-chevron ${expanded ? "open" : ""}`} />
			</button>
			{expanded ? (
				<div className="tool-card-body">
					{input ? (
						<div className="tool-section">
							<div className="tool-section-label">input</div>
							<pre>{JSON.stringify(input, null, 2)}</pre>
						</div>
					) : null}
					{result ? (
						<div className="tool-section">
							<div className="tool-section-label">output</div>
							<ToolOutput result={result} />
						</div>
					) : (
						<div className="tool-pending">waiting for tool result</div>
					)}
					<div className="tool-call-id">id {toolId}</div>
				</div>
			) : null}
		</div>
	);
}

function ToolOutput({ result }: { result: ToolResultItem }) {
	const output = result.output || "(empty)";
	const lines = output.split("\n");
	const isLong = lines.length > 28;
	const display = isLong ? `${lines.slice(0, 28).join("\n")}\n...` : output;
	return <pre className={result.status === "Success" ? "" : "tool-output-error"}>{display}</pre>;
}

function SystemMessage({ tone, text, entryId }: { tone: Notice["tone"]; text: string; entryId?: string }) {
	return (
		<div className={`system-message ${tone}`}>
			{entryId ? <EntryId entryId={entryId} inline /> : null}
			{text}
		</div>
	);
}

function NoticeStack({ notices, rightOpen }: { notices: Notice[]; rightOpen: boolean }) {
	if (notices.length === 0) return null;
	return (
		<div className={`notice-stack ${rightOpen ? "with-inspector" : ""}`} aria-live="polite">
			{notices.slice(-4).map((notice) => (
				<div className={`notice-toast ${notice.tone}`} key={notice.id}>
					{notice.text}
				</div>
			))}
		</div>
	);
}

function EntryId({ entryId, inline = false }: { entryId: string; inline?: boolean }) {
	const copy = () => {
		void navigator.clipboard?.writeText(entryId).catch(() => undefined);
	};
	return (
		<button
			type="button"
			className={`entry-id ${inline ? "inline" : ""}`}
			onClick={copy}
			title={`copy ${entryId}`}
			aria-label={`copy entry id ${entryId}`}
			data-entry-id={entryId}
		>
			<span>{entryId.slice(0, 8)}</span>
			<Copy size={10} />
		</button>
	);
}

function QueuedInputPane({
	inputs,
	visible,
	onPromote
}: {
	inputs: QueuedInput[];
	visible: boolean;
	onPromote: (inputId: string) => void;
}) {
	if (!visible) return null;
	return (
		<div className="queue-pane">
			<div className="queue-pane-head">
				<span>Queued messages</span>
				<code>{inputs.length}</code>
			</div>
			<div className="queue-list">
				{inputs.map((input) => {
					const canPromote = input.priority === "follow_up" && input.status === "queued";
					return (
						<div className="queue-row" key={input.input_id}>
							<span className={`queue-priority ${input.priority === "steer" ? "steer" : ""}`}>
								{input.priority === "steer" ? "steer" : "follow-up"}
							</span>
							<span className="queue-preview">{truncate(firstLine(contentBlocksToText(input.content)) || "(empty)", 96)}</span>
							<button
								className="queue-steer-button"
								type="button"
								onClick={() => onPromote(input.input_id)}
								disabled={!canPromote}
								title={canPromote ? "promote to steer" : "already steering"}
							>
								<MoveUp size={13} />
								<span>{input.priority === "steer" ? "steering" : "steer"}</span>
							</button>
						</div>
					);
				})}
			</div>
		</div>
	);
}

function SlashMenu({
	commands,
	visible,
	selectedIndex,
	onSetIndex,
	onSelect
}: {
	commands: typeof COMMANDS;
	visible: boolean;
	selectedIndex: number;
	onSetIndex: (index: number) => void;
	onSelect: (command: (typeof COMMANDS)[number]) => void;
}) {
	if (!visible || commands.length === 0) return null;
	return (
		<div className="slash-menu" role="listbox" aria-label="slash commands">
			{commands.map((command, index) => (
				<button
					type="button"
					key={command.name}
					className={`slash-row ${index === selectedIndex ? "selected" : ""}`}
					role="option"
					aria-selected={index === selectedIndex}
					onMouseEnter={() => onSetIndex(index)}
					onMouseDown={(event) => {
						event.preventDefault();
						onSelect(command);
					}}
				>
					<span className="slash-name">
						/{command.name}
						{command.argumentHint ? <small>{command.argumentHint}</small> : null}
					</span>
					<span className="slash-description">{command.description}</span>
				</button>
			))}
		</div>
	);
}

function Inspector({
	snapshot,
	config,
	tools
}: {
	snapshot: SessionSnapshot | null;
	config: DaemonConfig;
	tools: ToolDefinition[];
}) {
	return (
		<div className="inspector-inner">
			<div className="inspector-head">
				<Settings size={14} />
				<span>inspector</span>
			</div>
			<section className="inspect-section">
				<h2>Global</h2>
				<div className="kv">
					<span>system</span>
					<strong>{config.system_prompt ? truncate(config.system_prompt, 80) : "empty"}</strong>
				</div>
			</section>
			<section className="inspect-section">
				<h2>Session</h2>
				{snapshot ? (
					<>
						<div className="kv"><span>activity</span><strong>{snapshot.activity}</strong></div>
						<div className="kv"><span>leaf</span><strong>{snapshot.active_leaf_id?.slice(0, 12) ?? "root"}</strong></div>
						<div className="kv"><span>provider</span><strong>{snapshot.provider.kind} {snapshot.provider.model}</strong></div>
						<div className="kv"><span>metadata</span><strong>{Object.keys(snapshot.metadata).length}</strong></div>
					</>
				) : (
					<p className="muted">No session selected.</p>
				)}
			</section>
			<section className="inspect-section">
				<h2>Pending</h2>
				{snapshot?.pending_actions.length ? (
					<div className="pending-list">
						{snapshot.pending_actions.map((action) => (
							<div className="pending-row" key={action.action_row_id}>
								<span>{action.kind}</span>
								<code>{action.action_row_id.slice(0, 12)}</code>
							</div>
						))}
					</div>
				) : (
					<p className="muted">No active work.</p>
				)}
			</section>
			<section className="inspect-section">
				<h2>Tools</h2>
				<div className="tool-list">
					{tools.map((tool) => (
						<span key={tool.name}>{tool.name}</span>
					))}
				</div>
			</section>
			<section className="inspect-section commands">
				<h2>Slash</h2>
				{COMMANDS.map((command) => (
					<div className="command-row" key={command.name}>
						<code>/{command.name}</code>
						<span>{command.argumentHint ?? ""}</span>
					</div>
				))}
			</section>
		</div>
	);
}

function sessionTitle(session: SessionListItem): string {
	const title = session.metadata?.title;
	return typeof title === "string" && title.trim() ? title : session.session_id.slice(0, 13);
}

function tallyActivities(sessions: SessionListItem[]): Record<Activity, number> {
	return sessions.reduce<Record<Activity, number>>(
		(counts, session) => {
			counts[session.activity] += 1;
			return counts;
		},
		{ idle: 0, queued: 0, running: 0 }
	);
}

async function waitForIdleSession(rpc: AgentRpcClient, sessionId: string): Promise<SessionSnapshot> {
	for (let attempt = 0; attempt < 80; attempt += 1) {
		const snapshot = await rpc.request<SessionSnapshot>("session.get", { session_id: sessionId });
		if (snapshot.activity === "idle") return snapshot;
		await sleep(250);
	}
	throw new Error("session did not become idle after interrupt");
}

function sleep(ms: number): Promise<void> {
	return new Promise((resolve) => window.setTimeout(resolve, ms));
}

function contentBlocksToText(blocks: ContentBlock[]): string {
	return blocks
		.map((block) => {
			if (block.type === "text") return block.text;
			const source = block.image.source.kind === "url" ? block.image.source.value : "base64";
			return `[image ${block.image.mime_type} ${source}]`;
		})
		.join("\n");
}

function historyRewindOptions(
	entries: TranscriptEntry[],
	options: { includeRoot: boolean; activeLeafId: string | null }
): HistoryTargetOption[] {
	const branch = branchEntriesFor(entries, options.activeLeafId);
	const targets: HistoryTargetOption[] = [];
	let previousBoundaryId: string | null = null;
	let currentTurnId: number | null = null;
	for (let index = 0; index < branch.length; index += 1) {
		const entry = branch[index];
		const item = entry.item;
		if (item.type === "turn_started") {
			currentTurnId = item.turn_id;
			continue;
		}
		if (item.type === "user_message") {
			const text = contentBlocksToText(item.content);
			targets.push({
				id: entry.id,
				actionLeafId: previousBoundaryId,
				expectedActiveLeafId: options.activeLeafId,
				sourceEntryId: entry.id,
				restoreText: text,
				turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
				title: currentTurnId ? `Edit user message in turn ${currentTurnId}` : "Edit user message",
				preview: truncate(text.trim() || "Empty user message.", 96),
				meta: `before message · ${formatTimestamp(entry.timestamp_ms)}`,
				isActive: options.activeLeafId === previousBoundaryId
			});
			continue;
		}
		if (item.type === "turn_finished") {
			previousBoundaryId = entry.id;
			currentTurnId = null;
		}
	}
	targets.reverse();
	if (options.includeRoot) {
		targets.push({
			id: null,
			actionLeafId: null,
			expectedActiveLeafId: options.activeLeafId,
			turnLabel: "root",
			title: "Root",
			preview: "Before the first committed turn.",
			meta: "empty context",
			isActive: options.activeLeafId === null
		});
	}
	return targets;
}

function historyForkOptions(entries: TranscriptEntry[], activeLeafId: string | null): HistoryTargetOption[] {
	const options: HistoryTargetOption[] = [];
	for (const entry of entries) {
		if (entry.item.type === "turn_started") continue;
		const branch = branchEntriesFor(entries, entry.id);
		const index = branch.length - 1;
		const option = forkOptionForEntry(entry, branch, index, turnIdAt(branch, index), activeLeafId);
		if (option) options.push(option);
	}
	return options.reverse();
}

function branchEntriesFor(entries: TranscriptEntry[], leafId: string | null): TranscriptEntry[] {
	if (!leafId) return [];
	const byId = new Map(entries.map((entry) => [entry.id, entry]));
	const branch: TranscriptEntry[] = [];
	const seen = new Set<string>();
	let cursor: string | null = leafId;
	while (cursor && !seen.has(cursor)) {
		const entry = byId.get(cursor);
		if (!entry) break;
		branch.push(entry);
		seen.add(cursor);
		cursor = entry.parent_id;
	}
	return branch.reverse();
}

function turnIdAt(entries: TranscriptEntry[], index: number): number | null {
	const item = entries[index]?.item;
	if (item?.type === "turn_finished") return item.turn_id;
	for (let cursor = index; cursor >= 0; cursor -= 1) {
		const candidate = entries[cursor].item;
		if (candidate.type === "turn_started") return candidate.turn_id;
		if (cursor !== index && candidate.type === "turn_finished") return null;
	}
	return null;
}

function forkOptionForEntry(
	entry: TranscriptEntry,
	entries: TranscriptEntry[],
	index: number,
	currentTurnId: number | null,
	activeLeafId: string | null
): HistoryTargetOption | null {
	const item = entry.item;
	const time = formatTimestamp(entry.timestamp_ms);
	const isActive = activeLeafId === entry.id;
	if (item.type === "user_message") {
		const text = contentBlocksToText(item.content);
		return {
			id: entry.id,
			actionLeafId: previousTurnBoundaryId(entries, index),
			sourceEntryId: entry.id,
			placement: "before",
			restoreText: text,
			turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
			title: currentTurnId ? `User message in turn ${currentTurnId}` : "User message",
			preview: truncate(text.trim() || "Empty user message.", 96),
			meta: time,
			isActive
		};
	}
	if (item.type === "assistant_message") {
		const toolNames = item.items
			.filter((assistantItem) => assistantItem.type === "tool_call")
			.map((assistantItem) => assistantItem.tool_name);
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: currentTurnId ? `a${currentTurnId}` : "asst",
			title: currentTurnId ? `Assistant message in turn ${currentTurnId}` : "Assistant message",
			preview: assistantPreview(item) || (toolNames.length ? `Tool call: ${toolNames.join(", ")}` : "Assistant message."),
			meta: time,
			isActive
		};
	}
	if (item.type === "tool_result") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: "tool",
			title: `Tool result: ${item.tool_name}`,
			preview: `${item.status.toLowerCase()}: ${truncate(firstLine(item.output) || "(empty)", 84)}`,
			meta: time,
			isActive
		};
	}
	if (item.type === "turn_finished") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: `t${item.turn_id}`,
			title: `End of turn ${item.turn_id}`,
			preview: `${item.outcome.toLowerCase()} turn boundary.`,
			meta: time,
			isActive
		};
	}
	if (item.type === "injected") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: "note",
			title: item.kind,
			preview: truncate(item.content, 96),
			meta: time,
			isActive
		};
	}
	return null;
}

function assistantPreview(item: Extract<TranscriptItem, { type: "assistant_message" }>): string {
	return truncate(
		item.items
			.map((assistantItem) => (assistantItem.type === "text" ? assistantItem.text : ""))
			.join(" ")
			.trim(),
		96
	);
}

function entriesForTurn(entries: TranscriptEntry[], finishedIndex: number, turnId: number): TranscriptEntry[] {
	let startedIndex = -1;
	for (let index = finishedIndex; index >= 0; index -= 1) {
		const item = entries[index].item;
		if (item.type === "turn_started" && item.turn_id === turnId) {
			startedIndex = index;
			break;
		}
	}
	return entries.slice(startedIndex >= 0 ? startedIndex : 0, finishedIndex + 1);
}

function previousTurnBoundaryId(entries: TranscriptEntry[], beforeIndex: number): string | null {
	for (let index = beforeIndex - 1; index >= 0; index -= 1) {
		const entry = entries[index];
		if (entry.item.type === "turn_finished") return entry.id;
	}
	return null;
}

function turnPreview(entries: TranscriptEntry[]): string {
	for (let index = entries.length - 1; index >= 0; index -= 1) {
		const item = entries[index].item;
		if (item.type !== "user_message") continue;
		const text = contentBlocksToText(item.content).trim();
		if (text) return truncate(text, 96);
	}
	for (let index = entries.length - 1; index >= 0; index -= 1) {
		const item = entries[index].item;
		if (item.type !== "assistant_message") continue;
		const text = item.items
			.map((assistantItem) => (assistantItem.type === "text" ? assistantItem.text : ""))
			.join(" ")
			.trim();
		if (text) return truncate(text, 96);
	}
	return "No visible message content.";
}

function formatTimestamp(timestampMs: number): string {
	return new Date(timestampMs).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}

function indexToolEntries(entries: TranscriptEntry[]) {
	const results = new Map<string, ToolResultItem>();
	const calls = new Set<string>();
	for (const entry of entries) {
		const item = entry.item;
		if (item.type === "tool_result") {
			results.set(item.tool_call_id, item);
			continue;
		}
		if (item.type === "assistant_message") {
			for (const assistantItem of item.items) {
				if (assistantItem.type === "tool_call") calls.add(assistantItem.id);
			}
		}
	}
	return { results, calls };
}

function parseToolInput(argsJson?: string): Record<string, unknown> | null {
	if (!argsJson) return null;
	try {
		const value = JSON.parse(argsJson) as unknown;
		return value && typeof value === "object" && !Array.isArray(value)
			? (value as Record<string, unknown>)
			: null;
	} catch {
		return null;
	}
}

function formatToolHeader(name: string, input: Record<string, unknown> | null): string {
	if (!input) return name;
	const text = (key: string) => {
		const value = input[key];
		return typeof value === "string" ? value : "";
	};
	switch (name.toLowerCase()) {
		case "read":
			return `Read: ${text("path") || text("file_path")}`;
		case "write":
			return `Write: ${text("path") || text("file_path")}`;
		case "edit":
			return `Edit: ${text("path") || text("file_path")}`;
		case "bash":
			return `Bash: ${firstLine(text("command"))}`;
		default:
			return name;
	}
}

function firstLine(value: string): string {
	return value.split("\n")[0]?.trim() || "";
}

function summarizeContext(items: TranscriptItem[]): string {
	const counts = new Map<string, number>();
	for (const item of items) counts.set(item.type, (counts.get(item.type) ?? 0) + 1);
	const parts = Array.from(counts.entries()).map(([type, count]) => `${type}:${count}`);
	return `context ${items.length} items - ${parts.join(" ") || "empty"}`;
}

function summarizeTree(tree: HistoryTree): string {
	const boundaries = tree.entries.filter((entry) => entry.item.type === "turn_finished").length;
	return `tree ${tree.entries.length} entries - ${boundaries} boundaries - active ${tree.active_leaf_id?.slice(0, 8) ?? "root"}`;
}

function truncate(value: string, max: number): string {
	return value.length > max ? `${value.slice(0, max - 3)}...` : value;
}

function isDraftSessionId(sessionId: string): boolean {
	return sessionId.startsWith("draft_");
}

function randomToken(): string {
	if (typeof crypto !== "undefined" && "randomUUID" in crypto) {
		return crypto.randomUUID();
	}
	return `${Date.now()}_${Math.random().toString(16).slice(2)}`;
}

function loadDraftSessions(): DraftSession[] {
	const drafts = loadLocalJson<DraftSession[]>(DRAFT_SESSIONS_KEY, []).filter(isDraftSession);
	const collapsed = collapseEmptyDraftSessions(drafts);
	if (collapsed.length !== drafts.length) saveDraftSessions(collapsed);
	return collapsed;
}

function saveDraftSessions(drafts: DraftSession[]) {
	saveLocalJson(DRAFT_SESSIONS_KEY, drafts);
}

function collapseEmptyDraftSessions(drafts: DraftSession[], keepDraftId?: string | null): DraftSession[] {
	let keptEmptyDraft = false;
	return drafts.filter((draft) => {
		if (!isEmptyDraftSession(draft)) return true;
		if (keepDraftId && draft.draft_id === keepDraftId) {
			keptEmptyDraft = true;
			return true;
		}
		if (!keepDraftId && !keptEmptyDraft) {
			keptEmptyDraft = true;
			return true;
		}
		return false;
	});
}

function isEmptyDraftSession(draft: DraftSession): boolean {
	return draft.composer.trim().length === 0;
}

function loadComposerDrafts(): Record<string, DurableComposerDraft> {
	return loadLocalJson<Record<string, DurableComposerDraft>>(COMPOSER_DRAFTS_KEY, {});
}

function upsertComposerDraft(draft: DurableComposerDraft) {
	const drafts = loadComposerDrafts();
	drafts[draft.session_id] = draft;
	saveLocalJson(COMPOSER_DRAFTS_KEY, drafts);
}

function deleteComposerDraft(sessionId: string) {
	const drafts = loadComposerDrafts();
	if (!(sessionId in drafts)) return;
	delete drafts[sessionId];
	saveLocalJson(COMPOSER_DRAFTS_KEY, drafts);
}

function loadLocalJson<T>(key: string, fallback: T): T {
	try {
		const raw = window.localStorage.getItem(key);
		return raw ? (JSON.parse(raw) as T) : fallback;
	} catch {
		return fallback;
	}
}

function saveLocalJson<T>(key: string, value: T) {
	try {
		window.localStorage.setItem(key, JSON.stringify(value));
	} catch {
		// Local drafts are convenience state; the durable session model remains valid without them.
	}
}

function isDraftSession(value: unknown): value is DraftSession {
	if (!value || typeof value !== "object") return false;
	const draft = value as Partial<DraftSession>;
	return (
		typeof draft.draft_id === "string" &&
		typeof draft.session_id === "string" &&
		typeof draft.title === "string" &&
		typeof draft.composer === "string" &&
		typeof draft.created_at === "number" &&
		typeof draft.updated_at === "number" &&
		!!draft.provider &&
		typeof draft.provider.kind === "string" &&
		typeof draft.provider.model === "string"
	);
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function modelErrorNotice(data: Record<string, unknown>): string {
	const error = typeof data.error === "string" ? data.error : "model request failed";
	return `model error: ${truncate(error, 420)}`;
}
