import { describe, expect, it } from "vitest";
import { activeLeafIdFromEntries } from "./chatPane.tsx";
import type { TranscriptEntry } from "./types.ts";

describe("activeLeafIdFromEntries", () => {
	it("uses the loaded active-branch tail instead of ahead-of-body snapshot metadata", () => {
		expect(activeLeafIdFromEntries([entry("entry_1", null), entry("entry_2", "entry_1")])).toBe("entry_2");
	});
});

function entry(id: string, parentId: string | null): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1,
		sequence: 1,
		item: { type: "user_message", content: [{ type: "text", text: id }] },
		provider_replay: [],
	};
}

