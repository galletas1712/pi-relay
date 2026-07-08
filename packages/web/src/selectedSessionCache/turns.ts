import { displayParentIdForEntry } from "../displayParent.ts";
import type { TranscriptEntry, TranscriptItem, TranscriptTurnsResult, TurnCard } from "../types.ts";
import { mergeEntryBodies, selectedEntriesFromIds, sameStringArray, uniqueStringArray } from "./entries.ts";
import type { SelectedSessionCache } from "./types.ts";

export function applyTranscriptTurns(
	cache: SelectedSessionCache,
	result: TranscriptTurnsResult,
	options: { mode?: "replace" | "prepend" } = {},
): SelectedSessionCache {
	if (cache.sessionId !== result.session_id) return cache;
	const mode = options.mode ?? "replace";
	if (
		mode === "prepend" &&
		(cache.turnTranscriptRevision !== result.transcript_revision ||
			cache.turnActiveLeafId !== result.active_leaf_id ||
			cache.turnBeforeEntryId !== (result.before_entry_id ?? null))
	) {
		return cache;
	}
	if (mode === "replace" && isStaleTranscriptTurnsResult(cache, result)) return cache;
	let entriesById = cache.entriesById;
	const incomingCardsById = new Map<string, TurnCard>();
	for (const card of result.cards) {
		const cardEntries = [...card.user_messages, ...(card.assistant_message ? [card.assistant_message] : [])];
		entriesById = mergeEntryBodies(entriesById, cardEntries);
		incomingCardsById.set(card.id, {
			...card,
			user_messages: card.user_messages.map((entry) => entriesById.get(entry.id) ?? entry),
			assistant_message: card.assistant_message ? entriesById.get(card.assistant_message.id) ?? card.assistant_message : card.assistant_message,
		});
	}
	const orderedIds = mode === "prepend"
		? uniqueStringArray([...result.cards.map((card) => card.id), ...cache.turnOrder])
		: result.cards.map((card) => card.id);
	const turnCardsById = mode === "prepend"
		? new Map([...cache.turnCardsById, ...incomingCardsById])
		: incomingCardsById;
	const turnDetailsById = new Map<string, string[]>();
	for (const cardId of orderedIds) {
		const card = turnCardsById.get(cardId);
		if (!card) continue;
		const entryIds =
			cache.turnDetailsById.get(card.id) ??
			(card.start_entry_id ? cache.turnDetailsById.get(card.start_entry_id) : undefined);
		if (entryIds && turnDetailCoversCard(entryIds, card)) turnDetailsById.set(card.id, entryIds);
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
		turnOrder: orderedIds,
		turnDetailsById,
		turnTranscriptRevision: result.transcript_revision,
		turnActiveLeafId: result.active_leaf_id,
		turnHasMoreBefore: result.has_more_before,
		turnBeforeEntryId: result.next_before_entry_id ?? null,
	};
}

function turnDetailCoversCard(entryIds: string[], card: TurnCard): boolean {
	return (entryIds.at(-1) ?? null) === card.active_leaf_id;
}

function isStaleTranscriptTurnsResult(cache: SelectedSessionCache, result: TranscriptTurnsResult): boolean {
	const snapshotRevision = cache.snapshot?.transcript_revision ?? null;
	const knownRevision = Math.max(cache.turnTranscriptRevision ?? -1, snapshotRevision ?? -1);
	if (knownRevision >= 0 && result.transcript_revision < knownRevision) return true;
	const knownActiveLeafId = cache.snapshot?.active_leaf_id ?? cache.turnActiveLeafId;
	if (
		result.transcript_revision === knownRevision &&
		knownActiveLeafId !== undefined &&
		knownActiveLeafId !== result.active_leaf_id
	) {
		return true;
	}
	return false;
}

export interface ApplyTurnDetailResult {
	cache: SelectedSessionCache;
	applied: boolean;
}

export function applyTurnDetail(cache: SelectedSessionCache, sessionId: string, turnId: string, entries: TranscriptEntry[]): ApplyTurnDetailResult {
	if (cache.sessionId !== sessionId) return { cache, applied: false };
	const card = cache.turnCardsById.get(turnId);
	const lastEntryId = entries.at(-1)?.id ?? null;
	if (!card || !lastEntryId) return { cache, applied: false };
	const acceptsPartialOpenTurn = card.status === "open";
	if (lastEntryId !== card.active_leaf_id && !acceptsPartialOpenTurn) return { cache, applied: false };
	const entriesById = mergeEntryBodies(cache.entriesById, entries);
	const turnDetailsById = new Map(cache.turnDetailsById);
	turnDetailsById.set(turnId, extendTurnDetailEntryIds(entries.map((entry) => entry.id), card, entriesById));
	return {
		cache: {
			...cache,
			entriesById,
			turnDetailsById,
		},
		applied: true,
	};
}

function extendTurnDetailEntryIds(entryIds: string[], card: TurnCard, entriesById: Map<string, TranscriptEntry>): string[] {
	const ids = [...entryIds];
	const seenIds = new Set(ids);
	let currentLeafId = ids.at(-1) ?? null;
	while (currentLeafId && currentLeafId !== card.active_leaf_id) {
		const child = findOnlyDisplayChild(currentLeafId, entriesById);
		if (!child || seenIds.has(child.id)) break;
		ids.push(child.id);
		seenIds.add(child.id);
		currentLeafId = child.id;
	}
	return ids;
}

function findOnlyDisplayChild(parentId: string, entriesById: Map<string, TranscriptEntry>): TranscriptEntry | null {
	let child: TranscriptEntry | null = null;
	for (const entry of entriesById.values()) {
		if (displayParentIdForEntry(entry) !== parentId) continue;
		if (child) return null;
		child = entry;
	}
	return child;
}

export function turnCardsInOrder(cache: SelectedSessionCache): TurnCard[] {
	return cache.turnOrder.flatMap((id) => {
		const card = cache.turnCardsById.get(id);
		return card && card.status !== "compacted" ? [card] : [];
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

export function appendLoadedTurnDetail(
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

export function migrateCurrentTurnDetailId(
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

export function appendTurnCard(
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
		if (previousCard.status !== "open") return { turnCardsById: currentCards, turnOrder: currentOrder };
		const nextCard = updateTurnCard(previousCard, entry);
		const nextId = turnCardStableId(nextCard);
		const turnCardsById = new Map(currentCards);
		turnCardsById.delete(previousCard.id);
		turnCardsById.set(nextId, { ...nextCard, id: nextId });
		const turnOrder = sameLastId(currentOrder, previousCard.id)
			? [...currentOrder.slice(0, -1), nextId]
			: currentOrder.map((id) => (id === previousCard.id ? nextId : id));
		return { turnCardsById, turnOrder };
	}

	const startsNewTurn = entry.item.type === "turn_started" && previousCard.start_entry_id !== entry.id;
	if (startsNewTurn) {
		const nextCard = updateTurnCard(initialTurnCard(entry), entry);
		const turnCardsById = new Map(currentCards);
		turnCardsById.set(nextCard.id, nextCard);
		return { turnCardsById, turnOrder: [...currentOrder, nextCard.id] };
	}
	if (previousCard.status === "compacted") {
		const nextCard = updateTurnCard(initialTurnCardFromCompactionResume(previousCard, entry), entry);
		const nextId = turnCardStableId(nextCard);
		const turnCardsById = new Map(currentCards);
		turnCardsById.set(nextId, { ...nextCard, id: nextId });
		return { turnCardsById, turnOrder: [...currentOrder, nextId] };
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
	if (entry.item.type === "compaction_summary") {
		return { turnCardsById: new Map(), turnOrder: [] };
	}
	const card = updateTurnCard(initialTurnCard(entry), entry);
	const stableId = turnCardStableId(card);
	const normalizedCard = { ...card, id: stableId };
	return {
		turnCardsById: new Map([[normalizedCard.id, normalizedCard]]),
		turnOrder: [normalizedCard.id],
	};
}

function initialTurnCard(entry: TranscriptEntry): TurnCard {
	const turnId = turnIdForItem(entry.item);
	return {
		id: entry.id,
		turn_id: turnId,
		status: "open",
		outcome: null,
		start_entry_id: entry.item.type === "turn_started" ? entry.id : null,
		boundary_entry_id: null,
		active_leaf_id: entry.id,
		start_sequence: entry.sequence ?? 0,
		end_sequence: entry.sequence ?? 0,
		start_timestamp_ms: entry.timestamp_ms,
		timestamp_ms: entry.timestamp_ms,
		user_messages: [],
		daemon_observations: [],
		assistant_message: null,
		summary: null,
		can_resume: false,
	};
}

function initialTurnCardFromCompactionResume(compactionCard: TurnCard, entry: TranscriptEntry): TurnCard {
	return {
		id: entry.id,
		turn_id: turnIdForItem(entry.item) ?? compactionCard.turn_id ?? null,
		status: "open",
		outcome: null,
		start_entry_id: null,
		boundary_entry_id: null,
		active_leaf_id: entry.id,
		start_sequence: entry.sequence ?? 0,
		end_sequence: entry.sequence ?? 0,
		start_timestamp_ms: compactionCard.start_timestamp_ms,
		timestamp_ms: entry.timestamp_ms,
		user_messages: [],
		daemon_observations: [],
		assistant_message: null,
		summary: null,
		can_resume: false,
	};
}

function updateTurnCard(card: TurnCard, entry: TranscriptEntry): TurnCard {
	const item = entry.item;
	let next: TurnCard = {
		...card,
		active_leaf_id: entry.id,
		end_sequence: entry.sequence ?? card.end_sequence,
		timestamp_ms: entry.timestamp_ms,
		turn_id: card.turn_id ?? turnIdForItem(item),
	};

	if (item.type === "turn_started") {
		next = {
			...next,
			turn_id: item.turn_id,
			start_entry_id: next.start_entry_id ?? entry.id,
			start_sequence: next.start_entry_id ? next.start_sequence : entry.sequence ?? next.start_sequence,
		};
	} else if (item.type === "user_message" && !item.replayed_after_compaction) {
		next = {
			...next,
			user_messages: appendUniqueEntry(next.user_messages, entry),
		};
	} else if (item.type === "assistant_message") {
		next = {
			...next,
			assistant_message: entry,
		};
	} else if (item.type === "daemon_tool_observation") {
		next = {
			...next,
			daemon_observations: appendUniqueEntry(next.daemon_observations ?? [], entry),
		};
	} else if (item.type === "tool_call_started") {
		next = {
			...next,
			turn_id: item.turn_id,
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
	} else if (item.type === "compaction_summary") {
		next = {
			...next,
			turn_id: item.last_turn_id,
			start_timestamp_ms: typeof item.turn_started_at_ms === "number" ? item.turn_started_at_ms : next.start_timestamp_ms,
		};
	}
	return next;
}

function turnCardStableId(card: TurnCard): string {
	return card.boundary_entry_id ?? card.start_entry_id ?? card.active_leaf_id;
}

function appendUniqueEntry(entries: TranscriptEntry[], entry: TranscriptEntry): TranscriptEntry[] {
	const index = entries.findIndex((candidate) => candidate.id === entry.id);
	if (index === -1) return [...entries, entry];
	const next = [...entries];
	next[index] = entry;
	return next;
}

function turnIdForItem(item: TranscriptItem): number | null {
	if (item.type === "turn_started" || item.type === "turn_finished" || item.type === "tool_call_started") return item.turn_id;
	if (item.type === "compaction_summary") return item.last_turn_id;
	return null;
}

function sameLastId(ids: string[], expectedLastId: string): boolean {
	return ids.at(-1) === expectedLastId;
}
