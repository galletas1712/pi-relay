import { branchEntriesFor } from "./historyTargets.ts";
import { displayParentIdForEntry, displayParentIdForNode } from "./displayParent.ts";
import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type {
	EventFrame,
	QueueProjection,
	QueuedInput,
	ActiveBranchSyncResponse,
	SessionSnapshot,
	TranscriptEntry,
	TranscriptItem,
	TranscriptTreeIndex,
	TranscriptTreeNode,
	TranscriptTurnsResult,
	TurnCard,
} from "./types.ts";

export interface SelectedSessionCache {
	sessionId: string | null;
	snapshot: SessionSnapshot | null;
	activeBranchEntryIds: string[];
	entriesById: Map<string, TranscriptEntry>;
	treeNodesById: Map<string, TranscriptTreeNode>;
	treeChildrenByParentId: Map<string | null, string[]>;
	treeOrder: string[];
	treeActiveLeafId: string | null;
	treeTranscriptRevision: number | null;
	treeLoadedPrefixSequence: number;
	treeMaxSequence: number;
	treeComplete: boolean;
	turnCardsById: Map<string, TurnCard>;
	turnOrder: string[];
	turnDetailsById: Map<string, string[]>;
	turnTranscriptRevision: number | null;
	turnActiveLeafId: string | null;
}

export function applyTranscriptTurns(cache: SelectedSessionCache, result: TranscriptTurnsResult): SelectedSessionCache {
	if (cache.sessionId !== result.session_id) return cache;
	const entriesById = cache.entriesById;
	const turnCardsById = new Map<string, TurnCard>();
	for (const card of result.cards) turnCardsById.set(card.id, card);
	const turnDetailsById = new Map<string, string[]>();
	for (const card of result.cards) {
		const entryIds =
			cache.turnDetailsById.get(card.id) ??
			(card.start_entry_id ? cache.turnDetailsById.get(card.start_entry_id) : undefined);
		if (entryIds) turnDetailsById.set(card.id, entryIds);
	}
	const activeBranchEntryIds = result.active_leaf_id ? [result.active_leaf_id] : [];
	const snapshot = cache.snapshot
		? {
				...cache.snapshot,
				active_leaf_id: result.active_leaf_id,
				has_transcript_entries: result.cards.length > 0,
				session_revision: Math.max(cache.snapshot.session_revision ?? 0, result.session_revision),
				transcript_revision: Math.max(cache.snapshot.transcript_revision ?? 0, result.transcript_revision),
				entries: selectedEntriesFromIds(activeBranchEntryIds, entriesById),
			}
		: cache.snapshot;
	return {
		...cache,
		snapshot,
		activeBranchEntryIds: sameStringArray(cache.activeBranchEntryIds, activeBranchEntryIds)
			? cache.activeBranchEntryIds
			: activeBranchEntryIds,
		entriesById,
		turnCardsById,
		turnOrder: result.cards.map((card) => card.id),
		turnDetailsById,
		turnTranscriptRevision: result.transcript_revision,
		turnActiveLeafId: result.active_leaf_id,
	};
}

export function applyTurnDetail(cache: SelectedSessionCache, sessionId: string, turnId: string, entries: TranscriptEntry[]): SelectedSessionCache {
	if (cache.sessionId !== sessionId) return cache;
	const entriesById = mergeEntryBodies(cache.entriesById, entries);
	const turnDetailsById = new Map(cache.turnDetailsById);
	turnDetailsById.set(turnId, entries.map((entry) => entry.id));
	return {
		...cache,
		entriesById,
		turnDetailsById,
	};
}

export function turnCardsInOrder(cache: SelectedSessionCache): TurnCard[] {
	return cache.turnOrder.flatMap((id) => {
		const card = cache.turnCardsById.get(id);
		return card ? [card] : [];
	});
}

export function turnDetailEntries(cache: SelectedSessionCache, turnId: string): TranscriptEntry[] | null {
	const ids = cache.turnDetailsById.get(turnId);
	if (!ids) return null;
	const entries = ids.flatMap((id) => {
		const entry = cache.entriesById.get(id);
		return entry ? [entry] : [];
	});
	return entries.length === ids.length ? entries : null;
}

export function emptySelectedSessionCache(sessionId: string | null = null): SelectedSessionCache {
	return {
		sessionId,
		snapshot: null,
		activeBranchEntryIds: [],
		entriesById: new Map(),
		treeNodesById: new Map(),
		treeChildrenByParentId: new Map(),
		treeOrder: [],
		treeActiveLeafId: null,
		treeTranscriptRevision: null,
		treeLoadedPrefixSequence: 0,
		treeMaxSequence: 0,
		treeComplete: false,
		turnCardsById: new Map(),
		turnOrder: [],
		turnDetailsById: new Map(),
		turnTranscriptRevision: null,
		turnActiveLeafId: null,
	};
}

export function selectedEntries(cache: SelectedSessionCache): TranscriptEntry[] {
	return cache.activeBranchEntryIds.flatMap((id) => {
		const entry = cache.entriesById.get(id);
		return entry ? [entry] : [];
	});
}

export function treeNodesInOrder(cache: SelectedSessionCache): TranscriptTreeNode[] {
	return cache.treeOrder.flatMap((id) => {
		const node = cache.treeNodesById.get(id);
		return node ? [node] : [];
	});
}

export function applySelectedSnapshot(cache: SelectedSessionCache, snapshot: SessionSnapshot): SelectedSessionCache {
	const sameSession = cache.sessionId === snapshot.session_id;
	const base = sameSession ? cache : emptySelectedSessionCache(snapshot.session_id);
	const hasEntryBodies = Array.isArray(snapshot.entries);
	const incomingEntries = snapshot.entries ?? [];
	const entriesById = hasEntryBodies ? mergeEntryBodies(base.entriesById, incomingEntries) : base.entriesById;
	const activeBranchEntryIds = hasEntryBodies
		? entryIds(incomingEntries)
		: activeBranchIdsForSnapshot(base.activeBranchEntryIds, snapshot.active_leaf_id ?? null);
	const snapshotEntries = hasEntryBodies ? incomingEntries : selectedEntriesFromIds(activeBranchEntryIds, entriesById);
	const snapshotTranscriptRevision = snapshot.transcript_revision ?? null;
	const treeRevisionChanged =
		sameSession &&
		base.treeTranscriptRevision !== null &&
		snapshotTranscriptRevision !== null &&
		base.treeTranscriptRevision !== snapshotTranscriptRevision;
	return {
		...base,
		sessionId: snapshot.session_id,
		snapshot: { ...snapshot, entries: snapshotEntries },
		activeBranchEntryIds: sameStringArray(base.activeBranchEntryIds, activeBranchEntryIds) ? base.activeBranchEntryIds : activeBranchEntryIds,
		entriesById,
		treeActiveLeafId: treeRevisionChanged ? snapshot.active_leaf_id : base.treeActiveLeafId ?? snapshot.active_leaf_id,
		treeTranscriptRevision: treeRevisionChanged ? snapshotTranscriptRevision : base.treeTranscriptRevision,
		treeComplete: treeRevisionChanged ? false : base.treeComplete,
	};
}

export function applyEntryBodies(cache: SelectedSessionCache, sessionId: string, entries: TranscriptEntry[]): SelectedSessionCache {
	if (cache.sessionId !== sessionId) return cache;
	const entriesById = mergeEntryBodies(cache.entriesById, entries);
	if (entriesById === cache.entriesById) return cache;
	return {
		...cache,
		entriesById,
		snapshot: cache.snapshot
			? {
					...cache.snapshot,
					entries: selectedEntriesFromIds(cache.activeBranchEntryIds, entriesById),
				}
			: cache.snapshot,
	};
}

export type ActiveBranchSyncApplyResult = "applied" | "reload" | "ignored";

export function applyActiveBranchSyncToCache(
	cache: SelectedSessionCache,
	sync: ActiveBranchSyncResponse,
): { cache: SelectedSessionCache; result: ActiveBranchSyncApplyResult } {
	if (cache.sessionId !== sync.session_id || !cache.snapshot) return { cache, result: "ignored" };
	if (sync.status === "branch_changed") return { cache, result: "reload" };

	const overview = sync.overview;
	let entriesById = cache.entriesById;
	let activeBranchEntryIds = cache.activeBranchEntryIds;
	if (sync.status === "extended") {
		entriesById = mergeEntryBodies(entriesById, sync.entries);
		const appendedEntryIds = appendActiveBranchEntries(cache.activeBranchEntryIds, entriesById, sync.entries);
		if (!appendedEntryIds) return { cache, result: "reload" };
		activeBranchEntryIds = appendedEntryIds;
	}
	if ((activeBranchEntryIds.at(-1) ?? null) !== sync.active_leaf_id) return { cache, result: "reload" };

	const selectedEntryBodies = selectedEntriesFromIds(activeBranchEntryIds, entriesById);
	if (selectedEntryBodies.length !== activeBranchEntryIds.length) return { cache, result: "reload" };
	return {
		cache: {
			...cache,
			snapshot: {
				...cache.snapshot,
				...overview,
				active_leaf_id: sync.active_leaf_id,
				entries: selectedEntryBodies,
			},
			activeBranchEntryIds: sameStringArray(cache.activeBranchEntryIds, activeBranchEntryIds)
				? cache.activeBranchEntryIds
				: activeBranchEntryIds,
			entriesById,
			treeActiveLeafId: sync.active_leaf_id,
		},
		result: "applied",
	};
}

export function applyTreeIndex(cache: SelectedSessionCache, index: TranscriptTreeIndex): SelectedSessionCache {
	if (cache.sessionId !== index.session_id) return cache;
	const pageMatchesRevision = cache.treeTranscriptRevision === index.transcript_revision;
	const pageStartsAtLoadedPrefix = index.after_sequence === cache.treeLoadedPrefixSequence;
	const canApplyPage = index.after_sequence === 0 || (pageMatchesRevision && pageStartsAtLoadedPrefix);
	const shouldReset = index.after_sequence === 0 || !pageMatchesRevision || !pageStartsAtLoadedPrefix;
	const treeNodesById = shouldReset ? new Map<string, TranscriptTreeNode>() : new Map(cache.treeNodesById);
	if (canApplyPage) {
		for (const node of index.nodes) treeNodesById.set(node.id, node);
	}
	const treeOrder = Array.from(treeNodesById.values())
		.sort((left, right) => left.sequence - right.sequence)
		.map((node) => node.id);
	const treeChildrenByParentId = buildTreeChildren(treeOrder, treeNodesById);
	const snapshot = cache.snapshot
		? {
				...cache.snapshot,
				session_revision: Math.max(cache.snapshot.session_revision ?? 0, index.session_revision),
				transcript_revision: Math.max(cache.snapshot.transcript_revision ?? 0, index.transcript_revision),
				entries: cache.snapshot.entries ?? selectedEntries(cache),
			}
		: cache.snapshot;
	return {
		...cache,
		snapshot,
		treeNodesById,
		treeChildrenByParentId,
		treeOrder,
		treeActiveLeafId: canApplyPage ? index.active_leaf_id : cache.treeActiveLeafId,
		treeTranscriptRevision: index.transcript_revision,
		treeLoadedPrefixSequence: canApplyPage ? (index.nodes.at(-1)?.sequence ?? index.after_sequence) : 0,
		treeMaxSequence: canApplyPage ? index.max_sequence : 0,
		treeComplete: canApplyPage ? index.complete : false,
	};
}

export function applyQueueProjection(cache: SelectedSessionCache, sessionId: string, queue: QueueProjection): SelectedSessionCache {
	if (cache.sessionId !== sessionId || !cache.snapshot) return cache;
	if ((cache.snapshot.queue_revision ?? -1) > queue.queue_revision) return cache;
	return {
		...cache,
		snapshot: {
			...cache.snapshot,
			activity: queue.activity,
			queued_inputs: queue.queued_inputs,
			session_revision: queue.session_revision,
			queue_revision: queue.queue_revision,
			transcript_revision: queue.transcript_revision,
			entries: cache.snapshot.entries ?? selectedEntries(cache),
		},
	};
}

export function applyEventHighWater(cache: SelectedSessionCache, sessionId: string, eventId: number): SelectedSessionCache {
	if (cache.sessionId !== sessionId || !cache.snapshot || cache.snapshot.last_event_id >= eventId) return cache;
	return {
		...cache,
		snapshot: {
			...cache.snapshot,
			last_event_id: eventId,
			entries: cache.snapshot.entries ?? selectedEntries(cache),
		},
	};
}

export function mergeSessionActivityEvent(
	cache: SelectedSessionCache,
	sessionId: string,
	eventId: number,
	activity: SessionSnapshot["activity"],
): SelectedSessionCache {
	if (cache.sessionId !== sessionId || !cache.snapshot) return cache;
	return {
		...cache,
		snapshot: {
			...cache.snapshot,
			activity,
			last_event_id: Math.max(cache.snapshot.last_event_id, eventId),
			entries: cache.snapshot.entries ?? selectedEntries(cache),
		},
	};
}

export function applySwitchResultToCache(
	cache: SelectedSessionCache,
	result: {
		session_id: string;
		active_leaf_id: string | null;
		activity?: SessionSnapshot["activity"];
		session_revision?: number;
		queue_revision?: number;
		transcript_revision?: number;
		last_event_id?: number;
		active_branch_entry_ids?: string[] | null;
		active_branch_entries?: TranscriptEntry[] | null;
	},
): SelectedSessionCache {
	if (cache.sessionId !== result.session_id || !cache.snapshot) return cache;
	const entries = result.active_branch_entries ?? null;
	const entriesById = entries ? mergeEntryBodies(cache.entriesById, entries) : cache.entriesById;
	const activeBranchEntryIds = result.active_branch_entry_ids ?? (entries ? entryIds(entries) : cache.activeBranchEntryIds);
	const selectedEntryBodies = selectedEntriesFromIds(activeBranchEntryIds, entriesById);
	return {
		...cache,
		snapshot: {
			...cache.snapshot,
			active_leaf_id: result.active_leaf_id,
			activity: result.activity ?? cache.snapshot.activity,
			session_revision: result.session_revision ?? cache.snapshot.session_revision,
			queue_revision: result.queue_revision ?? cache.snapshot.queue_revision,
			transcript_revision: result.transcript_revision ?? cache.snapshot.transcript_revision,
			last_event_id: result.last_event_id ?? cache.snapshot.last_event_id,
			entries: selectedEntryBodies,
		},
		activeBranchEntryIds: sameStringArray(cache.activeBranchEntryIds, activeBranchEntryIds) ? cache.activeBranchEntryIds : activeBranchEntryIds,
		entriesById,
		treeActiveLeafId: result.active_leaf_id,
	};
}

export type EventApplyResult = "applied" | "refresh" | "ignored";

export function applyTranscriptAppendedEvent(cache: SelectedSessionCache, event: EventFrame): { cache: SelectedSessionCache; result: EventApplyResult } {
	if (cache.sessionId !== event.session_id || !cache.snapshot) return { cache, result: "ignored" };
	const entry = transcriptEntryFromEvent(event);
	const transcriptRevision = numberValue(event.data.transcript_revision);
	const sessionRevision = numberValue(event.data.session_revision);
	const queueRevision = numberValue(event.data.queue_revision);
	const activeLeafId = stringOrNull(event.data.active_leaf_id);
	if (!entry) return { cache, result: "refresh" };
	const currentLeafId = cache.activeBranchEntryIds.at(-1) ?? null;
	const entryDisplayParentId = displayParentIdForEntry(entry);
	const appendsToActiveBranch =
		entryDisplayParentId === currentLeafId || (currentLeafId === null && entryDisplayParentId === null);
	const entriesById = new Map(cache.entriesById);
	entriesById.set(entry.id, entry);
	let activeBranchEntryIds = cache.activeBranchEntryIds;
	if (appendsToActiveBranch) {
		activeBranchEntryIds = [...activeBranchEntryIds, entry.id];
	} else if (activeLeafId && activeLeafId === entry.id) {
		const snapshot: SessionSnapshot = {
			...cache.snapshot,
			active_leaf_id: activeLeafId,
			session_revision: sessionRevision ?? cache.snapshot.session_revision ?? 0,
			queue_revision: queueRevision ?? cache.snapshot.queue_revision ?? 0,
			transcript_revision: transcriptRevision ?? cache.snapshot.transcript_revision ?? 0,
			last_event_id: Math.max(cache.snapshot.last_event_id, event.event_id),
			entries: cache.snapshot.entries ?? selectedEntries(cache),
		};
		return { cache: { ...cache, snapshot, entriesById, ...applyTreeNodeFromEvent(cache, event) }, result: "refresh" };
	}
	const snapshot: SessionSnapshot = {
		...cache.snapshot,
		active_leaf_id: activeLeafId ?? cache.snapshot.active_leaf_id,
		session_revision: sessionRevision ?? cache.snapshot.session_revision ?? 0,
		queue_revision: queueRevision ?? cache.snapshot.queue_revision ?? 0,
		transcript_revision: transcriptRevision ?? cache.snapshot.transcript_revision ?? 0,
		last_event_id: Math.max(cache.snapshot.last_event_id, event.event_id),
		entries: activeBranchEntryIds.map((id) => entriesById.get(id)).filter((candidate): candidate is TranscriptEntry => !!candidate),
	};
	let turnDetailsById = appendsToActiveBranch
		? appendLoadedTurnDetail(cache.turnDetailsById, cache.turnOrder.at(-1) ?? null, currentLeafId, entry.id)
		: cache.turnDetailsById;
	const turnCards = appendsToActiveBranch
		? appendTurnCard(cache.turnCardsById, cache.turnOrder, entry)
		: {
				turnCardsById: cache.turnCardsById,
				turnOrder: cache.turnOrder,
			};
	turnDetailsById = migrateCurrentTurnDetailId(turnDetailsById, cache.turnOrder.at(-1) ?? null, turnCards.turnOrder.at(-1) ?? null);
	const nextCache = {
		...cache,
		snapshot,
		activeBranchEntryIds,
		entriesById,
		turnDetailsById,
		turnCardsById: turnCards.turnCardsById,
		turnOrder: turnCards.turnOrder,
		turnTranscriptRevision: transcriptRevision ?? cache.turnTranscriptRevision,
		turnActiveLeafId: activeLeafId ?? cache.turnActiveLeafId,
		...applyTreeNodeFromEvent(cache, event),
	};
	return { cache: nextCache, result: appendsToActiveBranch ? "applied" : "refresh" };
}

export function branchFromTree(cache: SelectedSessionCache, leafId: string | null): TranscriptTreeNode[] {
	if (!leafId) return [];
	const result: TranscriptTreeNode[] = [];
	const seen = new Set<string>();
	let cursor: string | null = leafId;
	while (cursor && !seen.has(cursor)) {
		const node = cache.treeNodesById.get(cursor);
		if (!node) break;
		result.push(node);
		seen.add(cursor);
		cursor = displayParentIdForNode(node, cache.treeNodesById);
	}
	return result.reverse();
}

export function activeBranchFromTreeBodies(cache: SelectedSessionCache): TranscriptEntry[] {
	const leafId = cache.snapshot?.active_leaf_id ?? null;
	const nodeIds = branchFromTree(cache, leafId).map((node) => node.id);
	return nodeIds.flatMap((id) => {
		const entry = cache.entriesById.get(id);
		return entry ? [entry] : [];
	});
}

export function fullTreeEntriesFromKnownBodies(cache: SelectedSessionCache): TranscriptEntry[] {
	return cache.treeOrder.flatMap((id) => {
		const entry = cache.entriesById.get(id);
		return entry ? [entry] : [];
	});
}

export function activeBranchEntriesForExport(cache: SelectedSessionCache): TranscriptEntry[] {
	const known = fullTreeEntriesFromKnownBodies(cache);
	return known.length > 0 ? branchEntriesFor(known, cache.snapshot?.active_leaf_id ?? null) : selectedEntries(cache);
}

function buildTreeChildren(order: string[], byId: Map<string, TranscriptTreeNode>): Map<string | null, string[]> {
	const children = new Map<string | null, string[]>();
	for (const id of order) {
		const node = byId.get(id);
		if (!node) continue;
		const parentId = displayParentIdForNode(node, byId);
		const siblings = children.get(parentId) ?? [];
		siblings.push(id);
		children.set(parentId, siblings);
	}
	return children;
}

function mergeEntryBodies(current: Map<string, TranscriptEntry>, entries: TranscriptEntry[]): Map<string, TranscriptEntry> {
	let next = current;
	for (const entry of entries) {
		const existing = current.get(entry.id);
		const merged = reusableEntry(existing, entry);
		if (existing === merged) continue;
		if (next === current) next = new Map(current);
		next.set(entry.id, merged);
	}
	return next;
}

function reusableEntry(existing: TranscriptEntry | undefined, incoming: TranscriptEntry): TranscriptEntry {
	if (!existing) return incoming;
	// Transcript rows are append-only and immutable. Reusing existing entry
	// objects when the durable identity matches keeps React transcript rows and
	// scroll bookkeeping stable across canonical `session.get` refreshes.
	if (
		existing.id === incoming.id &&
		existing.parent_id === incoming.parent_id &&
		existing.timestamp_ms === incoming.timestamp_ms &&
		existing.sequence === incoming.sequence
	) {
		return existing;
	}
	return incoming;
}

function entryIds(entries: TranscriptEntry[]): string[] {
	return entries.map((entry) => entry.id);
}

function activeBranchIdsForSnapshot(currentIds: string[], activeLeafId: string | null): string[] {
	if (!activeLeafId) return [];
	return currentIds.at(-1) === activeLeafId ? currentIds : [activeLeafId];
}

function appendActiveBranchEntries(
	currentIds: string[],
	entriesById: Map<string, TranscriptEntry>,
	entries: TranscriptEntry[],
): string[] | null {
	if (entries.length === 0) return currentIds;
	let nextIds = currentIds;
	for (const entry of entries) {
		if (nextIds.includes(entry.id)) continue;
		const currentLeafId = nextIds.at(-1) ?? null;
		if (displayParentIdForEntry(entry, entriesById) !== currentLeafId) return null;
		if (nextIds === currentIds) nextIds = [...currentIds];
		nextIds.push(entry.id);
	}
	return nextIds;
}

function selectedEntriesFromIds(ids: string[], entriesById: Map<string, TranscriptEntry>): TranscriptEntry[] {
	return ids.flatMap((id) => {
		const entry = entriesById.get(id);
		return entry ? [entry] : [];
	});
}

function appendLoadedTurnDetail(
	current: Map<string, string[]>,
	turnId: string | null,
	currentLeafId: string | null,
	entryId: string,
): Map<string, string[]> {
	if (!turnId) return current;
	const ids = current.get(turnId);
	if (!ids || ids.includes(entryId) || (ids.at(-1) ?? null) !== currentLeafId) return current;
	const next = new Map(current);
	next.set(turnId, [...ids, entryId]);
	return next;
}

function migrateCurrentTurnDetailId(
	current: Map<string, string[]>,
	previousTurnId: string | null,
	nextTurnId: string | null,
): Map<string, string[]> {
	if (!previousTurnId || !nextTurnId || previousTurnId === nextTurnId) return current;
	const ids = current.get(previousTurnId);
	if (!ids || current.has(nextTurnId)) return current;
	const next = new Map(current);
	next.delete(previousTurnId);
	next.set(nextTurnId, ids);
	return next;
}

function appendTurnCard(
	currentCards: Map<string, TurnCard>,
	currentOrder: string[],
	entry: TranscriptEntry,
): { turnCardsById: Map<string, TurnCard>; turnOrder: string[] } {
	if (currentOrder.length === 0) {
		return createTurnCardFromEntry(entry);
	}

	const previousCardId = currentOrder.at(-1);
	const previousCard = previousCardId ? currentCards.get(previousCardId) : undefined;
	if (!previousCard) return { turnCardsById: currentCards, turnOrder: currentOrder };

	if (entry.item.type === "compaction_summary") {
		const nextCard = compactionTurnCard(entry);
		const turnCardsById = new Map(currentCards);
		turnCardsById.set(nextCard.id, nextCard);
		return { turnCardsById, turnOrder: [...currentOrder, nextCard.id] };
	}

	const startsNewTurn = entry.item.type === "turn_started" && previousCard.entry_count > 0;
	if (startsNewTurn) {
		const nextCard = updateTurnCard(initialTurnCard(entry), entry);
		const turnCardsById = new Map(currentCards);
		turnCardsById.set(nextCard.id, nextCard);
		return { turnCardsById, turnOrder: [...currentOrder, nextCard.id] };
	}

	const updatedCard = updateTurnCard(previousCard, entry);
	const nextId = turnCardStableId(updatedCard);
	const turnCardsById = new Map(currentCards);
	turnCardsById.delete(previousCard.id);
	turnCardsById.set(nextId, { ...updatedCard, id: nextId });
	const turnOrder = sameLastId(currentOrder, previousCard.id)
		? [...currentOrder.slice(0, -1), nextId]
		: currentOrder.map((id) => (id === previousCard.id ? nextId : id));
	return { turnCardsById, turnOrder };
}

function createTurnCardFromEntry(entry: TranscriptEntry): { turnCardsById: Map<string, TurnCard>; turnOrder: string[] } {
	const card = entry.item.type === "compaction_summary" ? compactionTurnCard(entry) : updateTurnCard(initialTurnCard(entry), entry);
	const stableId = turnCardStableId(card);
	const normalizedCard = { ...card, id: stableId };
	return {
		turnCardsById: new Map([[normalizedCard.id, normalizedCard]]),
		turnOrder: [normalizedCard.id],
	};
}

function initialTurnCard(entry: TranscriptEntry): TurnCard {
	const sequence = entry.sequence ?? 0;
	const turnId = turnIdForItem(entry.item);
	return {
		id: entry.id,
		turn_id: turnId,
		status: "open",
		outcome: null,
		start_entry_id: entry.item.type === "turn_started" ? entry.id : null,
		boundary_entry_id: null,
		active_leaf_id: entry.id,
		start_sequence: sequence,
		end_sequence: sequence,
		start_timestamp_ms: entry.timestamp_ms,
		timestamp_ms: entry.timestamp_ms,
		user_preview: null,
		assistant_preview: null,
		tool_call_count: 0,
		tool_result_count: 0,
		entry_count: 0,
		can_resume: false,
	};
}

function updateTurnCard(card: TurnCard, entry: TranscriptEntry): TurnCard {
	const item = entry.item;
	const sequence = entry.sequence ?? card.end_sequence;
	let next: TurnCard = {
		...card,
		active_leaf_id: entry.id,
		end_sequence: sequence,
		timestamp_ms: entry.timestamp_ms,
		entry_count: card.entry_count + 1,
		turn_id: card.turn_id ?? turnIdForItem(item),
	};

	if (item.type === "turn_started") {
		next = {
			...next,
			turn_id: item.turn_id,
			start_entry_id: next.start_entry_id ?? entry.id,
		};
	} else if (item.type === "user_message") {
		next = {
			...next,
			user_preview: appendPreview(next.user_preview ?? null, contentBlocksToText(item.content)),
		};
	} else if (item.type === "assistant_message") {
		const assistantText = item.items
			.map((assistantItem) => (assistantItem.type === "text" ? assistantItem.text : ""))
			.join("")
			.trim();
		next = {
			...next,
			assistant_preview: assistantText ? truncate(firstLine(assistantText) || assistantText, 240) : next.assistant_preview,
			tool_call_count: next.tool_call_count + item.items.filter((assistantItem) => assistantItem.type === "tool_call").length,
		};
	} else if (item.type === "tool_call_started") {
		next = {
			...next,
			turn_id: item.turn_id,
			tool_call_count: next.tool_call_count + 1,
		};
	} else if (item.type === "tool_result") {
		next = {
			...next,
			tool_result_count: next.tool_result_count + 1,
		};
	} else if (item.type === "turn_finished") {
		next = {
			...next,
			turn_id: item.turn_id,
			status: "completed",
			outcome: item.outcome,
			boundary_entry_id: entry.id,
			can_resume: item.outcome === "Interrupted" || item.outcome === "Crashed",
		};
	}
	return next;
}

function compactionTurnCard(entry: TranscriptEntry): TurnCard {
	const sequence = entry.sequence ?? 0;
	const summary = entry.item.type === "compaction_summary" ? entry.item.summary.trim() : "";
	const turnId = entry.item.type === "compaction_summary" ? entry.item.last_turn_id : null;
	return {
		id: entry.id,
		turn_id: turnId,
		status: "compacted",
		outcome: null,
		start_entry_id: entry.id,
		boundary_entry_id: entry.id,
		active_leaf_id: entry.id,
		start_sequence: sequence,
		end_sequence: sequence,
		start_timestamp_ms: entry.timestamp_ms,
		timestamp_ms: entry.timestamp_ms,
		user_preview: null,
		assistant_preview: summary ? truncate(firstLine(summary) || summary, 240) : null,
		tool_call_count: 0,
		tool_result_count: 0,
		entry_count: 1,
		can_resume: false,
	};
}

function turnCardStableId(card: TurnCard): string {
	return card.boundary_entry_id ?? card.start_entry_id ?? card.active_leaf_id;
}

function appendPreview(current: string | null, next: string): string | null {
	const trimmed = next.trim();
	if (!trimmed) return current;
	const merged = current?.trim() ? `${current.trim()}\n${trimmed}` : trimmed;
	return truncate(merged, 240);
}

function turnIdForItem(item: TranscriptItem): number | null {
	if (item.type === "turn_started" || item.type === "turn_finished" || item.type === "tool_call_started") return item.turn_id;
	if (item.type === "compaction_summary") return item.last_turn_id;
	return null;
}

function sameLastId(ids: string[], expectedLastId: string): boolean {
	return ids.at(-1) === expectedLastId;
}

function sameStringArray(left: string[], right: string[]): boolean {
	if (left.length !== right.length) return false;
	for (let index = 0; index < left.length; index += 1) {
		if (left[index] !== right[index]) return false;
	}
	return true;
}

function applyTreeNodeFromEvent(cache: SelectedSessionCache, event: EventFrame): Partial<SelectedSessionCache> {
	const node = transcriptTreeNodeFromUnknown(event.data.tree_node);
	if (!node) return {};
	const revision = numberValue(event.data.transcript_revision) ?? cache.treeTranscriptRevision;
	const activeLeafId = stringOrNull(event.data.active_leaf_id);
	if (!cache.treeComplete) {
		return {
			treeActiveLeafId: activeLeafId ?? cache.treeActiveLeafId,
			treeTranscriptRevision: revision,
			treeMaxSequence: Math.max(cache.treeMaxSequence, node.sequence),
			treeComplete: false,
		};
	}
	const treeNodesById = new Map(cache.treeNodesById);
	treeNodesById.set(node.id, node);
	const treeOrder = Array.from(treeNodesById.values())
		.sort((left, right) => left.sequence - right.sequence)
		.map((candidate) => candidate.id);
	return {
		treeNodesById,
		treeChildrenByParentId: buildTreeChildren(treeOrder, treeNodesById),
		treeOrder,
		treeActiveLeafId: activeLeafId ?? cache.treeActiveLeafId,
		treeTranscriptRevision: revision,
		treeLoadedPrefixSequence: Math.max(cache.treeLoadedPrefixSequence, node.sequence),
		treeMaxSequence: Math.max(cache.treeMaxSequence, node.sequence),
		treeComplete: cache.treeComplete,
	};
}

function transcriptEntryFromEvent(event: EventFrame): TranscriptEntry | null {
	const value = event.data.entry;
	if (!isRecord(value)) return null;
	if (typeof value.id !== "string") return null;
	if (typeof value.timestamp_ms !== "number") return null;
	if (!isRecord(value.item) || typeof value.item.type !== "string") return null;
	return value as unknown as TranscriptEntry;
}

function transcriptTreeNodeFromUnknown(value: unknown): TranscriptTreeNode | null {
	if (!isRecord(value)) return null;
	if (typeof value.id !== "string") return null;
	if (typeof value.sequence !== "number") return null;
	if (typeof value.item_type !== "string") return null;
	return value as unknown as TranscriptTreeNode;
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function numberValue(value: unknown): number | null {
	return typeof value === "number" && Number.isFinite(value) ? value : null;
}

function stringOrNull(value: unknown): string | null {
	return typeof value === "string" ? value : value === null ? null : null;
}

export function queueProjectionFromEvent(event: EventFrame): QueueProjection | null {
	const data = event.data;
	const sessionRevision = numberValue(data.session_revision);
	const queueRevision = numberValue(data.queue_revision);
	const transcriptRevision = numberValue(data.transcript_revision);
	const activity = data.activity;
	const queuedInputs = data.queued_inputs;
	if (sessionRevision === null || queueRevision === null || transcriptRevision === null) return null;
	if (activity !== "idle" && activity !== "queued" && activity !== "running") return null;
	if (!Array.isArray(queuedInputs)) return null;
	return {
		session_revision: sessionRevision,
		queue_revision: queueRevision,
		transcript_revision: transcriptRevision,
		activity,
		queued_inputs: queuedInputs.filter(isQueuedInput),
	};
}

function isQueuedInput(value: unknown): value is QueuedInput {
	return isRecord(value) && typeof value.input_id === "string" && Array.isArray(value.content);
}
