import { describe, expect, it } from "vitest";
import { branchEntriesFor } from "../src/historyTargets.ts";
import type { TranscriptEntry } from "../src/types.ts";

function entry(
	id: string,
	parent_id: string | null,
	item: TranscriptEntry["item"],
): TranscriptEntry {
	return {
		id,
		parent_id,
		timestamp_ms: 1,
		item,
	};
}

function fixtureEntries(): TranscriptEntry[] {
	return [
		entry("start1", null, { type: "turn_started", turn_id: 1 }),
		entry("finish1", "start1", { type: "turn_finished", turn_id: 1, outcome: "Graceful" }),
		entry("start2", "finish1", { type: "turn_started", turn_id: 2 }),
		entry("finish2", "start2", { type: "turn_finished", turn_id: 2, outcome: "Graceful" }),
		entry("sibling", "finish1", { type: "user_message", content: [] }),
		entry("compact1", null, {
			type: "compaction_summary",
			source_session_id: "session1",
			source_leaf_id: "finish2",
			summary: "summary",
			tokens_before: 1200,
			last_turn_id: 2,
		}),
		entry("finish3", "compact1", { type: "turn_finished", turn_id: 3, outcome: "Graceful" }),
	];
}

describe("branchEntriesFor", () => {
	it("returns the selected ancestry without siblings", () => {
		expect(branchEntriesFor(fixtureEntries(), "finish2").map((item) => item.id)).toEqual([
			"start1",
			"finish1",
			"start2",
			"finish2",
		]);
		expect(branchEntriesFor(fixtureEntries(), "sibling").map((item) => item.id)).toEqual([
			"start1",
			"finish1",
			"sibling",
		]);
	});

	it("uses compaction source leaves as display parents", () => {
		expect(branchEntriesFor(fixtureEntries(), "finish3").map((item) => item.id)).toEqual([
			"start1",
			"finish1",
			"start2",
			"finish2",
			"compact1",
			"finish3",
		]);
	});
});
