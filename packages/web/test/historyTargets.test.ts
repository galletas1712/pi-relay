import { describe, expect, it } from "vitest";
import {
	branchEntriesFor,
	historyEntryDisplay,
	historyForkOptions,
	historySwitchOptions,
	historyTreeRows
} from "../src/historyTargets.ts";
import type { TranscriptEntry } from "../src/types.ts";

const baseTime = Date.UTC(2026, 0, 1, 12, 0, 0);

function entry(id: string, parent_id: string | null, item: TranscriptEntry["item"], offset = 0): TranscriptEntry {
	return {
		id,
		parent_id,
		timestamp_ms: baseTime + offset,
		item
	};
}

function compactedFixtureEntries(): TranscriptEntry[] {
	return [
		...fixtureEntries(),
		entry(
			"compact1",
			null,
			{
				type: "compaction_summary",
				source_session_id: "session1",
				source_leaf_id: "finish2",
				summary: "first question and second question were answered",
				tokens_before: 1200,
				last_turn_id: 2
			},
			9
		),
		entry("start3", "compact1", { type: "turn_started", turn_id: 3 }, 10),
		entry("user3", "start3", { type: "user_message", content: [{ type: "text", text: "after compaction" }] }, 11),
		entry("finish3", "user3", { type: "turn_finished", turn_id: 3, outcome: "Graceful" }, 12)
	];
}

function fixtureEntries(): TranscriptEntry[] {
	return [
		entry("start1", null, { type: "turn_started", turn_id: 1 }),
		entry("user1", "start1", { type: "user_message", content: [{ type: "text", text: "first question" }] }, 1),
		entry("assistant1", "user1", { type: "assistant_message", items: [{ type: "text", text: "first answer" }] }, 2),
		entry("finish1", "assistant1", { type: "turn_finished", turn_id: 1, outcome: "Graceful" }, 3),
		entry("start2", "finish1", { type: "turn_started", turn_id: 2 }, 4),
		entry("user2", "start2", { type: "user_message", content: [{ type: "text", text: "second question" }] }, 5),
		entry("assistant2", "user2", {
			type: "assistant_message",
			items: [{ type: "tool_call", id: "tool1", tool_name: "bash", args_json: "{\"command\":\"echo hi\"}" }]
		}, 6),
		entry("finish2", "assistant2", { type: "turn_finished", turn_id: 2, outcome: "Graceful" }, 7),
		entry("sibling", "finish1", { type: "user_message", content: [{ type: "text", text: "alternate branch" }] }, 8)
	];
}

describe("branchEntriesFor", () => {
	it("returns the ancestry for a selected leaf without siblings", () => {
		expect(branchEntriesFor(fixtureEntries(), "finish2").map((item) => item.id)).toEqual([
			"start1",
			"user1",
			"assistant1",
			"finish1",
			"start2",
			"user2",
			"assistant2",
			"finish2"
		]);
	});

	it("returns the ancestry for sibling branch leaves", () => {
		expect(branchEntriesFor(fixtureEntries(), "sibling").map((item) => item.id)).toEqual([
			"start1",
			"user1",
			"assistant1",
			"finish1",
			"sibling"
		]);
	});
});

describe("historyEntryDisplay", () => {
	it("derives user-message labels from tree ancestry rather than array adjacency", () => {
		const entries = fixtureEntries();
		const sibling = entries.find((item) => item.id === "sibling")!;

		expect(historyEntryDisplay(sibling, entries)).toMatchObject({
			turnLabel: "user",
			title: "User message",
			preview: "alternate branch"
		});
	});
});

describe("historyForkOptions", () => {
	it("uses the same actionable turn-boundary targets as switch", () => {
		const options = historyForkOptions(fixtureEntries(), "finish2");
		const userTarget = options.find((option) => option.id === "user2");
		const boundaryTarget = options.find((option) => option.id === "finish2");

		expect(options.map((option) => option.id)).toEqual(["sibling", "finish2", "user2", "finish1", "user1"]);
		expect(userTarget).toMatchObject({
			actionLeafId: "finish1",
			expectedActiveLeafId: "finish2",
			restoreText: "second question",
			sourceEntryId: "user2",
			turnLabel: "u2",
			meta: expect.stringContaining("fork ·")
		});
		expect(boundaryTarget).toMatchObject({
			actionLeafId: "finish2",
			expectedActiveLeafId: "finish2",
			isActive: true,
			turnLabel: "t2",
			meta: expect.stringContaining("fork ·")
		});
		expect(options.some((option) => option.id === "assistant2")).toBe(false);
	});
});

describe("historySwitchOptions", () => {
	it("offers user edits, completed turns, and compaction roots inside the current session forest", () => {
		const options = historySwitchOptions(compactedFixtureEntries(), "compact1");

		expect(options.map((option) => option.id)).toEqual([
			"finish3",
			"user3",
			"compact1",
			"sibling",
			"finish2",
			"user2",
			"finish1",
			"user1"
		]);
		expect(options.find((option) => option.id === "compact1")).toMatchObject({
			actionLeafId: "compact1",
			sourceEntryId: "compact1",
			title: "Compacted history",
			turnLabel: "c2",
			isActive: true
		});
		expect(options.find((option) => option.id === "user3")).toMatchObject({
			actionLeafId: "compact1",
			restoreText: "after compaction",
			title: "User message",
			turnLabel: "u3",
			meta: expect.stringContaining("edit ·"),
			isActive: false
		});
		expect(options.find((option) => option.id === "user1")).toMatchObject({
			actionLeafId: null,
			restoreText: "first question",
			turnLabel: "u1"
		});
		expect(options.some((option) => option.id === "assistant2")).toBe(false);
	});

	it("preserves non-graceful turn outcomes on switch targets", () => {
		const entries = fixtureEntries().map((item) =>
			item.id === "finish2"
				? entry(item.id, item.parent_id, { type: "turn_finished", turn_id: 2, outcome: "Crashed" }, 7)
				: item
		);
		const options = historySwitchOptions(entries, "finish2");

		expect(options.find((option) => option.id === "finish2")).toMatchObject({
			outcome: "Crashed",
			isActive: true
		});
	});
});

describe("historyTreeRows", () => {
	it("renders sibling branches with active-path metadata", () => {
		const rows = historyTreeRows(fixtureEntries(), "finish2");

		expect(rows.map((row) => [row.entry.id, row.depth, row.isOnActivePath])).toEqual([
			["start1", 0, true],
			["user1", 0, true],
			["assistant1", 0, true],
			["finish1", 0, true],
			["start2", 0, true],
			["user2", 0, true],
			["assistant2", 0, true],
			["finish2", 0, true],
			["sibling", 1, false]
		]);
	});
});
