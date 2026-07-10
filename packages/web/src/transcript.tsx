import {
	memo,
	useCallback,
	useEffect,
	useLayoutEffect,
	useMemo,
	useRef,
	useState,
	type KeyboardEvent as ReactKeyboardEvent,
	type PointerEvent as ReactPointerEvent,
	type ReactNode,
	type UIEvent,
} from "react";
import { AlertTriangle, Check, ChevronDown, ChevronUp, Copy, Loader2, Plus, RotateCcw, Terminal } from "lucide-react";
import rehypeRaw from "rehype-raw";
import rehypeHighlight from "rehype-highlight";
import ReactMarkdown from "react-markdown";
import type { Components } from "react-markdown";
import remarkGfm from "remark-gfm";
import { branchEntriesFor } from "./historyTargets.ts";
import { ConnectionBlockedReason } from "./connectionRecovery.tsx";
import { MermaidBlock } from "./mermaidBlock.tsx";
import { contentBlocksToText, firstLine } from "./text.ts";
import { assistantMessageText, buildTurnViews } from "./turnView.ts";
import type { ModelStepView, TurnView } from "./turnView.ts";
import type { AssistantItem, NoticeTone, PendingAction, TranscriptEntry, TranscriptItem, TurnCard } from "./types.ts";

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
			result?: ToolResultItem;
			input: Record<string, unknown> | null;
			editPreview: EditToolPreview | null;
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
	  }
	| { type: "tool_group"; key: string; id: string; items: ToolRunItem[]; turnId: number | null; turnOpen: boolean; isLive: boolean }
	| { type: "turn_finished"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "turn_finished" }> } }
	| { type: "tool_result"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "tool_result" }> } }
	| { type: "daemon_tool_observation"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "daemon_tool_observation" }> } }
	| { type: "compaction_summary"; key: string; entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "compaction_summary" }> } }
	| { type: "compaction_in_progress"; key: string; trigger: "auto" | "manual"; reason: string | null };

export interface TurnCardView {
	card: TurnCard;
	entries: TranscriptEntry[] | null;
	detailCached?: boolean;
	expanded: boolean;
	isCurrent: boolean;
}

type ScrollMetrics = Pick<HTMLDivElement, "clientHeight" | "scrollHeight" | "scrollTop">;
type TurnJumpDirection = "previous" | "next";
type TurnJumpTarget = { id: string; nodeKey?: string };
const STICKY_BOTTOM_EPSILON_PX = 1;
const TURN_JUMP_EPSILON_PX = 2;
const ACTIVE_SESSION_SCROLL_KEY = "__active_session__";
const TRANSCRIPT_SCROLL_STORAGE_KEY = "piRelayTranscriptScroll:v1";
const RECENT_TOOL_ROW_COUNT = 3;

export interface TurnJumpTargetPosition {
	id: string;
	top: number;
	bottom: number;
}

export function isScrolledAtBottom(node: ScrollMetrics): boolean {
	return node.scrollHeight - node.scrollTop - node.clientHeight <= STICKY_BOTTOM_EPSILON_PX;
}

function bottomScrollTop(node: ScrollMetrics): number {
	return Math.max(0, node.scrollHeight - node.clientHeight);
}

export function adjacentTurnJumpTargetId(
	targets: readonly TurnJumpTargetPosition[],
	scrollTop: number,
	direction: TurnJumpDirection,
	viewportHeight = 0
): string | null {
	const orderedTargets = [...targets].sort((left, right) => left.top - right.top);
	if (direction === "previous") {
		let currentIndex = -1;
		for (const [index, target] of orderedTargets.entries()) {
			if (target.top > scrollTop + TURN_JUMP_EPSILON_PX) break;
			currentIndex = index;
		}
		if (currentIndex === -1) return null;
		const currentTarget = orderedTargets[currentIndex];
		if (!turnJumpTargetIsFullyVisible(currentTarget, scrollTop, viewportHeight) && currentTarget.top < scrollTop - TURN_JUMP_EPSILON_PX) {
			return currentTarget.id;
		}
		return orderedTargets[currentIndex - 1]?.id ?? null;
	}
	for (const target of orderedTargets) {
		if (target.top > scrollTop + TURN_JUMP_EPSILON_PX) return target.id;
	}
	return null;
}

function turnJumpTargetIsFullyVisible(target: TurnJumpTargetPosition, scrollTop: number, viewportHeight: number): boolean {
	return target.top >= scrollTop - TURN_JUMP_EPSILON_PX && target.bottom <= scrollTop + viewportHeight + TURN_JUMP_EPSILON_PX;
}

interface ActiveTranscriptScrollIdentity {
	sessionKey: string | null;
	leafId: string | null;
}

interface PendingOlderTurnsLoad {
	sessionKey: string;
	leafId: string | null;
	requestId: number;
	elementId: string | null;
	viewportOffset: number | null;
	contentHeight: number;
	wasPinned: boolean;
	userCancelled: boolean;
}

interface PointerScrollbarIntent {
	pointerId: number;
	requestId: number;
	scroller: HTMLDivElement;
	initialScrollTop: number;
	removeListeners: () => void;
}

export interface TranscriptDestination {
	id: number;
	sessionId: string;
	targetLeafId: string | null;
	minimumTurnPageHydrationRevision: number;
}

export interface TranscriptTurnPageIdentity {
	sessionId: string;
	leafId: string | null;
	hydrationRevision: number;
}

export interface OlderTurnsLoadRequest {
	requestId: number;
	sessionId: string;
}

export interface OlderTurnsLoadResult extends OlderTurnsLoadRequest {
	status: "committed" | "noop" | "stale" | "failed";
	turnPageHydrationRevision?: number;
}

interface OlderTurnsLoadCompletion extends OlderTurnsLoadRequest {
	status: OlderTurnsLoadResult["status"] | "rejected";
	turnPageHydrationRevision?: number;
}

export function clearAcknowledgedTranscriptDestination(
	current: TranscriptDestination | null,
	destinationId: number,
): TranscriptDestination | null {
	return current?.id === destinationId ? null : current;
}

type TranscriptScrollStorage = Pick<Storage, "removeItem">;

export function removeLegacyTranscriptScroll(storage = browserStorage()): void {
	if (!storage) return;
	try {
		storage.removeItem(TRANSCRIPT_SCROLL_STORAGE_KEY);
	} catch {
		// Storage may be unavailable or blocked. The retired value is never read.
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
	sessionError = null,
	sessionErrorHasUsableCache = false,
	retryingSession = false,
	onRetrySession,
	onNewSession,
	onResumeTurn,
	resumingTurnId,
	resumeBlockedReason,
	remoteReadBlockedReason,
	turnCards,
	onExpandTurn,
	onCollapseTurn,
	loadingTurnId,
	hasOlderTurns,
	loadingOlderTurns,
	onLoadOlderTurns,
	destination = null,
	turnPageIdentity = null,
	onAcknowledgeDestination,
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
	sessionError?: string | null;
	sessionErrorHasUsableCache?: boolean;
	retryingSession?: boolean;
	onRetrySession?: () => void;
	onNewSession?: () => void;
	onResumeTurn?: (entryId: string, outcome: "Interrupted" | "Crashed") => void;
	resumingTurnId?: string | null;
	resumeBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
	turnCards?: TurnCardView[] | null;
	onExpandTurn?: (turnId: string) => void;
	onCollapseTurn?: (turnId: string) => void;
	loadingTurnId?: string | null;
	hasOlderTurns?: boolean;
	loadingOlderTurns?: boolean;
	onLoadOlderTurns?: (request: OlderTurnsLoadRequest) => Promise<OlderTurnsLoadResult>;
	destination?: TranscriptDestination | null;
	turnPageIdentity?: TranscriptTurnPageIdentity | null;
	onAcknowledgeDestination?: (destinationId: number) => void;
}) {
	const scrollRef = useRef<HTMLDivElement | null>(null);
	const contentRef = useRef<HTMLDivElement | null>(null);
	const shouldStickToBottomRef = useRef(true);
	const activeScrollIdentityRef = useRef<ActiveTranscriptScrollIdentity>({
		sessionKey: null,
		leafId: null,
	});
	const pendingLatestSessionKeyRef = useRef<string | null>(null);
	const consumedDestinationIdRef = useRef<number | null>(null);
	const pendingOlderTurnsLoadRef = useRef<PendingOlderTurnsLoad | null>(null);
	const pointerScrollbarIntentRef = useRef<PointerScrollbarIntent | null>(null);
	const olderTurnsRequestIdRef = useRef(0);
	const mountedRef = useRef(false);
	const legacyStorageRemovalAttemptedRef = useRef(false);
	const [olderTurnsLoadCompletion, setOlderTurnsLoadCompletion] = useState<OlderTurnsLoadCompletion | null>(null);
	const scrollSessionKey = hasSession ? (sessionId ?? ACTIVE_SESSION_SCROLL_KEY) : null;
	const entriesBelongToSelectedSession = !hasSession || !sessionId || entriesSessionId === sessionId;
	const effectiveEntries = entriesBelongToSelectedSession ? entries : [];
	const visibleEntries = useMemo(
		() => (hasSession ? branchEntriesFor(effectiveEntries, activeLeafId) : effectiveEntries),
		[activeLeafId, effectiveEntries, hasSession]
	);
	const transcriptContentReady =
		!!scrollSessionKey &&
		entriesBelongToSelectedSession &&
		!loadingSession &&
		(!sessionErrorHasUsableCache ? !sessionError && !retryingSession : true);

	const scrollToBottom = useCallback(() => {
		const node = scrollRef.current;
		if (!node) return;
		node.scrollTop = bottomScrollTop(node);
		shouldStickToBottomRef.current = true;
	}, []);

	const clearPointerScrollbarIntent = useCallback((expected?: PointerScrollbarIntent) => {
		const intent = pointerScrollbarIntentRef.current;
		if (!intent || (expected && intent !== expected)) return;
		pointerScrollbarIntentRef.current = null;
		intent.removeListeners();
	}, []);

	const cancelOlderTurnsPreservationForRequest = useCallback((requestId: number | null) => {
		const pending = pendingOlderTurnsLoadRef.current;
		if (requestId !== null && pending?.requestId !== requestId) return;
		if (requestId === null) clearPointerScrollbarIntent();
		else {
			const intent = pointerScrollbarIntentRef.current;
			if (intent?.requestId === requestId) clearPointerScrollbarIntent(intent);
		}
		if (!pending) return;
		pending.userCancelled = true;
		if (olderTurnsLoadCompletion?.requestId === pending.requestId) {
			pendingOlderTurnsLoadRef.current = null;
		}
	}, [clearPointerScrollbarIntent, olderTurnsLoadCompletion]);

	const cancelOlderTurnsPreservation = useCallback(() => {
		cancelOlderTurnsPreservationForRequest(null);
	}, [cancelOlderTurnsPreservationForRequest]);

	const finishPointerScrollbarIntent = useCallback((intent: PointerScrollbarIntent) => {
		if (pointerScrollbarIntentRef.current !== intent) return;
		const viewportMoved = intent.scroller.scrollTop !== intent.initialScrollTop;
		clearPointerScrollbarIntent(intent);
		if (viewportMoved) cancelOlderTurnsPreservationForRequest(intent.requestId);
	}, [cancelOlderTurnsPreservationForRequest, clearPointerScrollbarIntent]);

	const handlePointerScrollbarIntentEnd = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
		const intent = pointerScrollbarIntentRef.current;
		if (intent?.pointerId === event.pointerId) finishPointerScrollbarIntent(intent);
	}, [finishPointerScrollbarIntent]);

	const handlePointerScrollbarIntentStart = useCallback((event: ReactPointerEvent<HTMLDivElement>) => {
		const scroller = event.currentTarget;
		const pending = pendingOlderTurnsLoadRef.current;
		if (
			!pending ||
			event.target !== scroller ||
			event.button !== 0 ||
			!["mouse", "pen"].includes(event.pointerType) ||
			!isPointerInVerticalScrollbarGutter(scroller, event.clientX, event.clientY)
		) return;
		const ownerWindow = scroller.ownerDocument.defaultView;
		if (!ownerWindow) return;
		clearPointerScrollbarIntent();
		const intent: PointerScrollbarIntent = {
			pointerId: event.pointerId,
			requestId: pending.requestId,
			scroller,
			initialScrollTop: scroller.scrollTop,
			removeListeners: () => {},
		};
		const handlePointerEnd = (releaseEvent: PointerEvent) => {
			if (
				releaseEvent.pointerId === intent.pointerId &&
				pointerScrollbarIntentRef.current === intent
			) {
				finishPointerScrollbarIntent(intent);
			}
		};
		const handleWindowBlur = () => {
			if (pointerScrollbarIntentRef.current === intent) {
				finishPointerScrollbarIntent(intent);
			}
		};
		intent.removeListeners = () => {
			ownerWindow.removeEventListener("pointerup", handlePointerEnd, true);
			ownerWindow.removeEventListener("pointercancel", handlePointerEnd, true);
			ownerWindow.removeEventListener("blur", handleWindowBlur);
		};
		pointerScrollbarIntentRef.current = intent;
		ownerWindow.addEventListener("pointerup", handlePointerEnd, true);
		ownerWindow.addEventListener("pointercancel", handlePointerEnd, true);
		ownerWindow.addEventListener("blur", handleWindowBlur);
	}, [clearPointerScrollbarIntent, finishPointerScrollbarIntent]);

	const collectTurnJumpTargetPositions = useCallback((): TurnJumpTargetPosition[] => {
		const scroller = scrollRef.current;
		if (!scroller) return [];
		const scrollerRect = scroller.getBoundingClientRect();
		return Array.from(scroller.querySelectorAll<HTMLElement>("[data-turn-jump-target-id]"))
			.flatMap((target) => {
				const id = target.dataset.turnJumpTargetId;
				if (!id) return [];
				const targetRect = target.getBoundingClientRect();
				const top = scroller.scrollTop + targetRect.top - scrollerRect.top;
				const bottom = scroller.scrollTop + targetRect.bottom - scrollerRect.top;
				return Number.isFinite(top) && Number.isFinite(bottom) ? [{ id, top, bottom }] : [];
			})
			.sort((left, right) => left.top - right.top);
	}, []);

	const scrollToTurnJumpTarget = useCallback((targetId: string) => {
		const scroller = scrollRef.current;
		if (!scroller) return;
		const target = turnJumpTargetNode(scroller, targetId);
		if (!target) return;
		const scrollerRect = scroller.getBoundingClientRect();
		const targetRect = target.getBoundingClientRect();
		scroller.scrollTop = Math.max(0, scroller.scrollTop + targetRect.top - scrollerRect.top);
		shouldStickToBottomRef.current = false;
		cancelOlderTurnsPreservation();
	}, [cancelOlderTurnsPreservation]);

	const jumpToAdjacentTurn = useCallback((direction: TurnJumpDirection) => {
		const scroller = scrollRef.current;
		if (!scroller) return;
		const targetId = adjacentTurnJumpTargetId(collectTurnJumpTargetPositions(), scroller.scrollTop, direction, scroller.clientHeight);
		if (targetId) scrollToTurnJumpTarget(targetId);
	}, [collectTurnJumpTargetPositions, scrollToTurnJumpTarget]);

	const handleScroll = useCallback((event: UIEvent<HTMLDivElement>) => {
		if (pendingLatestSessionKeyRef.current === activeScrollIdentityRef.current.sessionKey) return;
		const pointerIntent = pointerScrollbarIntentRef.current;
		if (
			pointerIntent?.scroller === event.currentTarget &&
			pointerIntent.initialScrollTop !== event.currentTarget.scrollTop
		) {
			finishPointerScrollbarIntent(pointerIntent);
		}
		shouldStickToBottomRef.current = isScrolledAtBottom(event.currentTarget);
	}, [finishPointerScrollbarIntent]);

	const handleScrollKeyDown = useCallback((event: ReactKeyboardEvent<HTMLDivElement>) => {
		if (
			["ArrowUp", "ArrowDown", "PageUp", "PageDown", "Home", "End"].includes(event.key) ||
			(event.key === " " && event.target === event.currentTarget)
		) {
			cancelOlderTurnsPreservation();
		}
	}, [cancelOlderTurnsPreservation]);

	useEffect(() => {
		mountedRef.current = true;
		if (!legacyStorageRemovalAttemptedRef.current) {
			legacyStorageRemovalAttemptedRef.current = true;
			removeLegacyTranscriptScroll();
		}
		return () => {
			mountedRef.current = false;
			pendingOlderTurnsLoadRef.current = null;
			clearPointerScrollbarIntent();
		};
	}, [clearPointerScrollbarIntent]);

	useLayoutEffect(() => {
		const previous = activeScrollIdentityRef.current;
		const sessionChanged = previous.sessionKey !== scrollSessionKey;
		const leafChanged = !sessionChanged && previous.leafId !== activeLeafId;
		if (previous.sessionKey !== scrollSessionKey) {
			activeScrollIdentityRef.current = { sessionKey: scrollSessionKey, leafId: activeLeafId };
			pendingLatestSessionKeyRef.current = scrollSessionKey;
			pendingOlderTurnsLoadRef.current = null;
			clearPointerScrollbarIntent();
			shouldStickToBottomRef.current = false;
		} else if (leafChanged) {
			activeScrollIdentityRef.current = { ...previous, leafId: activeLeafId };
		}
		if (!scrollSessionKey) {
			pendingLatestSessionKeyRef.current = null;
			shouldStickToBottomRef.current = true;
			return;
		}
		if (!transcriptContentReady) return;
		const destinationReady =
			!!destination &&
			destination.id !== consumedDestinationIdRef.current &&
			destination.sessionId === scrollSessionKey &&
			turnPageIdentity?.sessionId === destination.sessionId &&
			turnPageIdentity.leafId === destination.targetLeafId &&
			turnPageIdentity.hydrationRevision >= destination.minimumTurnPageHydrationRevision;
		const destinationPending =
			!!destination &&
			destination.id !== consumedDestinationIdRef.current &&
			destination.sessionId === scrollSessionKey;
		if (pendingLatestSessionKeyRef.current === scrollSessionKey) {
			if (destinationPending && !destinationReady) return;
			pendingLatestSessionKeyRef.current = null;
			if (destinationReady) {
				consumedDestinationIdRef.current = destination.id;
				onAcknowledgeDestination?.(destination.id);
			}
			scrollToBottom();
			return;
		}
		if (destinationReady) {
			consumedDestinationIdRef.current = destination.id;
			onAcknowledgeDestination?.(destination.id);
			pendingOlderTurnsLoadRef.current = null;
			clearPointerScrollbarIntent();
			scrollToBottom();
			return;
		}
		if (destinationPending) return;
		if (leafChanged && shouldStickToBottomRef.current) scrollToBottom();
	}, [
		activeLeafId,
		clearPointerScrollbarIntent,
		destination,
		isRunning,
		onAcknowledgeDestination,
		scrollSessionKey,
		scrollToBottom,
		transcriptContentReady,
		turnPageIdentity,
		visibleEntries,
	]);

	useLayoutEffect(() => {
		const pending = pendingOlderTurnsLoadRef.current;
		if (!pending) return;
		if (pending.sessionKey !== scrollSessionKey || pending.leafId !== activeLeafId) {
			pendingOlderTurnsLoadRef.current = null;
			const intent = pointerScrollbarIntentRef.current;
			if (intent?.requestId === pending.requestId) clearPointerScrollbarIntent(intent);
		}
	}, [activeLeafId, clearPointerScrollbarIntent, scrollSessionKey]);

	useLayoutEffect(() => {
		const pending = pendingOlderTurnsLoadRef.current;
		const completion = olderTurnsLoadCompletion;
		if (!pending || !completion || completion.requestId !== pending.requestId) return;
		const scroller = scrollRef.current;
		if (!scroller) {
			pendingOlderTurnsLoadRef.current = null;
			const intent = pointerScrollbarIntentRef.current;
			if (intent?.requestId === pending.requestId) clearPointerScrollbarIntent(intent);
			return;
		}
		if (completion.sessionId !== pending.sessionKey || pending.userCancelled) {
			pendingOlderTurnsLoadRef.current = null;
			const intent = pointerScrollbarIntentRef.current;
			if (intent?.requestId === pending.requestId) clearPointerScrollbarIntent(intent);
			return;
		}
		const matchingCommittedPage =
			completion.status === "committed" &&
			completion.turnPageHydrationRevision !== undefined;
		if (matchingCommittedPage) {
			// The callback can settle before React commits the cache update. The
			// parent-provided page identity advances with the matching card DOM.
			if (
				turnPageIdentity?.sessionId !== completion.sessionId ||
				turnPageIdentity.hydrationRevision < completion.turnPageHydrationRevision!
			) return;
		}
		if (pending.wasPinned) {
			if (
				matchingCommittedPage ||
				scroller.scrollHeight > pending.contentHeight ||
				!isScrolledAtBottom(scroller)
			) {
				scrollToBottom();
			} else {
				shouldStickToBottomRef.current = true;
			}
		} else if (
			matchingCommittedPage &&
			pending.elementId !== null &&
			pending.viewportOffset !== null
		) {
			const current = transcriptAnchorNode(scroller, pending.elementId);
			if (current) {
				scroller.scrollTop += viewportOffset(scroller, current) - pending.viewportOffset;
				shouldStickToBottomRef.current = isScrolledAtBottom(scroller);
			}
		}
		pendingOlderTurnsLoadRef.current = null;
		const intent = pointerScrollbarIntentRef.current;
		if (intent?.requestId === pending.requestId) clearPointerScrollbarIntent(intent);
	}, [clearPointerScrollbarIntent, olderTurnsLoadCompletion, scrollToBottom, turnCards, turnPageIdentity]);

	useLayoutEffect(() => {
		if (!hasSession || !transcriptContentReady || typeof ResizeObserver === "undefined") return;
		const scroller = scrollRef.current;
		const content = contentRef.current;
		if (!scroller || !content) return;
		const observer = new ResizeObserver(() => {
			if (pendingLatestSessionKeyRef.current === activeScrollIdentityRef.current.sessionKey) return;
			if (
				destination &&
				destination.id !== consumedDestinationIdRef.current &&
				destination.sessionId === activeScrollIdentityRef.current.sessionKey
			) return;
			if (pendingOlderTurnsLoadRef.current) return;
			if (shouldStickToBottomRef.current) scrollToBottom();
		});
		observer.observe(scroller);
		observer.observe(content);
		return () => observer.disconnect();
	}, [destination, hasSession, scrollToBottom, transcriptContentReady]);
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
		() => (isRunning ? turnCards?.find((turn) => turn.isCurrent)?.card.start_timestamp_ms ?? runningTurnStartMs(visibleEntries) : null),
		[isRunning, turnCards, visibleEntries],
	);
	const shouldUseTurnCards = !!turnCards && turnCards.length > 0;
	const fallbackTurnJumpTargets = useMemo(
		() => buildFallbackTurnJumpTargets(turnViews, visibleDisplayNodes),
		[turnViews, visibleDisplayNodes],
	);
	const turnJumpTargets = useMemo<TurnJumpTarget[]>(
		() => shouldUseTurnCards ? turnCards!.map((turn) => ({ id: turn.card.id })) : fallbackTurnJumpTargets,
		[fallbackTurnJumpTargets, shouldUseTurnCards, turnCards],
	);
	const fallbackTargetIdByNodeKey = useMemo(
		() => new Map(fallbackTurnJumpTargets.flatMap((target) => target.nodeKey ? [[target.nodeKey, target.id]] : [])),
		[fallbackTurnJumpTargets],
	);
	const showTurnJumpControls = turnJumpTargets.length > 1;
	const loadOlderTurns = useCallback(() => {
		if (!onLoadOlderTurns || !scrollSessionKey || pendingOlderTurnsLoadRef.current !== null) return;
		const node = scrollRef.current;
		if (!node) return;
		clearPointerScrollbarIntent();
		const request = {
			requestId: ++olderTurnsRequestIdRef.current,
			sessionId: scrollSessionKey,
		};
		const element = visibleTranscriptAnchor(node);
		const elementId = element?.dataset.transcriptAnchorId ?? null;
		const pending: PendingOlderTurnsLoad = {
			sessionKey: scrollSessionKey,
			leafId: activeLeafId,
			requestId: request.requestId,
			elementId,
			viewportOffset: element ? viewportOffset(node, element) : null,
			contentHeight: node.scrollHeight,
			wasPinned: isScrolledAtBottom(node),
			userCancelled: false,
		};
		pendingOlderTurnsLoadRef.current = pending;
		void onLoadOlderTurns(request).then((result) => {
			if (mountedRef.current && pendingOlderTurnsLoadRef.current === pending) {
				setOlderTurnsLoadCompletion(result);
			}
		}, () => {
			if (mountedRef.current && pendingOlderTurnsLoadRef.current === pending) {
				setOlderTurnsLoadCompletion({ ...request, status: "rejected" });
			}
		});
	}, [activeLeafId, clearPointerScrollbarIntent, onLoadOlderTurns, scrollSessionKey]);

	if (!hasSession) {
		return (
			<div className="message-list-shell">
				<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
					<div className="empty-state">
						<Terminal size={34} />
						<div className="empty-state-title">No session open</div>
						<div className="empty-state-sub">Start a new one, or pick a session from the sidebar.</div>
						{onNewSession ? (
							<button type="button" className="primary-button empty-state-cta" onClick={onNewSession}>
								<Plus size={14} />
								New session
							</button>
						) : null}
					</div>
				</div>
			</div>
		);
	}

	if ((sessionError || retryingSession) && !sessionErrorHasUsableCache) {
		return (
			<div className="message-list-shell">
				<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
					<div className="empty-state load-error-state" role={sessionError ? "alert" : "status"}>
						{sessionError ? <AlertTriangle size={30} aria-hidden /> : <Loader2 className="spin" size={28} aria-hidden />}
						<div className="empty-state-title">{sessionError ? "Couldn’t load session" : "Retrying session"}</div>
						<div className="empty-state-sub">{sessionError ?? "Trying to load the selected session again…"}</div>
						{onRetrySession ? (
							<>
								<button
									type="button"
									className="secondary-button load-error-retry"
									disabled={retryingSession || !!remoteReadBlockedReason}
									aria-busy={retryingSession}
									onClick={onRetrySession}
								>
									{retryingSession ? "Retrying…" : "Retry"}
								</button>
								<ConnectionBlockedReason reason={remoteReadBlockedReason} />
							</>
						) : null}
					</div>
				</div>
			</div>
		);
	}

	if (loadingSession || !entriesBelongToSelectedSession) {
		return (
			<div className="message-list-shell">
				<div className="message-scroll" ref={scrollRef} onScroll={handleScroll}>
					<div className="empty-state">
						<Loader2 className="spin" size={28} />
						<span>Loading conversation…</span>
					</div>
				</div>
			</div>
		);
	}

	return (
		<div className={`message-list-shell ${showTurnJumpControls ? "with-turn-jump-controls" : ""}`}>
			<div
				className="message-scroll"
				ref={scrollRef}
				role="region"
				aria-label="Conversation transcript"
				tabIndex={0}
				onScroll={handleScroll}
				onWheel={cancelOlderTurnsPreservation}
				onTouchMove={cancelOlderTurnsPreservation}
				onPointerDown={handlePointerScrollbarIntentStart}
				onPointerUp={handlePointerScrollbarIntentEnd}
				onPointerCancel={handlePointerScrollbarIntentEnd}
				onLostPointerCapture={handlePointerScrollbarIntentEnd}
				onKeyDown={handleScrollKeyDown}
			>
				<div className="message-scroll-content" ref={contentRef}>
					{sessionError || retryingSession ? (
						<div className="load-error-banner transcript-load-error" role={sessionError ? "alert" : "status"}>
							{sessionError ? <AlertTriangle size={18} aria-hidden /> : <Loader2 className="spin" size={18} aria-hidden />}
							<div>
								<strong>{sessionError ? "Session refresh failed" : "Refreshing session"}</strong>
								<span>{sessionError ?? "Trying to refresh the selected session again…"}</span>
							</div>
							{onRetrySession ? (
								<>
									<button
										type="button"
										className="secondary-button load-error-retry"
										disabled={retryingSession || !!remoteReadBlockedReason}
										aria-busy={retryingSession}
										onClick={onRetrySession}
									>
										{retryingSession ? "Retrying…" : "Retry"}
									</button>
									<ConnectionBlockedReason reason={remoteReadBlockedReason} />
								</>
							) : null}
						</div>
					) : null}
					{shouldUseTurnCards
						? (
								<>
									{hasOlderTurns ? (
										<div className="turn-card-load-older">
											<button
												type="button"
												className="turn-card-expand"
												disabled={loadingOlderTurns || !!remoteReadBlockedReason}
												onClick={loadOlderTurns}
											>
												{loadingOlderTurns ? "Loading older…" : "Load older turns"}
											</button>
											<ConnectionBlockedReason reason={remoteReadBlockedReason} />
										</div>
									) : null}
									{turnCards!.map((turn) => (
										<TurnCardRow
											key={turn.card.id}
											turn={turn}
											pendingActions={turn.isCurrent ? pendingActions : []}
											activeLeafId={activeLeafId}
											isRunning={isRunning}
											onResumeTurn={onResumeTurn}
											resumingTurnId={resumingTurnId}
											resumeBlockedReason={resumeBlockedReason}
											remoteReadBlockedReason={remoteReadBlockedReason}
											onExpandTurn={onExpandTurn}
											onCollapseTurn={onCollapseTurn}
											loadingTurnId={loadingTurnId}
											turnJumpTargetId={turn.card.id}
											transcriptAnchorId={turn.card.id}
										/>
									))}
								</>
							)
						: visibleDisplayNodes.map((node) => {
								const targetId = fallbackTargetIdByNodeKey.get(node.key);
								const view = (
									<TranscriptDisplayNodeView
										key={node.key}
										node={node}
										toolIndex={toolIndex}
										isActiveLeaf={nodeLeafId(node) === activeLeafId}
										isRunning={isRunning}
										onResumeTurn={onResumeTurn}
										resumeEntryId={resumeEntryIdByNode.get(node.key) ?? nodeLeafId(node)}
										resuming={resumeEntryIdByNode.get(node.key) === resumingTurnId}
										resumeBlockedReason={resumeBlockedReason}
										compactionHiddenCount={compactionHiddenCounts.get(node.key) ?? 0}
										compactionExpanded={!collapsedCompactions.has(node.key)}
										onToggleCompaction={toggleCompaction}
									/>
								);
								return targetId ? (
									<div key={node.key} className="turn-jump-target" data-turn-jump-target-id={targetId}>
										{view}
									</div>
								) : (
									view
								);
							})}
					{isRunning && workingStartMs != null && serverTimeMs != null ? (
						<WorkingIndicator startMs={workingStartMs} serverTimeMs={serverTimeMs} />
					) : null}
				</div>
			</div>
			<TurnJumpControls visible={showTurnJumpControls} onJump={jumpToAdjacentTurn} />
		</div>
	);
});

function buildFallbackTurnJumpTargets(turns: TurnView[], displayNodes: TranscriptDisplayNode[]): TurnJumpTarget[] {
	const targets: TurnJumpTarget[] = [];
	for (const [index, turn] of turns.entries()) {
		const entryIds = new Set(turn.entries.map((entry) => entry.id));
		const displayNode = displayNodes.find((node) => entryIds.has(nodeLeafId(node)));
		if (displayNode) targets.push({ id: `turn-${index}-${displayNode.key}`, nodeKey: displayNode.key });
	}
	return targets;
}

function turnJumpTargetNode(scroller: HTMLDivElement, targetId: string): HTMLElement | null {
	return Array.from(scroller.querySelectorAll<HTMLElement>("[data-turn-jump-target-id]"))
		.find((target) => target.dataset.turnJumpTargetId === targetId) ?? null;
}

function transcriptAnchorNode(scroller: HTMLElement, anchorId: string): HTMLElement | null {
	return Array.from(scroller.querySelectorAll<HTMLElement>("[data-transcript-anchor-id]"))
		.find((candidate) => candidate.dataset.transcriptAnchorId === anchorId) ?? null;
}

function visibleTranscriptAnchor(scroller: HTMLElement): HTMLElement | null {
	const scrollerRect = scroller.getBoundingClientRect();
	const candidates = Array.from(scroller.querySelectorAll<HTMLElement>("[data-transcript-anchor-id]"));
	return candidates.find((candidate) => {
		const rect = candidate.getBoundingClientRect();
		return rect.bottom > scrollerRect.top && rect.top < scrollerRect.bottom && rect.height > 0;
	}) ?? null;
}

function viewportOffset(scroller: HTMLElement, element: HTMLElement): number {
	return element.getBoundingClientRect().top - scroller.getBoundingClientRect().top;
}

function isPointerInVerticalScrollbarGutter(
	scroller: HTMLElement,
	clientX: number,
	clientY: number,
): boolean {
	if (
		scroller.scrollHeight <= scroller.clientHeight ||
		!Number.isFinite(clientX) ||
		!Number.isFinite(clientY)
	) return false;
	const rect = scroller.getBoundingClientRect();
	if (rect.width <= 0 || rect.height <= 0 || scroller.offsetWidth <= 0) return false;
	const style = scroller.ownerDocument.defaultView?.getComputedStyle(scroller);
	const borderLeft = cssPixels(style?.borderLeftWidth);
	const borderRight = cssPixels(style?.borderRightWidth);
	const leftScrollbarWidth = Math.max(0, scroller.clientLeft - borderLeft);
	const rightScrollbarWidth = Math.max(
		0,
		scroller.offsetWidth - scroller.clientWidth - scroller.clientLeft - borderRight,
	);
	if (leftScrollbarWidth <= 0 && rightScrollbarWidth <= 0) return false;
	const horizontalScale = rect.width / scroller.offsetWidth;
	const verticalScale = scroller.offsetHeight > 0 ? rect.height / scroller.offsetHeight : 1;
	const gutterTop = rect.top + scroller.clientTop * verticalScale;
	const gutterBottom = gutterTop + scroller.clientHeight * verticalScale;
	const leftGutterStart = rect.left + borderLeft * horizontalScale;
	const leftGutterEnd = leftGutterStart + leftScrollbarWidth * horizontalScale;
	const rightGutterEnd = rect.right - borderRight * horizontalScale;
	const rightGutterStart = rightGutterEnd - rightScrollbarWidth * horizontalScale;
	return (
		clientY >= gutterTop &&
		clientY <= gutterBottom &&
		(
			(leftScrollbarWidth > 0 && clientX >= leftGutterStart && clientX <= leftGutterEnd) ||
			(rightScrollbarWidth > 0 && clientX >= rightGutterStart && clientX <= rightGutterEnd)
		)
	);
}

function cssPixels(value: string | undefined): number {
	const parsed = Number.parseFloat(value ?? "");
	return Number.isFinite(parsed) ? parsed : 0;
}

const TurnJumpControls = memo(function TurnJumpControls({
	visible,
	onJump,
}: {
	visible: boolean;
	onJump: (direction: TurnJumpDirection) => void;
}) {
	if (!visible) return null;
	return (
		<div className="turn-jump-controls" aria-label="Turn navigation">
			<button
				type="button"
				className="turn-jump-button"
				aria-label="Jump to previous turn"
				title="Previous turn"
				onClick={() => onJump("previous")}
			>
				<ChevronUp size={18} />
			</button>
			<button
				type="button"
				className="turn-jump-button"
				aria-label="Jump to next turn"
				title="Next turn"
				onClick={() => onJump("next")}
			>
				<ChevronDown size={18} />
			</button>
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
	const clock = previous?.startMs === startMs && previous.serverAnchorMs >= serverTimeMs
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
	}, [serverTimeMs, startMs]);
	return (
		<div className="working-indicator">
			<span className="working-indicator-dot" aria-hidden="true" />
			<span className="working-indicator-label">Working ({formatElapsed(elapsedMs)})…</span>
		</div>
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
	resumeBlockedReason,
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
	resumeBlockedReason?: string | null;
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
								label: resuming ? "Starting…" : actionLabel,
								disabled: resuming || !!resumeBlockedReason,
								disabledReason: resumeBlockedReason,
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
	if (node.type === "daemon_tool_observation") {
		return <DaemonObservationSystemMessage entry={node.entry} />;
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

function delegationIdFromDaemonObservation(item: Extract<TranscriptItem, { type: "daemon_tool_observation" }>): string | null {
	try {
		const args = JSON.parse(item.args_json) as unknown;
		if (isRecord(args) && typeof args.delegation_id === "string") return args.delegation_id;
	} catch {
		// Ignore malformed historical args; the raw JSON remains available in transcript data.
	}
	const result = item.result_json;
	if (isRecord(result) && typeof result.delegation_id === "string") return result.delegation_id;
	return null;
}

function delegationStatusFromDaemonObservation(item: Extract<TranscriptItem, { type: "daemon_tool_observation" }>): string | null {
	const result = item.result_json;
	if (isRecord(result) && typeof result.status === "string") return result.status;
	return null;
}

function daemonObservationSystemText(item: Extract<TranscriptItem, { type: "daemon_tool_observation" }>): string {
	const delegationId = delegationIdFromDaemonObservation(item);
	const status = delegationStatusFromDaemonObservation(item);
	const parts = [item.summary?.trim() || `Daemon observed ${item.tool_name}`];
	if (delegationId && !parts[0].includes(delegationId)) parts.push(`delegation ${delegationId}`);
	if (status) parts.push(`status ${status}`);
	return parts.join(" · ");
}

function DaemonObservationSystemMessage({
	entry,
}: {
	entry: TranscriptEntry & { item: Extract<TranscriptItem, { type: "daemon_tool_observation" }> };
}) {
	return <SystemMessage tone="info" text={daemonObservationSystemText(entry.item)} entryId={entry.id} />;
}

const TurnCardRow = memo(function TurnCardRow({
	turn,
	pendingActions,
	activeLeafId,
	isRunning,
	onResumeTurn,
	resumingTurnId,
	resumeBlockedReason,
	remoteReadBlockedReason,
	onExpandTurn,
	onCollapseTurn,
	loadingTurnId,
	turnJumpTargetId,
	transcriptAnchorId,
}: {
	turn: TurnCardView;
	pendingActions?: PendingAction[];
	activeLeafId: string | null;
	isRunning: boolean;
	onResumeTurn?: (entryId: string, outcome: "Interrupted" | "Crashed") => void;
	resumingTurnId?: string | null;
	resumeBlockedReason?: string | null;
	remoteReadBlockedReason?: string | null;
	onExpandTurn?: (turnId: string) => void;
	onCollapseTurn?: (turnId: string) => void;
	loadingTurnId?: string | null;
	turnJumpTargetId?: string;
	transcriptAnchorId?: string;
}) {
	const card = turn.card;
	const isLoading = loadingTurnId === card.id;
	const detailEntries = turn.expanded ? turn.entries : null;
	const isExpanded = !!detailEntries;
	const detailLoadBlockedReason =
		!isExpanded && !turn.detailCached ? remoteReadBlockedReason : null;
	const canToggleDetails = card.status !== "compacted" && (!!onExpandTurn || !!onCollapseTurn) && !(turn.isCurrent && isExpanded);
	const canResume = card.can_resume && card.active_leaf_id === activeLeafId && !isRunning && !!onResumeTurn;
	const resumableOutcome = card.outcome === "Interrupted" || card.outcome === "Crashed" ? card.outcome : null;
	const visibleUserMessages = card.user_messages.filter(
		(entry) => entry.item.type !== "user_message" || !entry.item.replayed_after_compaction,
	);
	const firstUserMessageId = visibleUserMessages.at(0)?.id ?? null;
	const rootTurnJumpTargetId = isExpanded || !firstUserMessageId ? turnJumpTargetId : undefined;
	const detailLabel = isExpanded ? "Hide details" : isLoading ? "Loading…" : "Show details";
	const onToggleDetails = () => {
		if (isExpanded) onCollapseTurn?.(card.id);
		else onExpandTurn?.(card.id);
	};
	const summaryUserMessages = isExpanded ? [] : visibleUserMessages;
	const summaryDaemonObservations = isExpanded ? [] : card.daemon_observations ?? [];
	let detailRows: ReactNode = null;
	if (detailEntries) {
		const toolIndex = indexToolEntries(detailEntries);
		const turnViews = buildTurnViews(detailEntries);
		const displayNodes = turnDetailDisplayNodesBeforeLatestAssistant(deriveTranscriptDisplayNodes(detailEntries, turnViews, toolIndex.results, pendingActions), turn.card);
		const resumeEntryIdByNode = new Map(displayNodes.map((node) => [node.key, nodeLeafId(node)]));
		detailRows = displayNodes.map((node) => (
			<TranscriptDisplayNodeView
				key={node.key}
				node={node}
				toolIndex={toolIndex}
				isActiveLeaf={nodeLeafId(node) === activeLeafId}
				isRunning={isRunning}
				onResumeTurn={onResumeTurn}
				resumeEntryId={resumeEntryIdByNode.get(node.key) ?? nodeLeafId(node)}
				resuming={resumeEntryIdByNode.get(node.key) === resumingTurnId}
				compactionHiddenCount={0}
				compactionExpanded
				onToggleCompaction={() => {}}
			/>
		));
	}
	return (
		<div
			className={["turn-summary", turn.isCurrent ? "current" : null, card.status, isExpanded ? "expanded" : null].filter(Boolean).join(" ")}
			data-turn-jump-target-id={rootTurnJumpTargetId}
			data-transcript-anchor-id={transcriptAnchorId}
		>
			{summaryUserMessages.map((entry) =>
				entry.item.type === "user_message" ? (
					<UserBubble
						key={entry.id}
						entryId={entry.id}
						item={entry.item}
						turnJumpTargetId={entry.id === firstUserMessageId ? turnJumpTargetId : undefined}
					/>
				) : null,
			)}
			{summaryDaemonObservations.map((entry) =>
				entry.item.type === "daemon_tool_observation" ? (
					<DaemonObservationSystemMessage
						key={entry.id}
						entry={entry as TranscriptEntry & { item: Extract<TranscriptItem, { type: "daemon_tool_observation" }> }}
					/>
				) : null,
			)}
			{detailRows}
			{canToggleDetails || canResume ? (
				<div className="turn-detail-toggle-row">
					{canToggleDetails ? (
						<>
							<button
								type="button"
								className="link-button"
								disabled={(isLoading && !isExpanded) || !!detailLoadBlockedReason}
								onClick={onToggleDetails}
							>
								{detailLabel}
							</button>
							<ConnectionBlockedReason reason={detailLoadBlockedReason} />
						</>
					) : null}
					{canResume ? <ConnectionBlockedReason reason={resumeBlockedReason} /> : null}
					{canResume && !turn.isCurrent && resumableOutcome ? (
						<button
							type="button"
							className="turn-card-expand"
							disabled={resumingTurnId === card.active_leaf_id || !!resumeBlockedReason}
							onClick={() => onResumeTurn?.(card.active_leaf_id, resumableOutcome)}
						>
							{resumingTurnId === card.active_leaf_id ? "Starting…" : resumableOutcome === "Interrupted" ? "Continue" : "Retry"}
						</button>
					) : null}
				</div>
			) : null}
			<TurnSummaryAssistant turn={turn} />
			<TurnDuration card={card} />
		</div>
	);
});

function turnDetailDisplayNodesBeforeLatestAssistant(displayNodes: TranscriptDisplayNode[], card: TurnCard): TranscriptDisplayNode[] {
	const assistantId = card.assistant_message?.id ?? null;
	return displayNodes.filter((node) => {
		if (assistantId && node.type === "assistant_text" && node.entry.id === assistantId) return false;
		if (node.type === "turn_finished") return false;
		return true;
	});
}

const TurnSummaryAssistant = memo(function TurnSummaryAssistant({ turn }: { turn: TurnCardView }) {
	const card = turn.card;
	if (card.status === "compacted" && card.summary) {
		return <div className="turn-card-assistant">{card.summary}</div>;
	}
	if (card.assistant_message?.item.type !== "assistant_message") return null;
	return (
		<AssistantTextBlock
			node={{
				type: "assistant_text",
				key: `${card.assistant_message.id}-turn-card`,
				entry: card.assistant_message as AssistantMessageEntry,
				text: assistantMessageText(card.assistant_message.item),
				copyText: assistantMessageText(card.assistant_message.item),
				phase: card.status === "completed" && card.outcome === "Graceful" ? "final_answer" : turn.isCurrent ? "running" : "unknown",
			}}
		/>
	);
});

function TurnDuration({ card }: { card: TurnCard }) {
	if (card.status !== "completed" || card.timestamp_ms <= card.start_timestamp_ms) return null;
	return <SystemMessage tone="info" text={`Worked for ${formatElapsed(card.timestamp_ms - card.start_timestamp_ms)}`} />;
}

const UserBubble = memo(function UserBubble({
	item,
	entryId,
	turnJumpTargetId
}: {
	item: Extract<TranscriptItem, { type: "user_message" }>;
	entryId: string;
	turnJumpTargetId?: string;
}) {
	return (
		<div className="message-row user-row" data-turn-jump-target-id={turnJumpTargetId}>
			<EntryId entryId={entryId} />
			<div className="user-bubble">{contentBlocksToText(item.content)}</div>
		</div>
	);
});

export type AssistantRenderPart =
	| { type: "text"; key: string; item: Extract<AssistantItem, { type: "text" }> }
	| { type: "tool_call"; key: string; item: Extract<AssistantItem, { type: "tool_call" }> };

export function assistantRenderParts(items: AssistantItem[]): AssistantRenderPart[] {
	return coalesceAdjacentTextItems(items).map((item, index) => itemRenderPart(item, `item-${index}`));
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
		turns: TurnView[],
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
			if (item.replayed_after_compaction) return;
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
		if (item.type === "daemon_tool_observation") {
			this.flushGroup();
			this.nodes.push({ type: "daemon_tool_observation", key: entry.id, entry: entry as Extract<TranscriptDisplayNode, { type: "daemon_tool_observation" }>["entry"] });
			return;
		}
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
		const parts = assistantRenderParts(entry.item.items);
		for (const part of parts) {
			if (part.type === "text") {
				if (!part.item.text) continue;
				this.flushGroup();
				this.nodes.push(this.assistantTextNode(entry, part.key, part.item.text));
				continue;
			}
			this.appendToolItem(entry, localToolRunItem(entry.id, part, this.toolResults.get(part.item.id)));
		}
	}

	private assistantTextNode(
		entry: AssistantMessageEntry,
		key: string,
		text: string
	): Extract<TranscriptDisplayNode, { type: "assistant_text" }> {
		const step = this.turnByEntryId.get(entry.id)?.modelSteps.find((candidate) => candidate.entry.id === entry.id);
		return {
			type: "assistant_text",
			key: `${entry.id}-${key}`,
			entry,
			text,
			copyText: assistantMessageText(entry.item),
			phase: step?.phase ?? "unknown",
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
	const prettyName = prettyToolName(part.item.tool_name);
	return {
		source: "local",
		key: `local-${entryId}-${part.item.id}`,
		entryId,
		id: part.item.id,
		rawName: part.item.tool_name,
		prettyName: editPreview ? "Edit" : prettyName,
		title: editPreview?.header ?? formatDisplayHeader(prettyName, inputSummaryFromInput(part.item.tool_name, input)),
		statusKind,
		statusLabel: result ? result.status.toLowerCase() : "running",
		argsJson: part.item.args_json,
		result,
		input,
		editPreview
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

const AssistantTextBlock = memo(function AssistantTextBlock({ node }: { node: Extract<TranscriptDisplayNode, { type: "assistant_text" }> }) {
	return (
		<div className="message-row assistant-row">
			<div className={`assistant-block phase-${node.phase} ${node.copyText ? "has-copy" : ""}`}>
				<div className="assistant-content">
					{node.text ? <MarkdownText text={node.text} /> : null}
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
	const isExpandable = !!item.editPreview || !!item.input || !!item.result;
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
					<LocalToolRunBody item={item} />
				</div>
			) : null}
		</div>
	);
});

function LocalToolRunBody({ item }: { item: ToolRunItem }) {
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

function isEditToolRunItem(item: ToolRunItem): item is ToolRunItem & { editPreview: EditToolPreview } {
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
	action?: { label: string; disabled?: boolean; disabledReason?: string | null; onClick: () => void };
	loading?: boolean;
}) {
	return (
		<div className={`system-message ${tone}`}>
			{entryId ? <EntryId entryId={entryId} inline /> : null}
			{loading ? <Loader2 className="spin" size={12} /> : null}
			<span>{text}</span>
			{action ? (
				<>
					<button type="button" className="system-message-action" onClick={action.onClick} disabled={action.disabled}>
						<RotateCcw size={12} />
						{action.label}
					</button>
					<ConnectionBlockedReason reason={action.disabledReason} />
				</>
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
