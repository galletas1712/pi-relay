const DRAFT_STORAGE_KEY = "pi-relay.web.composer-drafts.v2";
export const NEW_SESSION_DRAFT_ID = "__new_session__";
const MAX_DRAFT_AGE_MS = 1000 * 60 * 60 * 24 * 30;
const MAX_DRAFTS = 200;

export type StoredComposerDraft = {
	text: string;
	updatedAt: number;
};

export type StoredComposerDrafts = Record<string, StoredComposerDraft>;

function nowMs(): number {
	return Date.now();
}

function browserLocalStorage(): Storage | null {
	if (typeof window === "undefined") return null;
	try {
		return window.localStorage;
	} catch {
		return null;
	}
}

function isStoredDraft(value: unknown): value is StoredComposerDraft {
	return (
		!!value &&
		typeof value === "object" &&
		typeof (value as StoredComposerDraft).text === "string" &&
		typeof (value as StoredComposerDraft).updatedAt === "number" &&
		Number.isFinite((value as StoredComposerDraft).updatedAt)
	);
}

function sortedUnexpiredDraftEntries(drafts: StoredComposerDrafts, now = nowMs()): [string, StoredComposerDraft][] {
	return Object.entries(drafts)
		.filter(([id, draft]) => id.length > 0 && draft.text.length > 0 && now - draft.updatedAt <= MAX_DRAFT_AGE_MS)
		.sort((a, b) => b[1].updatedAt - a[1].updatedAt)
		.slice(0, MAX_DRAFTS);
}

export function parseComposerDrafts(raw: string | null, now = nowMs()): StoredComposerDrafts {
	if (!raw) return {};
	try {
		const parsed = JSON.parse(raw) as unknown;
		if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
		const drafts: StoredComposerDrafts = {};
		for (const [id, value] of Object.entries(parsed)) {
			if (!isStoredDraft(value)) continue;
			drafts[id] = value;
		}
		return Object.fromEntries(sortedUnexpiredDraftEntries(drafts, now));
	} catch {
		return {};
	}
}

function readDrafts(): StoredComposerDrafts {
	const storage = browserLocalStorage();
	if (!storage) return {};
	try {
		return parseComposerDrafts(storage.getItem(DRAFT_STORAGE_KEY));
	} catch {
		return {};
	}
}

function writeDrafts(drafts: StoredComposerDrafts): void {
	const storage = browserLocalStorage();
	if (!storage) return;
	const entries = sortedUnexpiredDraftEntries(drafts);
	try {
		if (entries.length === 0) {
			storage.removeItem(DRAFT_STORAGE_KEY);
		} else {
			storage.setItem(DRAFT_STORAGE_KEY, JSON.stringify(Object.fromEntries(entries)));
		}
	} catch {
		// Drafts are best-effort. Ignore localStorage quota/security failures.
	}
}

export function loadComposerDraft(sessionId: string | null): string {
	const draftId = sessionId ?? NEW_SESSION_DRAFT_ID;
	return readDrafts()[draftId]?.text ?? "";
}

export function saveComposerDraft(sessionId: string | null, text: string): void {
	const draftId = sessionId ?? NEW_SESSION_DRAFT_ID;
	const drafts = readDrafts();
	const normalized = text.trim().length > 0 ? text : "";
	if (normalized) drafts[draftId] = { text, updatedAt: nowMs() };
	else delete drafts[draftId];
	writeDrafts(drafts);
}

export function clearComposerDraft(sessionId: string | null): void {
	saveComposerDraft(sessionId, "");
}
