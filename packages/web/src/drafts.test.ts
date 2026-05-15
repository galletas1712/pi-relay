import { describe, expect, it } from "vitest";
import { NEW_SESSION_DRAFT_ID, parseComposerDrafts } from "./drafts.ts";

describe("parseComposerDrafts", () => {
	it("keeps valid drafts sorted by recency", () => {
		const drafts = parseComposerDrafts(
			JSON.stringify({
				old: { text: "older", updatedAt: 100 },
				[NEW_SESSION_DRAFT_ID]: { text: "new session", updatedAt: 300 },
				recent: { text: "newer", updatedAt: 200 }
			}),
			500
		);

		expect(Object.keys(drafts)).toEqual([NEW_SESSION_DRAFT_ID, "recent", "old"]);
		expect(drafts.recent.text).toBe("newer");
	});

	it("drops invalid, empty, and expired drafts", () => {
		const now = 1000 * 60 * 60 * 24 * 31;
		const drafts = parseComposerDrafts(
			JSON.stringify({
				valid: { text: "keep me", updatedAt: now - 1000 },
				empty: { text: "", updatedAt: now },
				expired: { text: "too old", updatedAt: 1 },
				invalid: { text: 42, updatedAt: now }
			}),
			now
		);

		expect(drafts).toEqual({ valid: { text: "keep me", updatedAt: now - 1000 } });
	});

	it("treats malformed storage as empty", () => {
		expect(parseComposerDrafts("not-json")).toEqual({});
	});
});
