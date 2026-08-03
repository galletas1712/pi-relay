import { describe, expect, it } from "vitest";
import {
	COMPOSER_DRAFTS_STORAGE_KEY,
	COMPOSER_DRAFT_STORAGE_PREFIX,
	ComposerDraftStore,
	composerDraftKey,
	loadComposerDrafts,
	reorderQueuedInputsBefore,
	saveComposerDrafts,
	submissionIdsForDraft,
	type ComposerDraftStorage,
	type PendingSubmittedDraft,
} from "./composer.tsx";

describe("queued input drop ordering", () => {
	it.each([
		["moves a row forward before the target", ["a", "b", "c"], "a", "c", ["b", "a", "c"]],
		["moves a row backward before the target", ["a", "b", "c"], "c", "a", ["c", "a", "b"]],
		["drops before the first row", ["a", "b", "c"], "b", "a", ["b", "a", "c"]],
		["drops before the last row", ["a", "b", "c"], "b", "c", ["a", "b", "c"]],
		["keeps a same-row drop unchanged", ["a", "b", "c"], "b", "b", ["a", "b", "c"]],
	])("%s", (_description, inputIds, draggedId, targetId, expected) => {
		expect(reorderQueuedInputsBefore(inputIds, draggedId, targetId)).toEqual(expected);
	});
});

describe("composer draft storage", () => {
	const pending: PendingSubmittedDraft = {
		value: "run tests",
		attachments: [],
		images: [],
		revision: 3,
		newSessionSetupGeneration: 7,
		clientControlId: "web_control_stable",
		newSessionId: "session_stable",
	};

	it("persists non-empty drafts by session key", () => {
		const storage = memoryStorage();
		const drafts = new Map([
			[composerDraftKey(null), "new session draft"],
			[composerDraftKey("session_a"), "existing session draft"],
		]);

		saveComposerDrafts(drafts, storage);

		expect(loadComposerDrafts(storage)).toEqual(drafts);
	});

	it("does not overwrite another tab's unrelated session draft", () => {
		const storage = memoryStorage();
		saveComposerDrafts(new Map([["session_a", "draft a"]]), storage);
		saveComposerDrafts(new Map([["session_b", "draft b"]]), storage);

		expect(storage.getItem(`${COMPOSER_DRAFT_STORAGE_PREFIX}session_a`)).toBe(
			"draft a",
		);
		expect(storage.getItem(`${COMPOSER_DRAFT_STORAGE_PREFIX}session_b`)).toBe(
			"draft b",
		);
	});

	it("replaces both IDs after a deliberate new-session setup edit", () => {
		let next = 0;
		expect(
			submissionIdsForDraft(
				pending,
				"run tests",
				8,
				(prefix) => `${prefix}_${++next}`,
			),
		).toEqual({
			clientControlId: "web_control_1",
			newSessionId: "session_2",
		});
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

	it("reuses both durable IDs when unchanged text is retried after an uncertain response", () => {
		expect(
			submissionIdsForDraft(pending, "run tests", 7, () => "new-id"),
		).toEqual({
			clientControlId: "web_control_stable",
			newSessionId: "session_stable",
		});
	});

	it("keeps retry identity small and changes it when artifact refs change", () => {
		const first = {
			type: "image" as const,
			artifact_id: `sha256:${"a".repeat(64)}`,
		};
		const second = {
			type: "image" as const,
			artifact_id: `sha256:${"b".repeat(64)}`,
		};
		const imagePending = { ...pending, attachments: [first] };

		expect(
			submissionIdsForDraft(imagePending, "run tests", 7, () => "new-id", [
				first,
			]),
		).toEqual({
			clientControlId: "web_control_stable",
			newSessionId: "session_stable",
		});
		expect(
			submissionIdsForDraft(
				imagePending,
				"run tests",
				7,
				(prefix) => `${prefix}_new`,
				[second],
			),
		).toEqual({
			clientControlId: "web_control_new",
			newSessionId: "session_new",
		});
	});

	it("replaces both IDs after a deliberate edit", () => {
		let next = 0;
		expect(
			submissionIdsForDraft(
				pending,
				"run all tests",
				7,
				(prefix) => `${prefix}_${++next}`,
			),
		).toEqual({
			clientControlId: "web_control_1",
			newSessionId: "session_2",
		});
	});

	it("refuses to replace an unresolved submission before newer user intent", () => {
		const store = new ComposerDraftStore(memoryStorage());
		store.setDraft("session-a", "run tests");
		const first = store.beginSubmission(
			"session-a",
			"run tests",
			[],
			0,
			(prefix) => `${prefix}_stable`,
		);

		expect(first).not.toBeNull();
		expect(
			store.beginSubmission(
				"session-a",
				"run tests",
				[],
				0,
				() => "replacement",
			),
		).toBeNull();
		store.settleSubmission("session-a", first!, true);
		expect(store.draft("session-a")).toBe("");
		store.dispose();
	});
});

function memoryStorage(): ComposerDraftStorage {
	const data = new Map<string, string>();
	return {
		get length() {
			return data.size;
		},
		clear: () => data.clear(),
		getItem: (key) => data.get(key) ?? null,
		key: (index) => Array.from(data.keys())[index] ?? null,
		setItem: (key, value) => {
			data.set(key, value);
		},
		removeItem: (key) => {
			data.delete(key);
		},
	};
}
