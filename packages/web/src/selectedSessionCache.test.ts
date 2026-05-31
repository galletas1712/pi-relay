import { describe, expect, it } from "vitest";
import {
	applyActiveBranchSyncToCache,
	applyEntryBodies,
	applyQueueProjection,
	applySelectedSnapshot,
	applySwitchResultToCache,
	applyTranscriptAppendedEvent,
	applyTreeIndex,
	branchFromTree,
	emptySelectedSessionCache,
	mergeSessionActivityEvent,
	selectedEntries,
	treeNodesInOrder,
} from "./selectedSessionCache.ts";
import type {
	EventFrame,
	ProviderConfig,
	QueueProjection,
	SessionSnapshot,
	TranscriptEntry,
	TranscriptTreeIndex,
	TranscriptTreeNode,
} from "./types.ts";

const sessionId = "session_1";
const provider: ProviderConfig = { kind: "openai", model: "gpt-5.1" };

describe("selected session cache", () => {
	it("normalizes selected snapshots into active branch bodies", () => {
		const root = entry("entry_1", null, "first", 1);
		const child = entry("entry_2", "entry_1", "second", 2);

		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([root, child], { transcriptRevision: 4 }));

		expect(cache.snapshot?.active_leaf_id).toBe("entry_2");
		expect(cache.activeBranchEntryIds).toEqual(["entry_1", "entry_2"]);
		expect(selectedEntries(cache)).toEqual([root, child]);
		expect(cache.entriesById.get("entry_2")).toBe(child);
	});

	it("replaces queue projections and ignores stale ones", () => {
		const cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([], { sessionRevision: 2, queueRevision: 2, transcriptRevision: 1 }),
		);
		const newer = applyQueueProjection(cache, sessionId, queueProjection(3, "queued", ["input_1"]));
		const stale = applyQueueProjection(newer, sessionId, queueProjection(2, "idle", ["input_stale"]));

		expect(newer.snapshot?.queue_revision).toBe(3);
		expect(newer.snapshot?.activity).toBe("queued");
		expect(newer.snapshot?.queued_inputs.map((input) => input.input_id)).toEqual(["input_1"]);
		expect(stale.snapshot?.queue_revision).toBe(3);
		expect(stale.snapshot?.queued_inputs.map((input) => input.input_id)).toEqual(["input_1"]);
	});

	it("merges activity hints from thin events without a full selected-session refresh", () => {
		const first = entry("entry_1", null, "first", 1);
		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([first], { lastEventId: 4 }));

		const next = mergeSessionActivityEvent(cache, sessionId, 7, "running");

		expect(next.snapshot?.activity).toBe("running");
		expect(next.snapshot?.last_event_id).toBe(7);
		expect(selectedEntries(next).map((candidate) => candidate.id)).toEqual(["entry_1"]);
	});

	it("accumulates contiguous tree-index pages for the same revision", () => {
		let cache = emptySelectedSessionCache(sessionId);
		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_1", null, 1), treeNode("entry_2", "entry_1", 2)], {
				afterSequence: 0,
				complete: false,
				maxSequence: 3,
				transcriptRevision: 7,
			}),
		);
		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_3", "entry_2", 3, "turn_finished")], {
				afterSequence: 2,
				complete: true,
				maxSequence: 3,
				transcriptRevision: 7,
			}),
		);

		expect(treeNodesInOrder(cache).map((node) => node.id)).toEqual(["entry_1", "entry_2", "entry_3"]);
		expect(cache.treeTranscriptRevision).toBe(7);
		expect(cache.treeLoadedPrefixSequence).toBe(3);
		expect(cache.treeMaxSequence).toBe(3);
		expect(cache.treeComplete).toBe(true);
	});

	it("rejects changed-revision delta tree pages so callers must restart from the beginning", () => {
		let cache = applyTreeIndex(
			emptySelectedSessionCache(sessionId),
			treeIndex([treeNode("entry_1", null, 1), treeNode("entry_2", "entry_1", 2)], {
				afterSequence: 0,
				complete: true,
				maxSequence: 2,
				transcriptRevision: 1,
			}),
		);

		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_3", "entry_2", 3)], {
				afterSequence: 2,
				complete: true,
				maxSequence: 3,
				transcriptRevision: 2,
			}),
		);

		expect(treeNodesInOrder(cache)).toEqual([]);
		expect(cache.treeTranscriptRevision).toBe(2);
		expect(cache.treeLoadedPrefixSequence).toBe(0);
		expect(cache.treeMaxSequence).toBe(0);
		expect(cache.treeComplete).toBe(false);
	});

	it("rejects overlapping delta tree pages because duplicate IDs can hide missing sequence gaps", () => {
		let cache = applyTreeIndex(
			emptySelectedSessionCache(sessionId),
			treeIndex([treeNode("entry_1", null, 1), treeNode("entry_2", "entry_1", 2)], {
				afterSequence: 0,
				complete: false,
				maxSequence: 4,
				transcriptRevision: 1,
			}),
		);

		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_2", "entry_1", 2), treeNode("entry_4", "entry_2", 4)], {
				afterSequence: 1,
				complete: true,
				maxSequence: 4,
				transcriptRevision: 1,
			}),
		);

		expect(treeNodesInOrder(cache)).toEqual([]);
		expect(cache.treeLoadedPrefixSequence).toBe(0);
		expect(cache.treeComplete).toBe(false);
	});

	it("rejects non-contiguous delta tree pages", () => {
		let cache = applyTreeIndex(
			emptySelectedSessionCache(sessionId),
			treeIndex([treeNode("entry_1", null, 1)], {
				afterSequence: 0,
				complete: false,
				maxSequence: 3,
				transcriptRevision: 1,
			}),
		);

		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_3", "entry_2", 3)], {
				afterSequence: 2,
				complete: true,
				maxSequence: 3,
				transcriptRevision: 1,
			}),
		);

		expect(treeNodesInOrder(cache)).toEqual([]);
		expect(cache.treeTranscriptRevision).toBe(1);
		expect(cache.treeComplete).toBe(false);
	});

	it("appends transcript events that extend the active branch", () => {
		const first = entry("entry_1", null, "first", 1);
		const second = entry("entry_2", "entry_1", "second", 2);
		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([first], { transcriptRevision: 1 }));

		const applied = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(second, 5, 2));

		expect(applied.result).toBe("applied");
		expect(applied.cache.snapshot?.active_leaf_id).toBe("entry_2");
		expect(selectedEntries(applied.cache).map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
	});

	it("leaves incomplete compact topology to transcript.index instead of merging append events", () => {
		const first = entry("entry_1", null, "first", 1);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([], { transcriptRevision: 0 }));

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(first, 4, 1)).cache;

		expect(treeNodesInOrder(cache)).toEqual([]);
		expect(cache.treeLoadedPrefixSequence).toBe(0);
		expect(cache.treeMaxSequence).toBe(1);
		expect(cache.treeComplete).toBe(false);
	});

	it("extends a complete compact tree from append events without assuming per-session contiguous sequences", () => {
		let cache = applyTreeIndex(
			emptySelectedSessionCache(sessionId),
			treeIndex([treeNode("entry_1", null, 10)], {
				afterSequence: 0,
				complete: true,
				maxSequence: 10,
				transcriptRevision: 1,
			}),
		);
		cache = applySelectedSnapshot(cache, snapshot([entry("entry_1", null, "first", 10)], { transcriptRevision: 1 }));
		const second = entry("entry_2", "entry_1", "second", 42);

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(second, 5, 2)).cache;

		expect(treeNodesInOrder(cache).map((node) => node.id)).toEqual(["entry_1", "entry_2"]);
		expect(cache.treeLoadedPrefixSequence).toBe(42);
		expect(cache.treeMaxSequence).toBe(42);
		expect(cache.treeComplete).toBe(true);
	});

	it("does not merge append events beyond a partial compact index", () => {
		let cache = applyTreeIndex(
			emptySelectedSessionCache(sessionId),
			treeIndex([treeNode("entry_1", null, 10)], {
				afterSequence: 0,
				complete: false,
				maxSequence: 20,
				transcriptRevision: 1,
			}),
		);
		cache = applySelectedSnapshot(cache, snapshot([entry("entry_1", null, "first", 10)], { transcriptRevision: 1 }));
		const later = entry("entry_3", "entry_2", "later", 42);

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(later, 6, 2)).cache;

		expect(treeNodesInOrder(cache).map((node) => node.id)).toEqual(["entry_1"]);
		expect(cache.treeLoadedPrefixSequence).toBe(10);
		expect(cache.treeMaxSequence).toBe(42);
		expect(cache.treeComplete).toBe(false);
	});

	it("appends compaction roots that continue from the current branch", () => {
		const first = entry("entry_1", null, "first", 1);
		const compact = compactionEntry("compact_1", "entry_1", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([first], { transcriptRevision: 1 }));
		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_1", null, 1)], {
				afterSequence: 0,
				complete: true,
				maxSequence: 1,
				transcriptRevision: 1,
				activeLeafId: "entry_1",
			}),
		);

		const applied = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(compact, 5, 2));

		expect(applied.result).toBe("applied");
		expect(selectedEntries(applied.cache).map((candidate) => candidate.id)).toEqual(["entry_1", "compact_1"]);
		expect(applied.cache.treeNodesById.get("compact_1")?.source_leaf_id).toBe("entry_1");
	});

	it("walks tree branches through compaction source leaves", () => {
		let cache = applyTreeIndex(
			emptySelectedSessionCache(sessionId),
			treeIndex(
				[
					treeNode("entry_1", null, 1),
					treeNode("compact_1", null, 2, "compaction_summary", "entry_1"),
					treeNode("entry_2", "compact_1", 3),
				],
				{
					afterSequence: 0,
					complete: true,
					maxSequence: 3,
					transcriptRevision: 1,
					activeLeafId: "entry_2",
				},
			),
		);

		expect(branchFromTree(cache, "entry_2").map((node) => node.id)).toEqual(["entry_1", "compact_1", "entry_2"]);
	});

	it("requests a refresh when transcript append events move to another branch", () => {
		const first = entry("entry_1", null, "first", 1);
		const branched = entry("entry_3", "entry_other", "branched", 3);
		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([first], { transcriptRevision: 1 }));

		const applied = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(branched, 6, 2));

		expect(applied.result).toBe("refresh");
		expect(selectedEntries(applied.cache).map((candidate) => candidate.id)).toEqual(["entry_1"]);
		expect(applied.cache.entriesById.get("entry_3")).toBe(branched);
	});

	it("replaces active-branch bodies from switch results and preserves sparse cached bodies", () => {
		const original = entry("entry_1", null, "first", 1);
		const sparse = entry("entry_sparse", null, "sparse", 9);
		const switched = entry("entry_2", "entry_1", "switched", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([original], { transcriptRevision: 1 }));
		cache = applyEntryBodies(cache, sessionId, [sparse]);

		cache = applySwitchResultToCache(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_2",
			activity: "idle",
			session_revision: 3,
			queue_revision: 1,
			transcript_revision: 1,
			last_event_id: 8,
			active_branch_entries: [original, switched],
		});

		expect(cache.snapshot?.active_leaf_id).toBe("entry_2");
		expect(cache.snapshot?.last_event_id).toBe(8);
		expect(selectedEntries(cache).map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
		expect(cache.entriesById.get("entry_sparse")).toBe(sparse);
	});

	it("applies active-branch suffix sync without replacing existing bodies", () => {
		const original = entry("entry_1", null, "first", 1);
		const appended = entry("entry_2", "entry_1", "second", 2);
		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([original], { transcriptRevision: 1 }));

		const applied = applyActiveBranchSyncToCache(cache, {
			session_id: sessionId,
			base_leaf_id: "entry_1",
			active_leaf_id: "entry_2",
			status: "extended",
			entries: [appended],
			overview: overview([], { sessionRevision: 2, transcriptRevision: 2, lastEventId: 9, activeLeafId: "entry_2" }),
		});

		expect(applied.result).toBe("applied");
		expect(selectedEntries(applied.cache).map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
		expect(applied.cache.entriesById.get("entry_1")).toBe(original);
		expect(applied.cache.snapshot?.last_event_id).toBe(9);
	});

	it("requests reload when active-branch sync suffix does not extend the cached leaf", () => {
		const original = entry("entry_1", null, "first", 1);
		const branched = entry("entry_3", "entry_other", "branched", 3);
		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([original], { transcriptRevision: 1 }));

		const applied = applyActiveBranchSyncToCache(cache, {
			session_id: sessionId,
			base_leaf_id: "entry_1",
			active_leaf_id: "entry_3",
			status: "extended",
			entries: [branched],
			overview: overview([], { activeLeafId: "entry_3" }),
		});

		expect(applied.result).toBe("reload");
		expect(selectedEntries(applied.cache).map((candidate) => candidate.id)).toEqual(["entry_1"]);
	});

	it("installs sparse switch branch ids while preserving cached bodies", () => {
		const original = entry("entry_1", null, "first", 1);
		const switched = entry("entry_2", "entry_1", "switched", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([original], { transcriptRevision: 1 }));
		cache = applyEntryBodies(cache, sessionId, [switched]);

		cache = applySwitchResultToCache(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_2",
			activity: "idle",
			session_revision: 3,
			queue_revision: 1,
			transcript_revision: 1,
			last_event_id: 8,
			active_branch_entry_ids: ["entry_1", "entry_2"],
			active_branch_entries: [],
		});

		expect(cache.activeBranchEntryIds).toEqual(["entry_1", "entry_2"]);
		expect(selectedEntries(cache).map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
		expect(cache.snapshot?.entries?.map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
	});

	it("hydrates selected snapshot entries when sparse bodies arrive after branch ids", () => {
		const original = entry("entry_1", null, "first", 1);
		const switched = entry("entry_2", "entry_1", "switched", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([original], { transcriptRevision: 1 }));
		cache = applySwitchResultToCache(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_2",
			active_branch_entry_ids: ["entry_1", "entry_2"],
			active_branch_entries: [],
		});

		cache = applyEntryBodies(cache, sessionId, [switched]);

		expect(selectedEntries(cache).map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
		expect(cache.snapshot?.entries?.map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
	});
});

function snapshot(
	entries: TranscriptEntry[],
	options: {
		sessionRevision?: number;
		queueRevision?: number;
		transcriptRevision?: number;
		lastEventId?: number;
	} = {},
): SessionSnapshot {
	return {
		session_id: sessionId,
		project_id: "project_1",
		outer_cwd: "/repo",
		workspaces: [],
		activity: "idle",
		active_leaf_id: entries.at(-1)?.id ?? null,
		provider,
		metadata: {},
		pending_actions: [],
		queued_inputs: [],
		session_revision: options.sessionRevision ?? 1,
		queue_revision: options.queueRevision ?? 1,
		transcript_revision: options.transcriptRevision ?? 1,
		last_event_id: options.lastEventId ?? 1,
		server_time_ms: 1_700_000_000_000,
		has_transcript_entries: entries.length > 0,
		entries,
	};
}

function overview(
	entries: TranscriptEntry[],
	options: {
		sessionRevision?: number;
		queueRevision?: number;
		transcriptRevision?: number;
		lastEventId?: number;
		activeLeafId?: string | null;
	} = {},
): Omit<SessionSnapshot, "entries"> {
	const value = snapshot(entries, options);
	const { entries: _entries, ...rest } = value;
	return {
		...rest,
		active_leaf_id: options.activeLeafId ?? value.active_leaf_id,
	};
}

function compactionEntry(id: string, sourceLeafId: string, sequence: number): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: {
			type: "compaction_summary",
			source_session_id: sessionId,
			source_leaf_id: sourceLeafId,
			summary: "summarized",
			tokens_before: null,
			last_turn_id: 1,
		},
		provider_replay: [],
	};
}

function entry(id: string, parentId: string | null, text: string, sequence: number): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: { type: "user_message", content: [{ type: "text", text }] },
		provider_replay: [],
	};
}

function treeNode(
	id: string,
	parentId: string | null,
	sequence: number,
	itemType: TranscriptTreeNode["item_type"] = "user_message",
	sourceLeafId: string | null = null,
): TranscriptTreeNode {
	return {
		id,
		parent_id: parentId,
		source_leaf_id: sourceLeafId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item_type: itemType,
		turn_id: null,
		outcome: null,
		can_switch_to: itemType === "turn_finished" || itemType === "compaction_summary",
		edit_target_leaf_id: null,
		display_hint: id,
	};
}

function treeIndex(
	nodes: TranscriptTreeNode[],
	options: {
		afterSequence: number;
		complete: boolean;
		maxSequence: number;
		transcriptRevision: number;
		sessionRevision?: number;
		activeLeafId?: string | null;
	},
): TranscriptTreeIndex {
	return {
		session_id: sessionId,
		active_leaf_id: options.activeLeafId ?? nodes.at(-1)?.id ?? null,
		session_revision: options.sessionRevision ?? options.transcriptRevision,
		transcript_revision: options.transcriptRevision,
		after_sequence: options.afterSequence,
		max_sequence: options.maxSequence,
		complete: options.complete,
		nodes,
	};
}

function queueProjection(queueRevision: number, activity: QueueProjection["activity"], inputIds: string[]): QueueProjection {
	return {
		session_revision: queueRevision,
		queue_revision: queueRevision,
		transcript_revision: 1,
		activity,
		queued_inputs: inputIds.map((inputId, index) => ({
			input_id: inputId,
			priority: "follow_up",
			status: "queued",
			content: [{ type: "text", text: inputId }],
			created_at: "2026-01-01T00:00:00Z",
			updated_at: "2026-01-01T00:00:00Z",
			follow_up_position: index,
		})),
	};
}

function transcriptAppendedEvent(entryRecord: TranscriptEntry, eventId: number, transcriptRevision: number): EventFrame {
	const sourceLeafId = entryRecord.item.type === "compaction_summary" ? entryRecord.item.source_leaf_id : null;
	return {
		event_id: eventId,
		event: "transcript.appended",
		session_id: sessionId,
		data: {
			entry_id: entryRecord.id,
			entry: entryRecord,
			tree_node: treeNode(entryRecord.id, entryRecord.parent_id, entryRecord.sequence ?? 0, entryRecord.item.type, sourceLeafId),
			active_leaf_id: entryRecord.id,
			session_revision: transcriptRevision,
			queue_revision: 1,
			transcript_revision: transcriptRevision,
		},
	};
}
