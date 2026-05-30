import { describe, expect, it } from "vitest";
import {
	applyEntryBodies,
	applyQueueProjection,
	applySelectedSnapshot,
	applySwitchResultToCache,
	applyTranscriptAppendedEvent,
	applyTreeIndex,
	emptySelectedSessionCache,
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
		expect(applied.cache.treeNodesById.get("entry_2")?.sequence).toBe(2);
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
): TranscriptTreeNode {
	return {
		id,
		parent_id: parentId,
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
	return {
		event_id: eventId,
		event: "transcript.appended",
		session_id: sessionId,
		data: {
			entry_id: entryRecord.id,
			entry: entryRecord,
			tree_node: treeNode(entryRecord.id, entryRecord.parent_id, entryRecord.sequence ?? 0, entryRecord.item.type),
			active_leaf_id: entryRecord.id,
			session_revision: transcriptRevision,
			queue_revision: 1,
			transcript_revision: transcriptRevision,
		},
	};
}
