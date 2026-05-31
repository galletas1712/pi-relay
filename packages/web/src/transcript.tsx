import { memo, useCallback, useEffect, useLayoutEffect, useMemo, useRef, useState, type ReactNode, type UIEvent } from "react";
import { AlertTriangle, Check, ChevronDown, Copy, Loader2, RotateCcw, Terminal } from "lucide-react";
import rehypeRaw from "rehype-raw";
import rehypeHighlight from "rehype-highlight";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { branchEntriesFor } from "./historyTargets.ts";
import { MermaidBlock } from "./mermaidBlock.tsx";
import { citationsFromReplay, hostedToolsFromReplay, localToolCallIdFromReplay, parsedProviderReplay, replayContainsAssistantText } from "./providerReplay.ts";
import type { HostedToolView, SourceCitation } from "./providerReplay.ts";
import { contentBlocksToText, firstLine } from "./text.ts";
import { assistantMessageText, buildTurnViews } from "./turnView.ts";
import type { ModelStepView, TurnView } from "./turnView.ts";
import type { AssistantItem, NoticeTone, PendingAction, ReplayDisplay, TranscriptEntry, TranscriptItem } from "./types.ts";

type ToolResultItem = Extract<TranscriptItem, { type: "tool_result" }>;
type AssistantMessageEntry = TranscriptEntry & { item: Extract<TranscriptItem, { type: "assistant_message" }> };
type ToolRunStatusKind = "success" | "error" | "running";

type ToolRunItem =
	| {
			source: "local";
			key: string;
			entryId: string;
			id: string;
			rawName: string;
			prettyName: string;
			title: string;
			statusKind: ToolRunStatusKind;
			statusLabel: string;
			argsJson?: string;
			display?: ReplayDisplay | null;
			result?: ToolResultItem;
			input: Record<string, unknown> | null;
			editPreview: EditToolPreview | null;
	  }
	| {
			source: "hosted";
			key: string;
			entryId: string;
			id: string;
			rawName: string;
			prettyName: string;
			title: string;
			statusKind: ToolRunStatusKind;
			statusLabel: string;
			tool: HostedToolView;
	  };

type TranscriptDisplayNode =
	| { type: "user"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "user_message" }> } }
	| {
			type: "assistant_text";
			key: string;
			entry: AssistantMessageEntry;
			text: string;
			copyText: string;
			phase: ModelStepView["phase"];
			citations: SourceCitation[];
	  }
	| { type: "tool_group"; key: string; id: string; items: ToolRunItem[]; turnId: number | null; turnOpen: boolean; isLive: boolean }
	| { type: "turn_finished"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "turn_finished" }> } }
	| { type: "tool_result"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "tool_result" }> } }
	| { type: "compaction_summary"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "compaction_summary" }> } }
	| { type: "compaction_in_progress"; key: string; trigger: "auto" | "manual"; reason: string | null };

type ScrollMetrics = Pick<HTMLDivElement, "clientHeight" | "scrollHeight" | "scrollTop">;
const STICKY_BOTTOM_EPSILON_PX = 1;
const ACTIVE_SESSION_SCROLL_KEY = "__active_session__";
const TRANSCRIPT_SCROLL_STORAGE_KEY = "piRelayTranscriptScroll:v1";
const RECENT_TOOL_ROW_COUNT = 3;

export interface ScrollPositionSnapshot {
	scrollTop: number;
	sticky: boolean;
}

export type TranscriptScrollStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

export function isScrolledAtBottom(node: ScrollMetrics): boolean {
	return node.scrollHeight - node.scrollTop - node.clientHeight <= STICKY_BOTTOM_EPSILON_PX;
}

function bottomScrollTop(node: ScrollMetrics): number {
	return Math.max(0, node.scrollHeight - node.clientHeight);
}

export function captureScrollPosition(node: ScrollMetrics): ScrollPositionSnapshot {
	return {
		scrollTop: node.scrollTop,
		sticky: isScrolledAtBottom(node)
	};
}

export function restoreScrollPosition(node: ScrollMetrics, position: ScrollPositionSnapshot): boolean {
	if (position.sticky) {
		node.scrollTop = bottomScrollTop(node);
	} else {
		node.scrollTop = position.scrollTop;
	}
	return isScrolledAtBottom(node);
}

export function loadTranscriptScrollPositions(storage = browserStorage()): Map<string, ScrollPositionSnapshot> {
	const positions = new Map<string, ScrollPositionSnapshot>();
	if (!storage) return positions;
	try {
		const raw = storage.getItem(TRANSCRIPT_SCROLL_STORAGE_KEY);
		if (!raw) return positions;
		const parsed = JSON.parse(raw) as unknown;
		if (!isRecord(parsed) || !isRecord(parsed.positions)) return positions;
		for (const [key, value] of Object.entries(parsed.positions)) {
			if (!key || !isRecord(value)) continue;
			const scrollTop = value.scrollTop;
			const sticky = value.sticky;
			if (typeof scrollTop !== "number" || !Number.isFinite(scrollTop) || typeof sticky !== "boolean") continue;
			positions.set(key, { scrollTop: Math.max(0, scrollTop), sticky });
		}
	} catch {
		return new Map();
	}
	return positions;
}

export function saveTranscriptScrollPositions(positions: Map<string, ScrollPositionSnapshot>, storage = browserStorage()): void {
	if (!storage) return;
	try {
		const entries = Array.from(positions.entries()).filter(([key]) => key);
		if (entries.length === 0) {
			storage.removeItem(TRANSCRIPT_SCROLL_STORAGE_KEY);
			return;
		}
		storage.setItem(
			TRANSCRIPT_SCROLL_STORAGE_KEY,
			JSON.stringify({
				positions: Object.fromEntries(entries),
				updatedAt: Date.now(),
			}),
		);
	} catch {
		// localStorage can be unavailable or full; scroll persistence is best-effort.
	}
}

function browserStorage(): TranscriptScrollStorage | null {
	if (typeof window === "undefined") return null;
	try {
		return window.localStorage ?? null;
	} catch {
		return null;
	}
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

export { TRANSCRIPT_SCROLL_STORAGE_KEY };

export const MessageList = memo(function MessageList({
	entries,
	pendingActions,
	activeLeafId,
	isRunning,
	serverTimeMs,
	hasSession,
	sessionId,
	entriesSessionId,
	loadingSession = false,
	onResumeTurn,
	resumingTurnId
}: {
	entries: TranscriptEntry[];
	pendingActions?: PendingAction[];
	activeLeafId: string | null;
	isRunning: boolean;
	serverTimeMs: number | null;
	hasSession: boolean;
	sessionId?: string | null;
	entriesSessionId?: string | null;
	loadingSession?: boolean;
	onResumeTurn?: (entryId: string, outcome: "Interrupted" | "Crashed") => void;
	resumingTurnId?: string | null;
}) {
	const scrollRef = useRef<HTMLDivElement | null>(null);
	const contentRef = useRef<HTMLDivElement | null>(null);
	const shouldStickToBottomRef = useRef(true);
	const activeScrollSessionKeyRef = useRef<string | null>(null);
	const activeScrollSessionCanSaveRef = useRef(false);
	const pendingScrollRestoreRef = useRef<{ key: string; position: ScrollPositionSnapshot } | null>(null);
	const scrollPositionsRef = useRef(loadTranscriptScrollPositions());
	const scrollSessionKey = hasSession ? (sessionId ?? ACTIVE_SESSION_SCROLL_KEY) : null;
	const entriesBelongToSelectedSession = !hasSession || !sessionId || entriesSessionId === sessionId;
	const effectiveEntries = entriesBelongToSelectedSession ? entries : [];
	const visibleEntries = useMemo(
		() => (hasSession ? branchEntriesFor(effectiveEntries, activeLeafId) : effectiveEntries),
		[activeLeafId, effectiveEntries, hasSession]
	);

	const scrollToBottom = useCallback(() => {
		const node = scrollRef.current;
		if (!node) return;
		node.scrollTop = bottomScrollTop(node);
		shouldStickToBottomRef.current = true;
		const key = activeScrollSessionKeyRef.current;
		if (key && activeScrollSessionCanSaveRef.current) {
			scrollPositionsRef.current.set(key, { scrollTop: node.scrollTop, sticky: true });
			saveTranscriptScrollPositions(scrollPositionsRef.current);
		}
	}, []);

	const handleScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
		if (pendingScrollRestoreRef.current?.key === activeScrollSessionKeyRef.current) return;
		const position = captureScrollPosition(event.currentTarget);
		shouldStickToBottomRef.current = position.sticky;
		const key = activeScrollSessionKeyRef.current;
		if (key && activeScrollSessionCanSaveRef.current) {
			scrollPositionsRef.current.set(key, position);
			saveTranscriptScrollPositions(scrollPositionsRef.current);
		}
	}, []);

	useLayoutEffect(() => {
		if (activeScrollSessionKeyRef.current === scrollSessionKey) return;
		const node = scrollRef.current;
		const previousKey = activeScrollSessionKeyRef.current;
		if (previousKey && node && activeScrollSessionCanSaveRef.current) {
			scrollPositionsRef.current.set(previousKey, captureScrollPosition(node));
			saveTranscriptScrollPositions(scrollPositionsRef.current);
		}
		activeScrollSessionKeyRef.current = scrollSessionKey;
		activeScrollSessionCanSaveRef.current = false;
		if (!scrollSessionKey) {
			pendingScrollRestoreRef.current = null;
			shouldStickToBottomRef.current = true;
			return;
		}
		const fallbackPosition = node ? captureScrollPosition(node) : { scrollTop: 0, sticky: true };
		pendingScrollRestoreRef.current = {
			key: scrollSessionKey,
			position: scrollPositionsRef.current.get(scrollSessionKey) ?? fallbackPosition
		};
		shouldStickToBottomRef.current = false;
	}, [scrollSessionKey]);

	useLayoutEffect(() => {
		const pendingRestore = pendingScrollRestoreRef.current;
		if (pendingRestore?.key === scrollSessionKey) {
			if (!entriesBelongToSelectedSession) return;
			const node = scrollRef.current;
			if (node) {
				const sticky = restoreScrollPosition(node, pendingRestore.position);
				shouldStickToBottomRef.current = sticky;
				if (scrollSessionKey) {
					scrollPositionsRef.current.set(scrollSessionKey, { scrollTop: node.scrollTop, sticky });
					saveTranscriptScrollPositions(scrollPositionsRef.current);
				}
			}
			activeScrollSessionCanSaveRef.current = true;
			pendingScrollRestoreRef.current = null;
			return;
		}
		if (!entriesBelongToSelectedSession) return;
		activeScrollSessionCanSaveRef.current = true;
		if (!shouldStickToBottomRef.current) return;
		scrollToBottom();
	}, [entriesBelongToSelectedSession, isRunning, scrollSessionKey, scrollToBottom, visibleEntries]);

	useLayoutEffect(() => {
		if (!hasSession || typeof ResizeObserver === "undefined") return;
		const scroller = scrollRef.current;
		const content = contentRef.current;
		if (!scroller || !content) return;
		const observer = new ResizeObserver(() => {
			if (pendingScrollRestoreRef.current?.key === activeScrollSessionKeyRef.current) return;
			if (!entriesBelongToSelectedSession) return;
			if (shouldStickToBottomRef.current) scrollToBottom();
		});
		observer.observe(scroller);
		observer.observe(content);
		return () => observer.disconnect();
	}, [entriesBelongToSelectedSession, hasSession, scrollToBottom]);
	const toolIndex = useMemo(() => indexToolEntries(visibleEntries), [visibleEntries]);
	const turnViews = useMemo(() => buildTurnViews(visibleEntries), [visibleEntries]);
	const displayNodes = useMemo(() => deriveTranscriptDisplayNodes(visibleEntries, turnViews, toolIndex.results, pendingActions), [pendingActions, toolIndex.results, turnViews, visibleEntries]);
	const resumeEntryIdByNode = useMemo(() => {
		const ids = new Map<string, string>();
		for (const node of displayNodes) ids.set(node.key, nodeLeafId(node));
		return ids;
	}, [displayNodes]);
	// Each compaction can hide the display nodes between itself and the previous
	// compaction (or session start). The count is precomputed so the marker's
	// "show N hidden" label stays accurate as turns stream in.
	const compactionHiddenCounts = useMemo(() => {
		const counts = new Map<string, number>();
		let segmentStart = 0;
		displayNodes.forEach((node, index) => {
			if (node.type !== "compaction_summary") return;
			counts.set(node.key, index - segmentStart);
			segmentStart = index + 1;
		});
		return counts;
	}, [displayNodes]);
	const [collapsedCompactions, setCollapsedCompactions] = useState<ReadonlySet<string>>(() => new Set());
	useEffect(() => {
		setCollapsedCompactions(new Set());
	}, [sessionId]);
	const toggleCompaction = useCallback((key: string) => {
		setCollapsedCompactions((prev) => {
			const next = new Set(prev);
			if (next.has(key)) next.delete(key);
			else next.add(key);
			return next;
		});
	}, []);
	const visibleDisplayNodes = useMemo(() => {
		if (compactionHiddenCounts.size === 0) return displayNodes;
		const result: TranscriptDisplayNode[] = [];
		let segmentStart = 0;
		displayNodes.forEach((node, index) => {
			if (node.type !== "compaction_summary") return;
			if (!collapsedCompactions.has(node.key)) {
				for (let j = segmentStart; j < index; j++) result.push(displayNodes[j]);
			}
			result.push(node);
			segmentStart = index + 1;
		});
		for (let j = segmentStart; j < displayNodes.length; j++) result.push(displayNodes[j]);
		return result;
	}, [collapsedCompactions, compactionHiddenCounts, displayNodes]);

	// While a turn is running we show a single "Working…" row at the end of
	// the transcript instead of a sticky pill, so the user can see how long
	// the agent has been thinking. The clock is anchored only to durable
	// server data: either the active branch's `turn_started` entry, or a
	// mid-turn compaction summary that remembers the original turn start after
	// compaction replaced the raw open-turn suffix. We deliberately do not
	// synthesize a local start time; a running turn without this anchor is a
	// protocol/storage bug, not something the UI can make correct.
	const workingStartMs = useMemo(
		() => (isRunning ? runningTurnStartMs(visibleEntries) : null),
		[isRunning, visibleEntries],
	);

	if (!hasSession) {
		return (
			<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
				<div className="empty-state">
					<Terminal size={34} />
					<span>Select or create a session</span>
				</div>
			</div>
		);
	}

	if (loadingSession || !entriesBelongToSelectedSession) {
		return (
			<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
				<div className="empty-state">
					<Loader2 className="spin" size={28} />
					<span>Loading session...</span>
				</div>
			</div>
		);
	}

	return (
		<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
			<div className="message-scroll-content" ref={contentRef}>
				{visibleDisplayNodes.map((node) => (
					<TranscriptDisplayNodeView
						node={node}
						key={node.key}
						toolIndex={toolIndex}
						isActiveLeaf={nodeLeafId(node) === activeLeafId}
						isRunning={isRunning}
						onResumeTurn={onResumeTurn}
						resumeEntryId={resumeEntryIdByNode.get(node.key) ?? nodeLeafId(node)}
						resuming={resumeEntryIdByNode.get(node.key) === resumingTurnId}
						compactionHiddenCount={compactionHiddenCounts.get(node.key) ?? 0}
						compactionExpanded={!collapsedCompactions.has(node.key)}
						onToggleCompaction={toggleCompaction}
					/>
				))}
				{isRunning && workingStartMs != null && serverTimeMs != null ? (
					<WorkingIndicator startMs={workingStartMs} serverTimeMs={serverTimeMs} />
				) : null}
			</div>
		</div>
	);
});

function workingElapsedMs(clock: WorkingClockAnchor): number {
	return Math.max(0, clock.serverAnchorMs + (performance.now() - clock.clientAnchorMs) - clock.startMs);
}

export function stableWorkingElapsedMs(
	previous: WorkingClockAnchor | null,
	startMs: number,
	serverTimeMs: number,
): { clock: WorkingClockAnchor; elapsedMs: number } {
	const clock = previous?.startMs === startMs
		? previous
		: {
				startMs,
				serverAnchorMs: serverTimeMs,
				clientAnchorMs: performance.now(),
			};
	return { clock, elapsedMs: workingElapsedMs(clock) };
}

export interface WorkingClockAnchor {
	startMs: number;
	serverAnchorMs: number;
	clientAnchorMs: number;
}

export function runningTurnStartMs(entries: TranscriptEntry[]): number | null {
	for (let index = entries.length - 1; index >= 0; index -= 1) {
		const entry = entries[index];
		if (entry.item.type === "turn_started") return entry.timestamp_ms;
		if (entry.item.type === "compaction_summary") {
			const turnStartedAtMs = entry.item.turn_started_at_ms;
			if (typeof turnStartedAtMs === "number" && Number.isFinite(turnStartedAtMs) && turnStartedAtMs >= 0) {
				return turnStartedAtMs;
			}
		}
		if (entry.item.type === "turn_finished") break;
	}
	return null;
}

export function runningTurnClockAnchor(
	entries: TranscriptEntry[],
	serverTimeMs: number | null
): WorkingClockAnchor | null {
	if (typeof serverTimeMs !== "number" || !Number.isFinite(serverTimeMs)) return null;
	const startMs = runningTurnStartMs(entries);
	if (startMs === null) return null;
	return {
		startMs,
		serverAnchorMs: serverTimeMs,
		clientAnchorMs: performance.now(),
	};
}

const WorkingIndicator = memo(function WorkingIndicator({ startMs, serverTimeMs }: { startMs: number; serverTimeMs: number }) {
	const anchorRef = useRef<WorkingClockAnchor | null>(null);
	const [elapsedMs, setElapsedMs] = useState(() => {
		const stable = stableWorkingElapsedMs(anchorRef.current, startMs, serverTimeMs);
		anchorRef.current = stable.clock;
		return stable.elapsedMs;
	});
	useEffect(() => {
		const stable = stableWorkingElapsedMs(anchorRef.current, startMs, serverTimeMs);
		anchorRef.current = stable.clock;
		const clock = anchorRef.current!;
		setElapsedMs(stable.elapsedMs);
		const interval = window.setInterval(() => {
			setElapsedMs(workingElapsedMs(clock));
		}, 1000);
		return () => window.clearInterval(interval);
	}, [startMs]);
	return (
		<SystemMessage
			tone="info"
			text={`Working… ${formatElapsed(elapsedMs)}`}
			loading
		/>
	);
});

export function formatElapsed(ms: number): string {
	const totalSeconds = Math.max(0, Math.floor(ms / 1000));
	if (totalSeconds < 60) return `${totalSeconds}s`;
	const minutes = Math.floor(totalSeconds / 60);
	const seconds = totalSeconds % 60;
	if (minutes < 60) return `${minutes}m ${seconds.toString().padStart(2, "0")}s`;
	const hours = Math.floor(minutes / 60);
	const remainingMinutes = minutes % 60;
	return `${hours}h ${remainingMinutes.toString().padStart(2, "0")}m ${seconds.toString().padStart(2, "0")}s`;
}

const TranscriptDisplayNodeView = memo(function TranscriptDisplayNodeView({
	node,
	toolIndex,
	isActiveLeaf,
	isRunning,
	onResumeTurn,
	resumeEntryId,
	resuming,
	compactionHiddenCount,
	compactionExpanded,
	onToggleCompaction
}: {
	node: TranscriptDisplayNode;
	toolIndex: ReturnType<typeof indexToolEntries>;
	isActiveLeaf: boolean;
	isRunning: boolean;
	onResumeTurn?: (entryId: string, outcome: "Interrupted" | "Crashed") => void;
	resumeEntryId: string;
	resuming: boolean;
	compactionHiddenCount: number;
	compactionExpanded: boolean;
	onToggleCompaction: (key: string) => void;
}) {
	if (node.type === "user") {
		return <UserBubble item={node.entry.item} entryId={node.entry.id} />;
	}
	if (node.type === "assistant_text") {
		return <AssistantTextBlock node={node} />;
	}
	if (node.type === "tool_group") {
		return <ToolRunGroup node={node} />;
	}
	if (node.type === "turn_finished") {
		const item = node.entry.item;
		if (item.outcome === "Graceful") return null;
		const canResume = isActiveLeaf && !isRunning && !!onResumeTurn;
		const actionLabel = item.outcome === "Interrupted" ? "Continue" : "Retry";
		const resumableOutcome = item.outcome as "Interrupted" | "Crashed";
		return (
			<SystemMessage
				tone={item.outcome === "Interrupted" ? "info" : "error"}
				text={`turn ${item.turn_id} ${item.outcome.toLowerCase()}`}
				action={
					canResume
						? {
								label: resuming ? "Starting..." : actionLabel,
								disabled: resuming,
								onClick: () => onResumeTurn?.(resumeEntryId, resumableOutcome)
							}
						: undefined
				}
			/>
		);
	}
	if (node.type === "tool_result") {
		const item = node.entry.item;
		if (toolIndex.calls.has(item.tool_call_id)) return null;
		return <ToolResultCard item={item} entryId={node.entry.id} />;
	}
	if (node.type === "compaction_summary") {
		const item = node.entry.item;
		const tokens = typeof item.tokens_before === "number" ? formatCompactionTokens(item.tokens_before) : null;
		const parts = [`Context compacted through turn ${item.last_turn_id}`];
		if (tokens) parts.push(`${tokens} tokens summarized`);
		if (compactionHiddenCount > 0 && !compactionExpanded) parts.push(`${compactionHiddenCount} prior entr${compactionHiddenCount === 1 ? "y" : "ies"} hidden`);
		return (
			<SystemMessage
				tone="info"
				text={parts.join(" · ")}
				action={
					compactionHiddenCount > 0
						? {
								label: compactionExpanded ? "Hide prior" : "Show prior",
								onClick: () => onToggleCompaction(node.key)
							}
						: undefined
				}
			/>
		);
	}
	if (node.type === "compaction_in_progress") {
		const label = node.trigger === "auto" ? "Auto-compacting history" : "Compacting history";
		const text = node.reason ? `${label} · ${node.reason}` : `${label}…`;
		return <SystemMessage tone="info" text={text} loading />;
	}
	return null;
});

const UserBubble = memo(function UserBubble({ item, entryId }: { item: Extract<TranscriptItem, { type: "user_message" }>; entryId: string }) {
	return (
		<div className="message-row user-row">
			<EntryId entryId={entryId} />
			<div className="user-bubble">{contentBlocksToText(item.content)}</div>
		</div>
	);
});

export type AssistantRenderPart =
	| { type: "text"; key: string; item: Extract<AssistantItem, { type: "text" }> }
	| { type: "tool_call"; key: string; item: Extract<AssistantItem, { type: "tool_call" }>; display?: ReplayDisplay | null }
	| { type: "hosted_tool"; key: string; tool: ReturnType<typeof hostedToolsFromReplay>[number] };

export function assistantRenderParts(items: AssistantItem[], providerReplay: TranscriptEntry["provider_replay"] | undefined): AssistantRenderPart[] {
	items = coalesceAdjacentTextItems(items);
	const parsedReplay = parsedProviderReplay(providerReplay);
	const hostedTools = hostedToolsFromReplay(providerReplay);
	if (parsedReplay.length === 0) {
		return items.map((item, index) => itemRenderPart(item, `item-${index}`));
	}

	const parts: AssistantRenderPart[] = [];
	const localToolById = new Map<string, Extract<AssistantItem, { type: "tool_call" }>>();
	for (const item of items) {
		if (item.type === "tool_call") localToolById.set(item.id, item);
	}
	const localDisplayById = new Map<string, ReplayDisplay | null | undefined>();
	for (const replayItem of parsedReplay) {
		const toolId = localToolCallIdFromReplay(replayItem.raw);
		if (toolId) localDisplayById.set(toolId, replayItem.display);
	}
	const hostedById = new Map(hostedTools.map((tool) => [tool.id, tool]));
	const renderedTextIndexes = new Set<number>();
	const renderedToolIds = new Set<string>();
	const renderedHostedIds = new Set<string>();

	const renderText = () => {
		items.forEach((item, index) => {
			if (item.type === "text" && !renderedTextIndexes.has(index)) {
				renderedTextIndexes.add(index);
				parts.push({ type: "text", key: `text-${index}`, item });
			}
		});
	};

	for (const replayItem of parsedReplay) {
		const hosted = replayItem.raw.id ? hostedById.get(replayItem.raw.id) : undefined;
		if (hosted && !renderedHostedIds.has(hosted.id)) {
			renderedHostedIds.add(hosted.id);
			parts.push({ type: "hosted_tool", key: `hosted-${hosted.id}`, tool: hosted });
			continue;
		}
		const toolId = localToolCallIdFromReplay(replayItem.raw);
		if (toolId) {
			const tool = localToolById.get(toolId);
			if (tool && !renderedToolIds.has(tool.id)) {
				renderedToolIds.add(tool.id);
				parts.push({ type: "tool_call", key: `tool-${tool.id}`, item: tool, display: localDisplayById.get(tool.id) });
			}
			continue;
		}
		if (replayContainsAssistantText(replayItem.raw)) {
			renderText();
		}
	}

	items.forEach((item, index) => {
		if (item.type === "text") {
			if (!renderedTextIndexes.has(index)) parts.push({ type: "text", key: `text-${index}`, item });
			return;
		}
		if (!renderedToolIds.has(item.id)) parts.push({ type: "tool_call", key: `tool-${item.id}`, item, display: localDisplayById.get(item.id) });
	});
	for (const tool of hostedTools) {
		if (!renderedHostedIds.has(tool.id)) parts.push({ type: "hosted_tool", key: `hosted-${tool.id}`, tool });
	}
	return parts;
}

function itemRenderPart(item: AssistantItem, key: string): AssistantRenderPart {
	return item.type === "text" ? { type: "text", key, item } : { type: "tool_call", key, item };
}

function coalesceAdjacentTextItems(items: AssistantItem[]): AssistantItem[] {
	const merged: AssistantItem[] = [];
	for (const item of items) {
		const last = merged.at(-1);
		if (item.type === "text" && last?.type === "text") {
			last.text += item.text;
		} else {
			merged.push(item.type === "text" ? { ...item } : item);
		}
	}
	return merged;
}

export function deriveTranscriptDisplayNodes(
	entries: TranscriptEntry[],
	turns: TurnView[] = buildTurnViews(entries),
	toolResults: Map<string, ToolResultItem> = indexToolEntries(entries).results,
	pendingActions: PendingAction[] = []
): TranscriptDisplayNode[] {
	const builder = new TranscriptDisplayBuilder(entries, turns, toolResults, pendingActions);
	return builder.build();
}

class TranscriptDisplayBuilder {
	private readonly turnByEntryId = new Map<string, TurnView>();
	private readonly toolCallIds = new Set<string>();
	private readonly nodes: TranscriptDisplayNode[] = [];
	private pendingGroup: Extract<TranscriptDisplayNode, { type: "tool_group" }> | null = null;

	constructor(
		private readonly entries: TranscriptEntry[],
		private readonly turns: TurnView[],
		private readonly toolResults: Map<string, ToolResultItem>,
		private readonly pendingActions: PendingAction[]
	) {
		for (const turn of turns) {
			for (const entry of turn.entries) this.turnByEntryId.set(entry.id, turn);
		}
		for (const entry of entries) {
			if (entry.item.type !== "assistant_message") continue;
			for (const assistantItem of entry.item.items) {
				if (assistantItem.type === "tool_call") this.toolCallIds.add(assistantItem.id);
			}
		}
	}

	build(): TranscriptDisplayNode[] {
		for (const entry of this.entries) this.appendEntry(entry);
		this.flushGroup();
		this.appendPendingToolRuns();
		this.appendPendingCompactions();
		return markLiveToolGroups(this.nodes);
	}

	private appendEntry(entry: TranscriptEntry) {
		const item = entry.item;
		if (item.type === "turn_started") {
			this.flushGroup();
			return;
		}
		if (item.type === "user_message") {
			this.flushGroup();
			this.nodes.push({ type: "user", key: entry.id, entry: entry as Extract<TranscriptDisplayNode, { type: "user" }>["entry"] });
			return;
		}
		if (item.type === "assistant_message") {
			this.appendAssistantMessage(entry as AssistantMessageEntry);
			return;
		}
		if (item.type === "tool_result") {
			if (!this.toolCallIds.has(item.tool_call_id)) {
				this.flushGroup();
				this.nodes.push({ type: "tool_result", key: entry.id, entry: entry as Extract<TranscriptDisplayNode, { type: "tool_result" }>["entry"] });
			}
			return;
		}
		if (item.type === "tool_call_started") return;
		if (item.type === "turn_finished") {
			this.flushGroup();
			this.nodes.push({ type: "turn_finished", key: entry.id, entry: entry as Extract<TranscriptDisplayNode, { type: "turn_finished" }>["entry"] });
			return;
		}
		if (item.type === "compaction_summary") {
			this.flushGroup();
			this.nodes.push({ type: "compaction_summary", key: entry.id, entry: entry as Extract<TranscriptDisplayNode, { type: "compaction_summary" }>["entry"] });
		}
	}

	private appendAssistantMessage(entry: AssistantMessageEntry) {
		const parts = assistantRenderParts(entry.item.items, entry.provider_replay);
		const citations = citationsFromReplay(entry.provider_replay);
		let citationsRendered = false;
		for (const part of parts) {
			if (part.type === "text") {
				if (!part.item.text) continue;
				this.flushGroup();
				this.nodes.push(this.assistantTextNode(entry, part.key, part.item.text, citationsRendered ? [] : citations));
				citationsRendered = true;
				continue;
			}
			if (part.type === "tool_call") {
				this.appendToolItem(entry, localToolRunItem(entry.id, part, this.toolResults.get(part.item.id)));
				continue;
			}
			this.appendToolItem(entry, hostedToolRunItem(entry.id, part));
		}
		if (citations.length && !citationsRendered) {
			this.flushGroup();
			this.nodes.push(this.assistantTextNode(entry, "citations", "", citations));
		}
	}

	private assistantTextNode(
		entry: AssistantMessageEntry,
		key: string,
		text: string,
		citations: SourceCitation[]
	): Extract<TranscriptDisplayNode, { type: "assistant_text" }> {
		const step = this.turnByEntryId.get(entry.id)?.modelSteps.find((candidate) => candidate.entry.id === entry.id);
		return {
			type: "assistant_text",
			key: `${entry.id}-${key}`,
			entry,
			text,
			copyText: assistantMessageText(entry.item),
			phase: step?.phase ?? "unknown",
			citations
		};
	}

	private appendToolItem(entry: AssistantMessageEntry, item: ToolRunItem) {
		const turn = this.turnByEntryId.get(entry.id);
		if (!this.pendingGroup) {
			this.pendingGroup = {
				type: "tool_group",
				key: `tool-group-${entry.id}-${item.key}`,
				id: `tool-group-${entry.id}-${item.id}`,
				items: [],
				turnId: turn?.turnId ?? null,
				turnOpen: !turn?.boundaryEntry,
				isLive: false
			};
		}
		this.pendingGroup.items.push(item);
	}

	private flushGroup() {
		if (!this.pendingGroup) return;
		this.nodes.push(this.pendingGroup);
		this.pendingGroup = null;
	}

	private appendPendingCompactions() {
		for (const action of this.pendingActions) {
			if (action.kind !== "compaction") continue;
			if (action.status !== "running" && action.status !== "pending") continue;
			const trigger = action.payload.trigger === "manual" ? "manual" : "auto";
			const reason = typeof action.payload.reason === "string" ? action.payload.reason : null;
			this.nodes.push({
				type: "compaction_in_progress",
				key: `compaction-pending-${action.action_row_id}`,
				trigger,
				reason
			});
		}
	}

	private appendPendingToolRuns() {
		const pendingTools = this.pendingActions
			.filter((action) => action.kind === "tool" && action.status === "running")
			.map((action) => toolRunItemFromPendingAction(action))
			.filter((item): item is ToolRunItem => !!item);
		if (!pendingTools.length) return;
		const lastNode = this.nodes.at(-1);
		if (lastNode?.type === "tool_group" && lastNode.turnOpen) {
			const existingTools = new Set(lastNode.items.map((item) => `${item.rawName}:${item.id}`));
			lastNode.items.push(...pendingTools.filter((item) => !existingTools.has(`${item.rawName}:${item.id}`)));
			return;
		}
		const lastAssistant = [...this.nodes].reverse().find((node) => node.type === "assistant_text") as Extract<TranscriptDisplayNode, { type: "assistant_text" }> | undefined;
		const turn = lastAssistant ? this.turnByEntryId.get(lastAssistant.entry.id) : undefined;
		this.nodes.push({
			type: "tool_group",
			key: `tool-group-pending-${pendingTools[0].id}`,
			id: `tool-group-pending-${pendingTools[0].id}`,
			items: pendingTools,
			turnId: turn?.turnId ?? null,
			turnOpen: true,
			isLive: true
		});
	}
}

function toolRunItemFromPendingAction(action: PendingAction): ToolRunItem | null {
	const payload = action.payload;
	const id = stringValue(payload.id) ?? stringValue(payload.tool_call_id) ?? action.action_row_id;
	const toolName = stringValue(payload.tool_name) ?? stringValue(payload.name) ?? "tool";
	const argsJson = typeof payload.args_json === "string" ? payload.args_json : JSON.stringify(payload);
	const input = parseToolInput(argsJson) ?? (payload && typeof payload === "object" && !Array.isArray(payload) ? payload : null);
	const prettyName = prettyToolName(toolName);
	const editPreview = editToolPreview(toolName, input);
	return {
		source: "local",
		key: `pending-${action.action_row_id}`,
		entryId: action.action_row_id,
		id,
		rawName: toolName,
		prettyName: editPreview ? "Edit" : prettyName,
		title: editPreview?.header ?? formatDisplayHeader(prettyName, inputSummaryFromInput(toolName, input)),
		statusKind: "running",
		statusLabel: "running",
		argsJson,
		input,
		editPreview
	};
}

function markLiveToolGroups(nodes: TranscriptDisplayNode[]): TranscriptDisplayNode[] {
	const liveGroupByTurn = new Map<number | "none", string>();
	for (const node of nodes) {
		if (node.type !== "tool_group" || !node.turnOpen) continue;
		liveGroupByTurn.set(node.turnId ?? "none", node.id);
	}
	return nodes.map((node) => {
		if (node.type !== "tool_group" || !node.turnOpen) return node;
		return { ...node, isLive: liveGroupByTurn.get(node.turnId ?? "none") === node.id };
	});
}

function localToolRunItem(
	entryId: string,
	part: Extract<AssistantRenderPart, { type: "tool_call" }>,
	result?: ToolResultItem
): ToolRunItem {
	const input = parseToolInput(part.item.args_json);
	const statusKind = !result ? "running" : result.status === "Success" ? "success" : "error";
	const editPreview = editToolPreview(part.item.tool_name, input, result);
	const prettyName = part.display?.pretty_name ?? prettyToolName(part.item.tool_name);
	return {
		source: "local",
		key: `local-${entryId}-${part.item.id}`,
		entryId,
		id: part.item.id,
		rawName: part.item.tool_name,
		prettyName: editPreview ? "Edit" : prettyName,
		title: editPreview?.header ?? formatDisplayHeader(prettyName, part.display?.input_summary ?? inputSummaryFromInput(part.item.tool_name, input)),
		statusKind,
		statusLabel: result ? result.status.toLowerCase() : "running",
		argsJson: part.item.args_json,
		display: part.display,
		result,
		input,
		editPreview
	};
}

function hostedToolRunItem(entryId: string, part: Extract<AssistantRenderPart, { type: "hosted_tool" }>): ToolRunItem {
	const statusKind = part.tool.status === "completed" ? "success" : part.tool.status === "error" ? "error" : "running";
	return {
		source: "hosted",
		key: `hosted-${entryId}-${part.tool.id}`,
		entryId,
		id: part.tool.id,
		rawName: part.tool.name,
		prettyName: part.tool.prettyName,
		title: formatDisplayHeader(part.tool.prettyName, part.tool.inputSummary),
		statusKind,
		statusLabel: part.tool.status,
		tool: part.tool
	};
}

function nodeLeafId(node: TranscriptDisplayNode): string {
	if (node.type === "tool_group") return node.items.at(-1)?.entryId ?? node.id;
	if (node.type === "compaction_in_progress") return node.key;
	return node.entry.id;
}

function prettyToolName(toolName: string): string {
	switch (toolName) {
		case "Edit":
			return "Edit";
		case "Bash":
			return "Bash";
		case "Grep":
			return "Grep";
		case "web_search":
			return "Web search";
		case "web_fetch":
			return "Web fetch";
		case "open_page":
			return "Open page";
		default:
			return toolName;
	}
}

function inputSummaryFromInput(toolName: string, input: Record<string, unknown> | null): string | null {
	if (!input) return null;
	if (toolName === "Bash") {
		const command = input.command;
		if (typeof command === "string") return firstLine(command);
		if (Array.isArray(command)) return firstLine(command.filter((part): part is string => typeof part === "string").join(" "));
	}
	if (toolName === "Grep") return [stringValue(input.pattern), stringValue(input.path)].filter(Boolean).join(" ") || null;
	if (toolName === "Edit" && typeof input.command === "string") return [stringValue(input.command), stringValue(input.path)].filter(Boolean).join(" ") || null;
	return null;
}

function AssistantCopyButton({ text }: { text: string }) {
	const [copied, setCopied] = useState(false);
	const copy = () => {
		void navigator.clipboard
			?.writeText(text)
			.then(() => {
				setCopied(true);
				window.setTimeout(() => setCopied(false), 1200);
			})
			.catch(() => undefined);
	};
	return (
		<button type="button" className="assistant-copy-button" onClick={copy} title="Copy assistant message" aria-label="Copy assistant message">
			{copied ? <Check size={13} /> : <Copy size={13} />}
		</button>
	);
}

const MERMAID_LANGUAGE_CLASS = "language-mermaid";

function isMermaidLanguageClass(className: unknown): boolean {
	if (typeof className !== "string") return false;
	return className.split(/\s+/).includes(MERMAID_LANGUAGE_CLASS);
}

function preWrapsMermaid(node: unknown): boolean {
	if (!node || typeof node !== "object") return false;
	const children = (node as { children?: Array<{ tagName?: string; properties?: { className?: unknown } }> }).children;
	if (!Array.isArray(children) || children.length === 0) return false;
	const first = children[0];
	if (first?.tagName !== "code") return false;
	const className = first.properties?.className;
	if (Array.isArray(className)) {
		return className.some((entry) => entry === MERMAID_LANGUAGE_CLASS);
	}
	return isMermaidLanguageClass(className);
}

function codeChildrenToString(children: ReactNode): string {
	if (typeof children === "string") return children;
	if (Array.isArray(children)) return children.map(codeChildrenToString).join("");
	if (children && typeof children === "object" && "props" in children) {
		const inner = (children as { props?: { children?: ReactNode } }).props?.children;
		return codeChildrenToString(inner);
	}
	return "";
}

export const markdownComponents: Components = {
	a: ({ href, children, ...props }) => (
		<a href={href} target="_blank" rel="noreferrer" {...props}>
			{children}
		</a>
	),
	code: ({ node: _node, className, children, ...props }) => {
		if (isMermaidLanguageClass(className)) {
			const source = codeChildrenToString(children).replace(/\n$/, "");
			return <MermaidBlock code={source} />;
		}
		return (
			<code className={className} {...props}>
				{children}
			</code>
		);
	},
	pre: ({ node, children, ...props }) => {
		if (preWrapsMermaid(node)) {
			// The child <code> override already returns a MermaidBlock; render
			// it directly so the SVG isn't trapped inside a <pre>.
			return <>{children}</>;
		}
		return (
			<pre {...props}>
				{children}
			</pre>
		);
	}
};

const MarkdownText = memo(function MarkdownText({ text }: { text: string }) {
	return (
		<div className="assistant-markdown">
			<ReactMarkdown
				rehypePlugins={[rehypeRaw, [rehypeHighlight, { detect: true }]]}
				remarkPlugins={[remarkGfm]}
				components={markdownComponents}
			>
				{text}
			</ReactMarkdown>
		</div>
	);
});

function CitationList({ citations }: { citations: ReturnType<typeof citationsFromReplay> }) {
	return (
		<div className="citation-list" aria-label="Sources">
			<span className="citation-label">Sources</span>
			{citations.map((citation, index) => (
				<a
					className="citation-chip"
					href={citation.url}
					key={citation.id}
					target="_blank"
					rel="noreferrer"
					title={citation.citedText ?? citation.title}
				>
					<span className="citation-index">{index + 1}</span>
					<span className="citation-title">{citation.title}</span>
				</a>
			))}
		</div>
	);
}

const AssistantTextBlock = memo(function AssistantTextBlock({ node }: { node: Extract<TranscriptDisplayNode, { type: "assistant_text" }> }) {
	return (
		<div className="message-row assistant-row">
			<div className={`assistant-block phase-${node.phase} ${node.copyText ? "has-copy" : ""}`}>
				<div className="assistant-content">
					{node.text ? <MarkdownText text={node.text} /> : null}
					{node.citations.length ? <CitationList citations={node.citations} /> : null}
				</div>
				{node.copyText ? <AssistantCopyButton text={node.copyText} /> : null}
			</div>
		</div>
	);
});

const ToolRunGroup = memo(function ToolRunGroup({ node }: { node: Extract<TranscriptDisplayNode, { type: "tool_group" }> }) {
	// Three-mode card: "collapsed" hides every item, "recent" shows just the
	// last N items with a link to expand, "all" shows every item in a capped
	// scrolling list with a link to shrink back. The default tracks `isLive`
	// (working → recent, done → collapsed); once the user touches anything we
	// stash an override so later state churn (tool statuses changing, new items
	// streaming in) doesn't blow away their selection — that was the existing bug.
	const computedDefault = defaultToolGroupMode(node);
	const [override, setOverride] = useState<ToolGroupMode | null>(null);
	const mode: ToolGroupMode = override ?? computedDefault;

	const status = groupStatus(node.items);
	const totalItems = node.items.length;
	const recentItems = useMemo(() => node.items.slice(-RECENT_TOOL_ROW_COUNT), [node.items]);
	const hiddenCount = Math.max(0, totalItems - recentItems.length);
	const isOpen = mode !== "collapsed";
	const visibleItems = mode === "all" ? node.items : recentItems;
	const scrollItems = mode === "all" && totalItems > 10;

	const handleHeadToggle = useCallback(() => {
		setOverride(isOpen ? "collapsed" : "recent");
	}, [isOpen]);
	const handleShowAll = useCallback(() => setOverride("all"), []);
	const handleShowRecent = useCallback(() => setOverride("recent"), []);

	const icon = status === "running" ? <Loader2 className="spin" size={14} /> : <Check size={14} />;
	const onlyItem = totalItems === 1 ? node.items[0] : null;

	if (onlyItem) {
		return (
			<div className="message-row assistant-row">
				<div className={`tool-card stand-alone single-tool ${onlyItem.statusKind}`}>
					<ToolRunDetailItem item={onlyItem} />
				</div>
			</div>
		);
	}

	return (
		<div className="message-row assistant-row">
			<div className={`tool-run-group ${status} ${mode} ${node.isLive ? "live" : ""}`}>
				<button className="tool-run-group-head" type="button" onClick={handleHeadToggle} aria-expanded={isOpen}>
					<span className="tool-run-status-icon" aria-hidden="true">
						{icon}
					</span>
					<span className="tool-run-head-main">
						<span className="tool-run-title">{groupTitle(node)}</span>
						<span className="tool-run-summary" aria-label="Tool counts">
							{groupSummaryPills(node.items).map((pill) => (
								<span className="tool-run-pill" key={pill}>{pill}</span>
							))}
						</span>
					</span>
					<ChevronDown size={14} className={`tool-chevron ${isOpen ? "open" : ""}`} />
				</button>
				{isOpen && totalItems > 0 ? (
					<div className="tool-run-detail">
						{hiddenCount > 0 ? (
							<button
								type="button"
								className="tool-run-show-link"
								onClick={mode === "recent" ? handleShowAll : handleShowRecent}
							>
								<span className="tool-run-detail-icon" aria-hidden="true">…</span>
								<span>
									{mode === "recent"
										? `See ${hiddenCount} other tool ${plural(hiddenCount, "use")}`
										: "Show recent only"}
								</span>
							</button>
						) : null}
						<div className={`tool-run-items ${scrollItems ? "scrollable" : ""}`}>
							{visibleItems.map((item) => (
								<ToolRunDetailItem item={item} key={item.key} />
							))}
						</div>
					</div>
				) : null}
			</div>
		</div>
	);
});

const ToolRunDetailItem = memo(function ToolRunDetailItem({ item, defaultExpanded = false }: { item: ToolRunItem; defaultExpanded?: boolean }) {
	const [expanded, setExpanded] = useState(defaultExpanded);
	const isExpandable = item.source === "local" ? !!item.editPreview || !!item.input || !!item.result : !!item.tool.input || !!item.tool.output;
	const rowStyle = isExpandable ? undefined : { gridTemplateColumns: "24px minmax(0, 1fr) auto" };
	const icon =
		item.statusKind === "error" ? (
			<AlertTriangle size={13} />
		) : item.statusKind === "running" ? (
			<Loader2 className="spin" size={13} />
		) : (
			<Check size={13} />
		);
	return (
		<div className={`tool-run-item ${item.statusKind} ${expanded ? "expanded" : ""}`}>
			<button
				className="tool-run-item-toggle"
				type="button"
				onClick={() => (isExpandable ? setExpanded((open) => !open) : undefined)}
				aria-expanded={isExpandable ? expanded : undefined}
				disabled={!isExpandable}
				style={rowStyle}
			>
				<span className="tool-run-item-icon" aria-hidden="true">
					{icon}
				</span>
				<span className="tool-run-item-title">{item.title}</span>
				<span className="tool-run-item-status">{isEditToolRunItem(item) && item.statusKind === "success" ? "diff" : item.statusLabel}</span>
				{isExpandable ? <ChevronDown size={13} className={`tool-chevron ${expanded ? "open" : ""}`} /> : null}
			</button>
			{expanded && isExpandable ? (
				<div className="tool-run-item-body">
					{item.source === "local" ? <LocalToolRunBody item={item} /> : <HostedToolRunBody item={item} />}
				</div>
			) : null}
		</div>
	);
});

function LocalToolRunBody({ item }: { item: Extract<ToolRunItem, { source: "local" }> }) {
	const result = item.result;
	const showResultOutput = result && (!item.editPreview?.hideSuccessOutput || result.status !== "Success");
	return (
		<>
			{item.editPreview ? (
				<EditToolView preview={item.editPreview} />
			) : item.input ? (
				<div className="tool-section">
					<div className="tool-section-label">input</div>
					<pre>{JSON.stringify(item.input, null, 2)}</pre>
				</div>
			) : null}
			{showResultOutput ? (
				<div className="tool-section">
					<div className="tool-section-label">output</div>
					<ToolOutput result={result} />
				</div>
			) : !item.result ? (
				<div className="tool-pending">waiting for tool result</div>
			) : null}
			{item.editPreview ? null : <div className="tool-call-id">id {item.id}</div>}
		</>
	);
}

function HostedToolRunBody({ item }: { item: Extract<ToolRunItem, { source: "hosted" }> }) {
	const failed = item.tool.status === "error";
	return (
		<>
			{item.tool.input ? (
				<div className="tool-section">
					<div className="tool-section-label">input</div>
					<pre>{JSON.stringify(item.tool.input, null, 2)}</pre>
				</div>
			) : null}
			{item.tool.output ? (
				<div className="tool-section">
					<div className="tool-section-label">output</div>
					<pre className={failed ? "tool-output-error" : ""}>{item.tool.output}</pre>
				</div>
			) : null}
			<div className="tool-call-id">
				{item.tool.provider} hosted tool {item.tool.id}
			</div>
		</>
	);
}

type ToolGroupMode = "collapsed" | "recent" | "all";

function defaultToolGroupMode(node: Extract<TranscriptDisplayNode, { type: "tool_group" }>): ToolGroupMode {
	// Working: keep the last N visible so users can watch progress.
	// Done: collapse so the transcript stays calm.
	if (node.isLive) return "recent";
	if (groupStatus(node.items) === "running") return "recent";
	return "collapsed";
}

function groupStatus(items: ToolRunItem[]): "complete" | "running" {
	return items.some((item) => item.statusKind === "running") ? "running" : "complete";
}

function groupTitle(node: Extract<TranscriptDisplayNode, { type: "tool_group" }>): string {
	if (node.isLive || groupStatus(node.items) === "running") return "Agent is working";
	return `Used ${node.items.length} ${plural(node.items.length, "tool")}`;
}

function groupSummaryPills(items: ToolRunItem[]): string[] {
	const status = groupStatus(items);
	if (status === "running") {
		const completed = items.filter((item) => item.statusKind !== "running").length;
		const running = items.length - completed;
		return [`${items.length} ${plural(items.length, "tool")}`, `${completed} completed`, `${running} running`];
	}
	const counts = new Map<string, number>();
	for (const item of items) counts.set(item.prettyName, (counts.get(item.prettyName) ?? 0) + 1);
	return [...counts.entries()]
		.sort((left, right) => right[1] - left[1] || left[0].localeCompare(right[0]))
		.slice(0, 5)
		.map(([name, count]) => `${count} ${name}`);
}

function isEditToolRunItem(item: ToolRunItem): item is Extract<ToolRunItem, { source: "local" }> & { editPreview: EditToolPreview } {
	return item.source === "local" && !!item.editPreview;
}

function formatCompactionTokens(tokens: number): string {
	if (tokens >= 1_000_000) return `${(tokens / 1_000_000).toFixed(1)}M`;
	if (tokens >= 1_000) return `${(tokens / 1_000).toFixed(1)}k`;
	return tokens.toString();
}

function plural(count: number, singular: string, pluralForm = `${singular}s`): string {
	return count === 1 ? singular : pluralForm;
}

const ToolResultCard = memo(function ToolResultCard({ item, entryId }: { item: Extract<TranscriptItem, { type: "tool_result" }>; entryId: string }) {
	const runItem = localToolRunItem(
		entryId,
		{
			type: "tool_call",
			key: `orphan-${item.tool_call_id}`,
			item: { type: "tool_call", id: item.tool_call_id, tool_name: item.tool_name, args_json: "{}" }
		},
		item
	);
	return (
		<div className="message-row tool-row">
			<EntryId entryId={entryId} />
			<div className="tool-card stand-alone">
				<ToolRunDetailItem item={runItem} />
			</div>
		</div>
	);
});

export interface EditToolPreview {
	header: string;
	action: string;
	file: string | null;
	additions: number;
	deletions: number;
	kind: "diff" | "file";
	rows?: EditDiffRow[];
	content?: string;
	hideSuccessOutput: boolean;
}

interface EditDiffRow {
	kind: "add" | "remove" | "context";
	text: string;
}

export function editToolPreview(
	toolName: string,
	input: Record<string, unknown> | null,
	result?: ToolResultItem
): EditToolPreview | null {
	if (isApplyPatchEdit(toolName, input)) {
		const patch = stringValue(input?.input) ?? result?.output ?? null;
		return patch ? applyPatchPreview(patch) : null;
	}

	if (!isTextEditorEdit(toolName, input)) return null;
	const command = stringValue(input.command);
	const path = stringValue(input.path);
	if (command === "view") {
		return {
			header: path ? `Read ${baseName(path)}` : "Read file",
			action: "Read",
			file: path,
			additions: 0,
			deletions: 0,
			kind: "file",
			content: result?.output ?? "",
			hideSuccessOutput: true
		};
	}
	if (command === "create") {
		const fileText = stringValue(input.file_text);
		return fileText == null || path == null
			? null
			: diffPreview("Edited", path, linesToRows(fileText, "add"), true);
	}
	if (command === "str_replace") {
		const oldStr = stringValue(input.old_str);
		const newStr = stringValue(input.new_str) ?? "";
		return oldStr == null || path == null
			? null
			: diffPreview("Edited", path, [...linesToRows(oldStr, "remove"), ...linesToRows(newStr, "add")], true);
	}
	if (command === "insert") {
		const newStr = stringValue(input.new_str);
		const insertLine = typeof input.insert_line === "number" ? input.insert_line : null;
		return path == null || newStr == null
			? null
			: diffPreview(`Edited after line ${insertLine ?? "?"}`, path, linesToRows(newStr, "add"), true);
	}
	return null;
}

function isApplyPatchEdit(toolName: string, input: Record<string, unknown> | null): boolean {
	return toolName === "Edit" && typeof input?.input === "string";
}

function isTextEditorEdit(toolName: string, input: Record<string, unknown> | null): input is Record<string, unknown> {
	return (
		toolName === "Edit" &&
		!!input &&
		typeof input.command === "string" &&
		typeof input.path === "string"
	);
}

function EditToolView({ preview }: { preview: EditToolPreview }) {
	return (
		<div className="edit-preview">
			<div className="edit-preview-head">
				<span className="edit-preview-action">{preview.action}</span>
				{preview.file ? (
					<span className="edit-preview-file" title={preview.file}>
						{baseName(preview.file)}
					</span>
				) : null}
				{preview.additions ? <span className="edit-count add">+{preview.additions}</span> : null}
				{preview.deletions ? <span className="edit-count remove">-{preview.deletions}</span> : null}
			</div>
			{preview.kind === "file" ? (
				<pre className="tool-file">{preview.content || "(empty)"}</pre>
			) : (
				<div className="edit-diff">
					{(preview.rows ?? []).map((row, index) => (
						<div className={`edit-diff-row ${row.kind}`} key={`${row.kind}-${index}`}>
							<span className="edit-diff-marker">{diffMarker(row.kind)}</span>
							<span className="edit-diff-text">{row.text || " "}</span>
						</div>
					))}
				</div>
			)}
		</div>
	);
}

function applyPatchPreview(patch: string): EditToolPreview {
	const lines = patch.split("\n");
	const file = patchFile(lines);
	const rows: EditDiffRow[] = [];
	for (const line of lines) {
		if (!line || line.startsWith("*** Begin Patch") || line.startsWith("*** End Patch") || line.startsWith("*** Add File:") || line.startsWith("*** Update File:") || line.startsWith("*** Delete File:")) continue;
		if (line.startsWith("+")) rows.push({ kind: "add", text: line.slice(1) });
		else if (line.startsWith("-")) rows.push({ kind: "remove", text: line.slice(1) });
		else rows.push({ kind: "context", text: line });
	}
	return diffPreview("Edited", file, rows, true);
}

function diffPreview(action: string, file: string | null, rows: EditDiffRow[], hideSuccessOutput: boolean): EditToolPreview {
	const additions = rows.filter((row) => row.kind === "add").length;
	const deletions = rows.filter((row) => row.kind === "remove").length;
	const counts = `${additions ? ` +${additions}` : ""}${deletions ? ` -${deletions}` : ""}`;
	return {
		header: `${action} ${file ? baseName(file) : "file"}${counts}`,
		action,
		file,
		additions,
		deletions,
		kind: "diff",
		rows,
		hideSuccessOutput
	};
}

function patchFile(lines: string[]): string | null {
	for (const line of lines) {
		const match = line.match(/^\*\*\* (?:Add|Update|Delete) File: (.+)$/);
		if (match) return match[1];
	}
	return null;
}

function linesToRows(text: string, kind: EditDiffRow["kind"]): EditDiffRow[] {
	const withoutFinalNewline = text.endsWith("\n") ? text.slice(0, -1) : text;
	const lines = withoutFinalNewline.length ? withoutFinalNewline.split("\n") : [""];
	return lines.map((line) => ({ kind, text: line }));
}

function diffMarker(kind: EditDiffRow["kind"]): string {
	if (kind === "add") return "+";
	if (kind === "remove") return "-";
	return " ";
}

export function ToolOutput({ result }: { result: ToolResultItem }) {
	const output = result.output || "(empty)";
	return <pre className={result.status === "Success" ? "" : "tool-output-error"}>{output}</pre>;
}

function SystemMessage({
	tone,
	text,
	entryId,
	action,
	loading
}: {
	tone: NoticeTone;
	text: string;
	entryId?: string;
	action?: { label: string; disabled?: boolean; onClick: () => void };
	loading?: boolean;
}) {
	return (
		<div className={`system-message ${tone}`}>
			{entryId ? <EntryId entryId={entryId} inline /> : null}
			{loading ? <Loader2 className="spin" size={12} /> : null}
			<span>{text}</span>
			{action ? (
				<button type="button" className="system-message-action" onClick={action.onClick} disabled={action.disabled}>
					<RotateCcw size={12} />
					{action.label}
				</button>
			) : null}
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

function stringValue(value: unknown): string | null {
	return typeof value === "string" ? value : null;
}

function baseName(path: string): string {
	return path.split(/[\\/]/).filter(Boolean).at(-1) ?? path;
}

function formatDisplayHeader(prettyName: string, inputSummary: string | null): string {
	return inputSummary ? `${prettyName}: ${firstLine(inputSummary)}` : prettyName;
}
