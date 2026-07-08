import { describe, expect, it } from "vitest";
import {
	applyActiveBranchSyncToCache,
	applyEntryBodies,
	applyQueueProjection,
	applySelectedSnapshot,
	applySwitchResultToCache,
	applyTranscriptAppendedEvent,
	applyTreeIndex,
	applyTranscriptTurns,
	applyTurnDetail,
	activeBranchEntriesForExport,
	branchFromTree,
	captureSelectedSessionRefresh,
	commitSelectedSessionRefresh,
	emptySelectedSessionCache,
	hasUsableSelectedSessionCache,
	mergeSessionActivityEvent,
	selectedEntries,
	snapshotWithTranscriptTurnsMetadata,
	treeNodesInOrder,
} from "./selectedSessionCache.ts";
import {
	buildCachedExportBlocks,
	buildExportBlocks,
	defaultSelectedAssistantIds,
	formatExportMarkdown,
} from "./exportTranscript.ts";
import type {
	EventFrame,
	ProviderConfig,
	QueueProjection,
	SessionSnapshot,
	TranscriptEntry,
	TurnCard,
	TranscriptTreeIndex,
	TranscriptTreeNode,
	TranscriptTurnsResult,
} from "./types.ts";

const sessionId = "session_1";
const provider: ProviderConfig = { kind: "openai", model: "gpt-5.1" };

describe("selected session cache", () => {
	it("discards a staged refresh when a websocket event advances the visible cache", async () => {
		const original = entry("entry_1", null, "original", 1);
		const appended = entry("entry_2", original.id, "new event", 2);
		let visibleCache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([original], {
				sessionRevision: 1,
				transcriptRevision: 1,
				lastEventId: 1,
			}),
		);
		visibleCache = applyTranscriptTurns(visibleCache, turnsResult(original, 1));
		const fence = captureSelectedSessionRefresh(visibleCache);
		const pendingTurns = deferred<TranscriptTurnsResult>();
		const refresh = (async () => {
			let staged = applySelectedSnapshot(
				visibleCache,
				overview([], {
					sessionRevision: 1,
					transcriptRevision: 1,
					lastEventId: 1,
					activeLeafId: original.id,
				}),
			);
			staged = applyTranscriptTurns(staged, await pendingTurns.promise);
			const commit = commitSelectedSessionRefresh(fence, visibleCache, staged);
			if (commit.committed) visibleCache = commit.cache;
			return commit;
		})();

		visibleCache = applyTranscriptAppendedEvent(
			visibleCache,
			transcriptAppendedEvent(appended, 2, 2),
		).cache;
		pendingTurns.resolve(turnsResult(original, 1));
		const commit = await refresh;

		expect(commit.committed).toBe(false);
		expect(visibleCache.snapshot?.session_revision).toBe(2);
		expect(visibleCache.snapshot?.transcript_revision).toBe(2);
		expect(visibleCache.snapshot?.last_event_id).toBe(2);
		expect(selectedEntries(visibleCache).map((candidate) => candidate.id)).toEqual([
			original.id,
			appended.id,
		]);
	});

	it("preserves cached turn-card grouping and final-answer selection without inventing boundary entries", () => {
		const firstUser = entry("entry_user_1", "entry_start_1", "first question", 2);
		const firstAssistant = assistantEntry("entry_assistant_1", firstUser.id, "first answer", 3);
		const secondUser = entry("entry_user_2", "entry_start_2", "second question", 6);
		const secondAssistant = assistantEntry("entry_assistant_2", secondUser.id, "second answer", 7);
		const finished = turnFinishedEntry("entry_finish_2", secondAssistant.id, 2, "Graceful", 8);
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([finished], { activeLeafId: finished.id, transcriptRevision: 2 }),
		);
		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: finished.id,
			session_revision: 2,
			transcript_revision: 2,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [
				{
					...turnCard("entry_finish_1", 1),
					status: "completed",
					outcome: "Graceful",
					boundary_entry_id: "entry_finish_1",
					active_leaf_id: "entry_finish_1",
					start_sequence: 1,
					end_sequence: 4,
					user_messages: [firstUser],
					assistant_message: firstAssistant,
				},
				{
					...turnCard(finished.id, 2),
					status: "completed",
					outcome: "Graceful",
					boundary_entry_id: finished.id,
					active_leaf_id: finished.id,
					start_sequence: 5,
					end_sequence: 8,
					user_messages: [secondUser],
					assistant_message: secondAssistant,
				},
			],
		});

		const blocks = buildCachedExportBlocks(cache);
		expect(blocks).toMatchObject([
			{ type: "user", entryId: firstUser.id },
			{
				type: "assistant",
				entryId: firstAssistant.id,
				priorUserEntryIds: [firstUser.id],
				phase: "final_answer",
				turnLabel: "turn 1",
			},
			{ type: "user", entryId: secondUser.id },
			{
				type: "assistant",
				entryId: secondAssistant.id,
				priorUserEntryIds: [secondUser.id],
				phase: "final_answer",
				turnLabel: "turn 2",
			},
		]);
		expect([...defaultSelectedAssistantIds(blocks)]).toEqual([
			firstAssistant.id,
			secondAssistant.id,
		]);
	});

	it("commits a staged refresh when the visible cache has not advanced", async () => {
		const original = entry("entry_1", null, "original", 1);
		const canonical = entry("entry_2", original.id, "canonical", 2);
		let visibleCache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([original], {
				sessionRevision: 1,
				transcriptRevision: 1,
				lastEventId: 1,
			}),
		);
		visibleCache = applyTranscriptTurns(visibleCache, turnsResult(original, 1));
		const fence = captureSelectedSessionRefresh(visibleCache);
		const pendingTurns = deferred<TranscriptTurnsResult>();
		const refresh = (async () => {
			let staged = applySelectedSnapshot(
				visibleCache,
				overview([], {
					sessionRevision: 2,
					transcriptRevision: 2,
					lastEventId: 2,
					activeLeafId: canonical.id,
				}),
			);
			staged = applyTranscriptTurns(staged, await pendingTurns.promise);
			const commit = commitSelectedSessionRefresh(fence, visibleCache, staged);
			if (commit.committed) visibleCache = commit.cache;
			return commit;
		})();

		pendingTurns.resolve(turnsResult(canonical, 2));
		const commit = await refresh;

		expect(commit.committed).toBe(true);
		expect(visibleCache.snapshot?.session_revision).toBe(2);
		expect(visibleCache.snapshot?.transcript_revision).toBe(2);
		expect(visibleCache.snapshot?.last_event_id).toBe(2);
		expect(selectedEntries(visibleCache).map((candidate) => candidate.id)).toEqual([
			canonical.id,
		]);
	});

	it("requires a completed matching transcript-turn load, including for empty transcripts", () => {
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([], { activeLeafId: null, transcriptRevision: 0 }),
		);

		expect(hasUsableSelectedSessionCache(cache, sessionId)).toBe(false);

		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: null,
			session_revision: 1,
			transcript_revision: 0,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [],
		});

		expect(hasUsableSelectedSessionCache(cache, sessionId)).toBe(true);
	});

	it("supports staging a newer snapshot without replacing the last usable cache on turn failure", () => {
		const oldEntry = entry("entry_old", null, "old content", 1);
		let visibleCache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([oldEntry], { activeLeafId: oldEntry.id, transcriptRevision: 1 }),
		);
		visibleCache = applyTranscriptTurns(visibleCache, {
			session_id: sessionId,
			active_leaf_id: oldEntry.id,
			session_revision: 1,
			transcript_revision: 1,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [turnCard(oldEntry.id, 1)],
		});
		const staged = applySelectedSnapshot(
			visibleCache,
			overview([], {
				activeLeafId: "entry_new",
				sessionRevision: 2,
				transcriptRevision: 2,
			}),
		);

		expect(hasUsableSelectedSessionCache(visibleCache, sessionId)).toBe(true);
		expect(selectedEntries(visibleCache).map((entry) => entry.id)).toEqual([oldEntry.id]);
		expect(hasUsableSelectedSessionCache(staged, sessionId)).toBe(false);
		expect(staged.snapshot?.active_leaf_id).toBe("entry_new");
	});

	it("normalizes selected snapshots into active branch bodies", () => {
		const root = entry("entry_1", null, "first", 1);
		const child = entry("entry_2", "entry_1", "second", 2);

		const cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([root, child], { transcriptRevision: 4 }));

		expect(cache.snapshot?.active_leaf_id).toBe("entry_2");
		expect(cache.activeBranchEntryIds).toEqual(["entry_1", "entry_2"]);
		expect(selectedEntries(cache)).toEqual([root, child]);
		expect(cache.entriesById.get("entry_2")).toBe(child);
	});

	it("keeps cached active branch bodies when a metadata-only snapshot has the same active leaf", () => {
		const root = entry("entry_1", null, "first", 1);
		const child = entry("entry_2", "entry_1", "second", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([root, child], { transcriptRevision: 4 }));

		cache = applySelectedSnapshot(cache, overview([root, child], { sessionRevision: 5, transcriptRevision: 4 }));

		expect(cache.activeBranchEntryIds).toEqual(["entry_1", "entry_2"]);
		expect(selectedEntries(cache)).toEqual([root, child]);
		expect(cache.snapshot?.entries).toEqual([root, child]);
		expect(cache.snapshot?.session_revision).toBe(5);
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

	it("keeps compact tree metadata from changing the visible active branch", () => {
		const visible = entry("entry_1", null, "visible", 1);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([visible], { transcriptRevision: 1 }));

		cache = applyTreeIndex(
			cache,
			treeIndex([treeNode("entry_1", null, 1), treeNode("entry_2", "entry_1", 2)], {
				afterSequence: 0,
				complete: true,
				maxSequence: 2,
				transcriptRevision: 2,
				sessionRevision: 2,
				activeLeafId: "entry_2",
			}),
		);

		expect(cache.snapshot?.active_leaf_id).toBe("entry_1");
		expect(cache.treeActiveLeafId).toBe("entry_2");
		expect(cache.snapshot?.transcript_revision).toBe(2);
		expect(selectedEntries(cache).map((candidate) => candidate.id)).toEqual(["entry_1"]);
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

	it("extends an expanded turn detail when transcript events append to that turn", () => {
		const first = entry("entry_1", null, "first", 1);
		const second = entry("entry_2", "entry_1", "second", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([first], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnOrder: ["turn_1"],
			turnDetailsById: new Map([["turn_1", ["entry_1"]]]),
		};

		const applied = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(second, 5, 2));

		expect(applied.cache.turnDetailsById.get("turn_1")).toEqual(["entry_1", "entry_2"]);
	});

	it("updates the current turn card incrementally as transcript events append", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const user = entry("entry_user", "entry_start", "hello there", 2);
		const assistant = assistantEntry("entry_assistant", "entry_user", "answer text", 3, 1);
		const toolResult = toolResultEntry("entry_result", "entry_assistant", 4);
		const finished = turnFinishedEntry("entry_finish", "entry_result", 1, "Graceful", 5);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([started], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnOrder: ["entry_start"],
			turnCardsById: new Map([["entry_start", turnCard("entry_start", 1)]]),
			turnDetailsById: new Map([["entry_start", ["entry_start"]]]),
			turnTranscriptRevision: 1,
			turnActiveLeafId: "entry_start",
		};

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(user, 5, 2)).cache;
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(assistant, 6, 3)).cache;
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(toolResult, 7, 4)).cache;
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(finished, 8, 5)).cache;

		expect(cache.turnOrder).toEqual(["entry_finish"]);
		const card = cache.turnCardsById.get("entry_finish");
		expect(card).toMatchObject({
			id: "entry_finish",
			status: "completed",
			active_leaf_id: "entry_finish",
			boundary_entry_id: "entry_finish",
		});
		expect(card?.user_messages.map((entry) => entry.id)).toEqual(["entry_user"]);
		expect(card?.assistant_message?.id).toBe("entry_assistant");
		expect(cache.turnDetailsById.get("entry_finish")).toEqual([
			"entry_start",
			"entry_user",
			"entry_assistant",
			"entry_result",
			"entry_finish",
		]);
		expect(cache.turnDetailsById.has("entry_start")).toBe(false);
	});

	it("omits compaction-replayed users from live turn cards while advancing metadata", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const original = entry("entry_user", "entry_start", "same instruction", 2);
		const replayed = {
			...entry("entry_replayed", "entry_user", "same instruction", 3),
			item: {
				type: "user_message" as const,
				content: [{ type: "text" as const, text: "same instruction" }],
				replayed_after_compaction: true,
			},
		};
		const genuineSteer = entry("entry_steer", "entry_replayed", "same instruction", 4);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([started], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnOrder: ["entry_start"],
			turnCardsById: new Map([["entry_start", turnCard("entry_start", 1)]]),
			turnTranscriptRevision: 1,
			turnActiveLeafId: "entry_start",
		};

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(original, 5, 2)).cache;
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(replayed, 6, 3)).cache;

		const afterReplay = cache.turnCardsById.get("entry_start");
		expect(afterReplay?.user_messages.map((message) => message.id)).toEqual(["entry_user"]);
		expect(afterReplay).toMatchObject({
			active_leaf_id: "entry_replayed",
			end_sequence: 3,
			timestamp_ms: replayed.timestamp_ms,
		});

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(genuineSteer, 7, 4)).cache;
		expect(cache.turnCardsById.get("entry_start")?.user_messages.map((message) => message.id)).toEqual([
			"entry_user",
			"entry_steer",
		]);
	});

	it("keeps completed turn detail attached when a new turn card is appended", () => {
		const started = turnStartedEntry("entry_start_2", "entry_finish_1", 2, 6);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([entry("entry_finish_1", null, "done", 5)], { transcriptRevision: 1 }));
		cache = {
			...cache,
			activeBranchEntryIds: ["entry_finish_1"],
			turnOrder: ["entry_finish_1"],
			turnCardsById: new Map([[
				"entry_finish_1",
				{
					...turnCard("entry_finish_1", 1),
					status: "completed",
					boundary_entry_id: "entry_finish_1",
					active_leaf_id: "entry_finish_1",
				},
			]]),
			turnDetailsById: new Map([["entry_finish_1", ["entry_start_1", "entry_user_1", "entry_finish_1"]]]),
		};

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(started, 9, 2)).cache;

		expect(cache.turnOrder).toEqual(["entry_finish_1", "entry_start_2"]);
		expect(cache.turnDetailsById.get("entry_finish_1")).toEqual(["entry_start_1", "entry_user_1", "entry_finish_1"]);
		expect(cache.turnDetailsById.has("entry_start_2")).toBe(false);
	});

	it("carries compaction turn metadata into the current card without adding a compaction card", () => {
		const source = turnStartedEntry("entry_source", null, 7, 0);
		const compact = compactionEntry("entry_compact", "entry_source", 1, 7, 1_700_000_000_123);
		const assistant = assistantEntry("entry_assistant", "entry_compact", "resumed answer", 2);
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([source], { transcriptRevision: 0 }),
		);
		cache = {
			...cache,
			turnCardsById: new Map([["entry_source", turnCard("entry_source", 7)]]),
			turnOrder: ["entry_source"],
		};

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(compact, 4, 1)).cache;
		expect(cache.turnOrder).toEqual(["entry_source"]);
		expect(cache.turnCardsById.has("entry_compact")).toBe(false);
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(assistant, 5, 2)).cache;

		expect(cache.turnOrder).toEqual(["entry_source"]);
		expect(cache.turnCardsById.has("entry_compact")).toBe(false);
		const resumedCard = cache.turnCardsById.get("entry_source");
		expect(resumedCard).toMatchObject({
			turn_id: 7,
			start_timestamp_ms: 1_700_000_000_123,
			assistant_message: assistant,
		});
	});

	it("merges full turn-card message bodies from transcript.turns", () => {
		const user = entry("entry_user", "entry_start", "full user message text", 2);
		const finalAssistant = assistantEntry("entry_assistant_final", "entry_result", "full final answer", 5);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([], { transcriptRevision: 1 }));

		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_finish",
			session_revision: 3,
			transcript_revision: 2,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [
				{
					id: "entry_finish",
					turn_id: 1,
					status: "completed",
					outcome: "Graceful",
					start_entry_id: "entry_start",
					boundary_entry_id: "entry_finish",
					active_leaf_id: "entry_finish",
					start_sequence: 1,
					end_sequence: 6,
					start_timestamp_ms: 1_700_000_000_001,
					timestamp_ms: 1_700_000_000_006,
					user_messages: [user],
					assistant_message: finalAssistant,
					summary: null,
					can_resume: false,
				},
			],
		});

		const card = cache.turnCardsById.get("entry_finish");
		expect(card?.user_messages[0]).toBe(cache.entriesById.get("entry_user"));
		expect(card?.assistant_message).toBe(cache.entriesById.get("entry_assistant_final"));
		expect(cache.entriesById.get("entry_user")).toBe(user);
		expect(cache.entriesById.get("entry_assistant_final")).toBe(finalAssistant);
	});

	it("exports readable canonical turn-card bodies when the active branch only names the terminal leaf", () => {
		const finished = turnFinishedEntry("entry_finish", "entry_assistant", 1, "Graceful", 4);
		const user = entry("entry_user", "entry_start", "cached question", 2);
		const assistant = assistantEntry("entry_assistant", "entry_user", "cached answer", 3);
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([finished], { activeLeafId: finished.id, transcriptRevision: 2 }),
		);

		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: finished.id,
			session_revision: 2,
			transcript_revision: 2,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [{
				...turnCard(finished.id, 1),
				status: "completed",
				outcome: "Graceful",
				start_entry_id: "entry_start",
				boundary_entry_id: finished.id,
				active_leaf_id: finished.id,
				start_sequence: 1,
				end_sequence: 4,
				user_messages: [user],
				assistant_message: assistant,
			}],
		});

		expect(cache.activeBranchEntryIds).toEqual([finished.id]);
		const exportEntries = activeBranchEntriesForExport(cache);
		expect(exportEntries.map((candidate) => candidate.id)).toEqual([
			user.id,
			assistant.id,
			finished.id,
		]);
		const blocks = buildExportBlocks(exportEntries);
		expect(formatExportMarkdown(blocks, defaultSelectedAssistantIds(blocks))).toContain(
			"## Assistant\n\ncached answer",
		);
	});

	it("deduplicates loaded export bodies and orders them by canonical transcript sequence", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const user = entry("entry_user", started.id, "question", 2);
		const assistant = assistantEntry("entry_assistant", user.id, "answer", 3);
		const finished = turnFinishedEntry("entry_finish", assistant.id, 1, "Graceful", 4);
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([finished], { activeLeafId: finished.id, transcriptRevision: 2 }),
		);
		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: finished.id,
			session_revision: 2,
			transcript_revision: 2,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [{
				...turnCard(finished.id, 1),
				status: "completed",
				outcome: "Graceful",
				start_entry_id: started.id,
				boundary_entry_id: finished.id,
				active_leaf_id: finished.id,
				start_sequence: 1,
				end_sequence: 4,
				user_messages: [user],
				assistant_message: assistant,
			}],
		});
		cache = applyTurnDetail(
			cache,
			sessionId,
			finished.id,
			[started, user, assistant, finished],
		).cache;

		expect(activeBranchEntriesForExport(cache).map((candidate) => candidate.id)).toEqual([
			started.id,
			user.id,
			assistant.id,
			finished.id,
		]);
	});

	it("returns canonical empty exports and ignores unloaded turn-card bodies", () => {
		expect(activeBranchEntriesForExport(emptySelectedSessionCache())).toEqual([]);

		const selected = entry("entry_selected", null, "loaded selected body", 1);
		const staleAssistant = assistantEntry("entry_stale", selected.id, "stale turn body", 2);
		let unloaded = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([selected], { transcriptRevision: 2 }),
		);
		unloaded = {
			...unloaded,
			turnOrder: [selected.id],
			turnCardsById: new Map([[
				selected.id,
				{ ...turnCard(selected.id, 1), assistant_message: staleAssistant },
			]]),
		};
		expect(activeBranchEntriesForExport(unloaded)).toEqual([selected]);

		let empty = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([], { activeLeafId: null, transcriptRevision: 0 }),
		);
		empty = applyTranscriptTurns(empty, {
			session_id: sessionId,
			active_leaf_id: null,
			session_revision: 1,
			transcript_revision: 0,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [],
		});
		expect(activeBranchEntriesForExport(empty)).toEqual([]);
	});

	it("drops stale expanded turn details when canonical turn cards advance", () => {
		const user = entry("entry_user", "entry_start", "full user message text", 2);
		const finalAssistant = assistantEntry("entry_assistant_final", "entry_result", "full final answer", 5);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnDetailsById: new Map([["entry_start", ["entry_start", "entry_user"]]]),
		};

		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_finish",
			session_revision: 3,
			transcript_revision: 2,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [
				{
					id: "entry_finish",
					turn_id: 1,
					status: "completed",
					outcome: "Graceful",
					start_entry_id: "entry_start",
					boundary_entry_id: "entry_finish",
					active_leaf_id: "entry_finish",
					start_sequence: 1,
					end_sequence: 6,
					start_timestamp_ms: 1_700_000_000_001,
					timestamp_ms: 1_700_000_000_006,
					user_messages: [user],
					assistant_message: finalAssistant,
					summary: null,
					can_resume: false,
				},
			],
		});

		expect(cache.turnDetailsById.has("entry_finish")).toBe(false);
	});

	it("ignores stale completed turn detail responses that do not reach the current card leaf", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const user = entry("entry_user", "entry_start", "full user message text", 2);
		const firstAssistant = assistantEntry("entry_assistant_1", "entry_user", "partial", 3);
		const currentAssistant = assistantEntry("entry_assistant_2", "entry_assistant_1", "newer", 4);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnOrder: ["entry_start"],
			turnCardsById: new Map([[
				"entry_start",
				{
					...turnCard("entry_start", 1),
					status: "completed",
					outcome: "Graceful",
					boundary_entry_id: "entry_assistant_2",
					active_leaf_id: "entry_assistant_2",
					start_sequence: 1,
					end_sequence: 4,
				},
			]]),
		};

		const stale = applyTurnDetail(cache, sessionId, "entry_start", [started, user, firstAssistant]);
		const fresh = applyTurnDetail(cache, sessionId, "entry_start", [started, user, firstAssistant, currentAssistant]);

		expect(stale).toEqual({ cache, applied: false });
		expect(fresh.applied).toBe(true);
		expect(fresh.cache.turnDetailsById.get("entry_start")).toEqual([
			"entry_start",
			"entry_user",
			"entry_assistant_1",
			"entry_assistant_2",
		]);
	});

	it("accepts open turn detail responses that race with active leaf changes", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const user = entry("entry_user", "entry_start", "full user message text", 2);
		const firstAssistant = assistantEntry("entry_assistant_1", "entry_user", "partial", 3);
		const currentAssistant = assistantEntry("entry_assistant_2", "entry_assistant_1", "newer", 4);
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot([started, user, firstAssistant, currentAssistant], { transcriptRevision: 4 }),
		);
		cache = {
			...cache,
			turnOrder: ["entry_start"],
			turnCardsById: new Map([[
				"entry_start",
				{
					...turnCard("entry_start", 1),
					active_leaf_id: "entry_assistant_2",
					start_sequence: 1,
					end_sequence: 4,
				},
			]]),
		};

		const detail = applyTurnDetail(cache, sessionId, "entry_start", [started, user, firstAssistant]);

		expect(detail.applied).toBe(true);
		expect(detail.cache.turnDetailsById.get("entry_start")).toEqual([
			"entry_start",
			"entry_user",
			"entry_assistant_1",
			"entry_assistant_2",
		]);
	});

	it("prepends older transcript.turns pages without replacing the loaded tail page", () => {
		const olderUser = entry("entry_user_old", "entry_start_old", "old user", 2);
		const latestUser = entry("entry_user_new", "entry_start_new", "new user", 7);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([], { transcriptRevision: 1 }));
		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_finish_new",
			session_revision: 3,
			transcript_revision: 2,
			before_entry_id: null,
			next_before_entry_id: "entry_finish_old",
			has_more_before: true,
			limit: 1,
			cards: [
				{
					...turnCard("entry_finish_new", 2),
					status: "completed",
					start_entry_id: "entry_start_new",
					boundary_entry_id: "entry_finish_new",
					active_leaf_id: "entry_finish_new",
					start_sequence: 6,
					end_sequence: 8,
					user_messages: [latestUser],
				},
			],
		});

		cache = applyTranscriptTurns(
			cache,
			{
				session_id: sessionId,
				active_leaf_id: "entry_finish_new",
				session_revision: 3,
				transcript_revision: 2,
				before_entry_id: "entry_finish_old",
				next_before_entry_id: null,
				has_more_before: false,
				limit: 1,
				cards: [
					{
						...turnCard("entry_finish_old", 1),
						status: "completed",
						start_entry_id: "entry_start_old",
						boundary_entry_id: "entry_finish_old",
						active_leaf_id: "entry_finish_old",
						start_sequence: 1,
						end_sequence: 3,
						user_messages: [olderUser],
					},
				],
			},
			{ mode: "prepend" },
		);

		expect(cache.turnOrder).toEqual(["entry_finish_old", "entry_finish_new"]);
		expect(cache.turnHasMoreBefore).toBe(false);
		expect(cache.turnBeforeEntryId).toBeNull();
		expect(cache.turnCardsById.get("entry_finish_new")?.user_messages[0].id).toBe("entry_user_new");
		expect(cache.turnCardsById.get("entry_finish_old")?.user_messages[0].id).toBe("entry_user_old");
	});

	it("ignores stale older transcript.turns pages when the cursor no longer matches", () => {
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnTranscriptRevision: 2,
			turnActiveLeafId: "entry_finish_new",
			turnBeforeEntryId: "entry_expected_cursor",
			turnHasMoreBefore: true,
			turnOrder: ["entry_finish_new"],
			turnCardsById: new Map([["entry_finish_new", turnCard("entry_finish_new", 2)]]),
		};

		const next = applyTranscriptTurns(
			cache,
			{
				session_id: sessionId,
				active_leaf_id: "entry_finish_new",
				session_revision: 3,
				transcript_revision: 2,
				before_entry_id: "entry_stale_cursor",
				next_before_entry_id: null,
				has_more_before: false,
				limit: 1,
				cards: [turnCard("entry_finish_old", 1)],
			},
			{ mode: "prepend" },
		);

		expect(next).toBe(cache);
	});

	it("ignores stale replacement transcript.turns pages after append events advance the cache", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const appended = entry("entry_user_new", "entry_start", "new message", 2);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([started], { transcriptRevision: 1, sessionRevision: 1 }));
		cache = {
			...cache,
			turnTranscriptRevision: 1,
			turnActiveLeafId: "entry_start",
			turnOrder: ["entry_start"],
			turnCardsById: new Map([["entry_start", turnCard("entry_start", 1)]]),
		};
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(appended, 5, 2)).cache;

		const stale = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_start",
			session_revision: 1,
			transcript_revision: 1,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [turnCard("entry_start", 1)],
		});

		expect(stale).toBe(cache);
		expect(stale.turnActiveLeafId).toBe("entry_user_new");
		expect(stale.snapshot?.active_leaf_id).toBe("entry_user_new");
	});

	it("accepts transcript.turns pages with older session metadata when transcript state still matches", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const user = entry("entry_user", "entry_start", "hello", 2);
		const card = {
			...turnCard("entry_user", 1),
			start_entry_id: "entry_start",
			active_leaf_id: "entry_user",
			end_sequence: 2,
			user_messages: [user],
		};
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([started, user], { transcriptRevision: 4, sessionRevision: 8 }));

		cache = applyTranscriptTurns(cache, {
			session_id: sessionId,
			active_leaf_id: "entry_user",
			session_revision: 7,
			transcript_revision: 4,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [card],
		});

		expect(cache.turnOrder).toEqual(["entry_user"]);
		expect(cache.turnCardsById.get("entry_user")?.user_messages).toEqual([user]);
		expect(cache.snapshot?.session_revision).toBe(8);
	});

	it("uses fresher transcript.turns metadata when recovering a reselected cold load", () => {
		const coldSnapshot = overview([], {
			sessionRevision: 2,
			transcriptRevision: 2,
			activeLeafId: "entry_old",
		});
		const latestUser = entry("entry_new", "entry_start", "new", 4);
		const turns: TranscriptTurnsResult = {
			session_id: sessionId,
			active_leaf_id: "entry_new",
			session_revision: 4,
			transcript_revision: 4,
			before_entry_id: null,
			next_before_entry_id: null,
			has_more_before: false,
			limit: 50,
			cards: [
				{
					...turnCard("entry_new", 1),
					start_entry_id: "entry_start",
					active_leaf_id: "entry_new",
					start_sequence: 3,
					end_sequence: 4,
					user_messages: [latestUser],
				},
			],
		};
		let cache = applyTranscriptTurns(emptySelectedSessionCache(sessionId), turns);

		cache = applySelectedSnapshot(cache, snapshotWithTranscriptTurnsMetadata(coldSnapshot, turns));

		expect(cache.snapshot?.active_leaf_id).toBe("entry_new");
		expect(cache.snapshot?.session_revision).toBe(4);
		expect(cache.snapshot?.transcript_revision).toBe(4);
		expect(cache.activeBranchEntryIds).toEqual(["entry_new"]);
		expect(cache.turnActiveLeafId).toBe("entry_new");
	});

	it("starts a new current turn card when a turn_started entry follows a completed turn", () => {
		const started = turnStartedEntry("entry_start_2", "entry_finish_1", 2, 6);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([entry("entry_finish_1", null, "done", 5)], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnOrder: ["entry_finish_1"],
			turnCardsById: new Map([[
				"entry_finish_1",
				{
					...turnCard("entry_finish_1", 1),
					status: "completed",
					boundary_entry_id: "entry_finish_1",
					active_leaf_id: "entry_finish_1",
				},
			]]),
		};

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(started, 9, 2)).cache;

		expect(cache.turnOrder).toEqual(["entry_finish_1", "entry_start_2"]);
		expect(cache.turnCardsById.get("entry_start_2")).toMatchObject({
			id: "entry_start_2",
			turn_id: 2,
			start_entry_id: "entry_start_2",
			active_leaf_id: "entry_start_2",
			user_messages: [],
			assistant_message: null,
		});
	});

	it("updates incremental turn card end timestamps from appended entries", () => {
		const started = turnStartedEntry("entry_start", null, 1, 1);
		const user = entry("entry_user", "entry_start", "hello", 2);
		const finished = turnFinishedEntry("entry_finish", "entry_user", 1, "Graceful", 3);
		let cache = applySelectedSnapshot(emptySelectedSessionCache(sessionId), snapshot([started], { transcriptRevision: 1 }));
		cache = {
			...cache,
			turnOrder: ["entry_start"],
			turnCardsById: new Map([["entry_start", turnCard("entry_start", 1)]]),
		};

		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(user, 4, 2)).cache;
		cache = applyTranscriptAppendedEvent(cache, transcriptAppendedEvent(finished, 5, 3)).cache;

		expect(cache.turnCardsById.get("entry_finish")).toMatchObject({
			start_timestamp_ms: 1_700_000_000_001,
			timestamp_ms: 1_700_000_000_003,
		});
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

interface Deferred<T> {
	promise: Promise<T>;
	resolve: (value: T) => void;
	reject: (reason: unknown) => void;
}

function deferred<T>(): Deferred<T> {
	let resolve!: (value: T) => void;
	let reject!: (reason: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

function turnsResult(activeEntry: TranscriptEntry, revision: number): TranscriptTurnsResult {
	return {
		session_id: sessionId,
		active_leaf_id: activeEntry.id,
		session_revision: revision,
		transcript_revision: revision,
		before_entry_id: null,
		next_before_entry_id: null,
		has_more_before: false,
		limit: 50,
		cards: [{
			...turnCard(activeEntry.id, 1),
			active_leaf_id: activeEntry.id,
			user_messages: [activeEntry],
		}],
	};
}

function snapshot(
	entries: TranscriptEntry[],
	options: {
		sessionRevision?: number;
		queueRevision?: number;
		transcriptRevision?: number;
		lastEventId?: number;
		activeLeafId?: string | null;
	} = {},
): SessionSnapshot {
	return {
		session_id: sessionId,
		project_id: "project_1",
		outer_cwd: "/repo",
		workspaces: [],
		activity: "idle",
		active_leaf_id: "activeLeafId" in options ? options.activeLeafId ?? null : entries.at(-1)?.id ?? null,
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

function compactionEntry(
	id: string,
	sourceLeafId: string,
	sequence: number,
	lastTurnId = 1,
	turnStartedAtMs?: number,
): TranscriptEntry {
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
			last_turn_id: lastTurnId,
			turn_started_at_ms: turnStartedAtMs,
		},
	};
}

function entry(id: string, parentId: string | null, text: string, sequence: number): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: { type: "user_message", content: [{ type: "text", text }] },
	};
}

function turnStartedEntry(id: string, parentId: string | null, turnId: number, sequence: number): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: { type: "turn_started", turn_id: turnId },
	};
}

function assistantEntry(id: string, parentId: string | null, text: string, sequence: number, toolCallCount = 0): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: {
			type: "assistant_message",
			items: [
				{ type: "text", text },
				...Array.from({ length: toolCallCount }, (_, index) => ({
					type: "tool_call" as const,
					id: `tool_${index}`,
					tool_name: `tool_${index}`,
					args_json: "{}",
				})),
			],
		},
	};
}

function toolResultEntry(id: string, parentId: string | null, sequence: number): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: {
			type: "tool_result",
			tool_call_id: "tool_0",
			tool_name: "tool_0",
			output: "ok",
			status: "Success",
		},
	};
}

function turnFinishedEntry(
	id: string,
	parentId: string | null,
	turnId: number,
	outcome: "Graceful" | "Interrupted" | "Crashed",
	sequence: number,
): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000 + sequence,
		sequence,
		item: { type: "turn_finished", turn_id: turnId, outcome },
	};
}

function turnCard(id: string, turnId: number): TurnCard {
	return {
		id,
		turn_id: turnId,
		status: "open",
		outcome: null,
		start_entry_id: id,
		boundary_entry_id: null,
		active_leaf_id: id,
		start_sequence: 1,
		end_sequence: 1,
		start_timestamp_ms: 1_700_000_000_001,
		timestamp_ms: 1_700_000_000_001,
		user_messages: [],
		assistant_message: null,
		summary: null,
		can_resume: false,
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
