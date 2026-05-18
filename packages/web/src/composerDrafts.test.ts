import { describe, expect, it } from "vitest";
import {
	COMPOSER_DRAFTS_STORAGE_KEY,
	composerDraftKey,
	loadComposerDrafts,
	saveComposerDrafts,
	type ComposerDraftStorage,
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
