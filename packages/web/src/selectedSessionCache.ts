import { branchEntriesFor } from "./historyTargets.ts";
import type {
	EventFrame,
	QueueProjection,
	QueuedInput,
	SessionSnapshot,
	TranscriptEntry,
	TranscriptTreeIndex,
	TranscriptTreeNode,
} from "./types.ts";

export interface SelectedSessionCache {
	sessionId: string | null;
	snapshot: SessionSnapshot | null;
	activeBranchEntryIds: string[];
	entriesById: Map<string, TranscriptEntry>;
	treeNodesById: Map<string, TranscriptTreeNode>;
	treeChildrenByParentId: Map<string | null, string[]>;
	treeOrder: string[];
	treeTranscriptRevision: number | null;
	treeLoadedPrefixSequence: number;
	treeMaxSequence: number;
	treeComplete: boolean;
	loading: boolean;
	refreshing: boolean;
	error: string | null;
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
		treeTranscriptRevision: null,
		treeLoadedPrefixSequence: 0,
		treeMaxSequence: 0,
		treeComplete: false,
		loading: false,
		refreshing: false,
		error: null,
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
	const entries = snapshot.entries ?? [];
	const sameSession = cache.sessionId === snapshot.session_id;
	const base = sameSession ? cache : emptySelectedSessionCache(snapshot.session_id);
	const entriesById = new Map(base.entriesById);
	for (const entry of entries) entriesById.set(entry.id, entry);
	const snapshotTranscriptRevision = snapshot.transcript_revision ?? null;
	const treeRevisionChanged =
		sameSession &&
		base.treeTranscriptRevision !== null &&
		snapshotTranscriptRevision !== null &&
		base.treeTranscriptRevision !== snapshotTranscriptRevision;
	return {
		...base,
		sessionId: snapshot.session_id,
		snapshot: { ...snapshot, entries },
		activeBranchEntryIds: entries.map((entry) => entry.id),
		entriesById,
		treeTranscriptRevision: treeRevisionChanged ? snapshotTranscriptRevision : base.treeTranscriptRevision,
		treeComplete: treeRevisionChanged ? false : base.treeComplete,
		loading: false,
		refreshing: false,
		error: null,
	};
}

export function applyEntryBodies(cache: SelectedSessionCache, sessionId: string, entries: TranscriptEntry[]): SelectedSessionCache {
	if (cache.sessionId !== sessionId) return cache;
	const entriesById = new Map(cache.entriesById);
	for (const entry of entries) entriesById.set(entry.id, entry);
	return { ...cache, entriesById };
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
				active_leaf_id: index.active_leaf_id,
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
		active_branch_entries?: TranscriptEntry[] | null;
	},
): SelectedSessionCache {
	if (cache.sessionId !== result.session_id || !cache.snapshot) return cache;
	const entries = result.active_branch_entries ?? null;
	const entriesById = new Map(cache.entriesById);
	if (entries) {
		for (const entry of entries) entriesById.set(entry.id, entry);
	}
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
			entries: entries ?? cache.snapshot.entries ?? [],
		},
		activeBranchEntryIds: entries ? entries.map((entry) => entry.id) : cache.activeBranchEntryIds,
		entriesById,
		refreshing: false,
		loading: false,
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
	const appendsToActiveBranch = entry.parent_id === currentLeafId || (currentLeafId === null && entry.parent_id === null);
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
	const nextCache = {
		...cache,
		snapshot,
		activeBranchEntryIds,
		entriesById,
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
		cursor = node.parent_id;
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
		const parentId = node.parent_id && byId.has(node.parent_id) ? node.parent_id : null;
		const siblings = children.get(parentId) ?? [];
		siblings.push(id);
		children.set(parentId, siblings);
	}
	return children;
}

function applyTreeNodeFromEvent(cache: SelectedSessionCache, event: EventFrame): Partial<SelectedSessionCache> {
	const node = transcriptTreeNodeFromUnknown(event.data.tree_node);
	if (!node) return {};
	const revision = numberValue(event.data.transcript_revision) ?? cache.treeTranscriptRevision;
	const canMergeNode =
		cache.treeTranscriptRevision === null ||
		cache.treeComplete ||
		node.sequence <= cache.treeLoadedPrefixSequence;
	if (!canMergeNode) {
		return {
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
		treeTranscriptRevision: revision,
		treeLoadedPrefixSequence: cache.treeComplete
			? Math.max(cache.treeLoadedPrefixSequence, node.sequence)
			: cache.treeLoadedPrefixSequence,
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
