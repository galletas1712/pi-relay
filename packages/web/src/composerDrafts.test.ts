import { describe, expect, it } from "vitest";
import {
	COMPOSER_DRAFTS_STORAGE_KEY,
	composerDraftKey,
	loadComposerDrafts,
	resolveSubmittedDraft,
	saveComposerDrafts,
	submittedDraftStillCurrent,
	type ComposerDraftStorage,
	type PendingSubmittedDraft,
} from "./composer.tsx";

describe("composer draft storage", () => {
	it("persists non-empty drafts by session key", () => {
		const storage = memoryStorage();
		const drafts = new Map([
			[composerDraftKey(null), "new session draft"],
			[composerDraftKey("session_a"), "existing session draft"],
		]);

		saveComposerDrafts(drafts, storage);

		expect(loadComposerDrafts(storage)).toEqual(drafts);
	});

	it("drops empty drafts and removes storage when none remain", () => {
		const storage = memoryStorage();

		saveComposerDrafts(new Map([["session_a", "  "]]), storage);

		expect(storage.getItem(COMPOSER_DRAFTS_STORAGE_KEY)).toBeNull();
		expect(loadComposerDrafts(storage).size).toBe(0);
	});

	it("ignores malformed persisted drafts", () => {
		const storage = memoryStorage();
		storage.setItem(COMPOSER_DRAFTS_STORAGE_KEY, "{not json");

		expect(loadComposerDrafts(storage).size).toBe(0);
	});
});

describe("submitted composer draft guards", () => {
	const pending: PendingSubmittedDraft = { value: "run tests", version: 3 };

	it("accepts a pending submitted draft only while the stored draft version still matches", () => {
		expect(submittedDraftStillCurrent(pending, 3, "run tests", 3)).toBe(true);
		expect(resolveSubmittedDraft(pending, 3, "run tests", 3)).toBe("apply");
	});

	it("rejects stale successes or failures after a newer non-empty draft replaced the submitted text", () => {
		expect(submittedDraftStillCurrent(pending, 4, "run tests", 3)).toBe(false);
		expect(resolveSubmittedDraft(pending, 4, "run tests", 3)).toBe("ignore");
	});

	it("rejects stale successes or failures after a newer empty draft cleared the submitted text", () => {
		expect(submittedDraftStillCurrent(pending, 4, "run tests", 3)).toBe(false);
		expect(resolveSubmittedDraft(pending, 4, "run tests", 3)).toBe("ignore");
	});

	it("rejects same-version pending markers for different submitted text", () => {
		expect(submittedDraftStillCurrent(pending, 3, "ship it", 3)).toBe(false);
	});

	it("allows unversioned imperative restore only when the pending marker is still current", () => {
		expect(submittedDraftStillCurrent(pending, 3, "run tests")).toBe(true);
		expect(submittedDraftStillCurrent(pending, 4, "run tests")).toBe(false);
	});
});

function memoryStorage(): ComposerDraftStorage {
	const data = new Map<string, string>();
	return {
		getItem: (key) => data.get(key) ?? null,
		setItem: (key, value) => {
			data.set(key, value);
		},
		removeItem: (key) => {
			data.delete(key);
		},
	};
}
