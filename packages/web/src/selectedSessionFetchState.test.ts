import { describe, expect, it } from "vitest";
import {
	applySelectedSnapshot,
	applyTranscriptTurns,
	emptySelectedSessionCache,
	hasUsableSelectedSessionCache,
} from "./selectedSessionCache.ts";
import {
	IntermediateUiStateError,
	SelectedSessionFetchCoordinator,
	shouldReportActionError,
	type SelectedSessionFetchState,
} from "./selectedSessionFetchState.ts";
import type {
	SessionSnapshot,
	TranscriptEntry,
	TranscriptTurnsResult,
	TurnCard,
} from "./types.ts";

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

function initialState(): SelectedSessionFetchState {
	return {
		sessionId: null,
		selectionVersion: 0,
		loading: false,
		retrying: false,
		hadUsableCache: false,
		error: null,
	};
}

function snapshot(
	sessionId: string,
	options: {
		activeLeafId?: string | null;
		transcriptRevision?: number;
		hasTranscriptEntries?: boolean;
	} = {},
): SessionSnapshot {
	const activeLeafId = options.activeLeafId ?? null;
	return {
		session_id: sessionId,
		project_id: "project-a",
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: activeLeafId,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: {},
		pending_actions: [],
		queued_inputs: [],
		session_revision: 1,
		queue_revision: 0,
		transcript_revision: options.transcriptRevision ?? 0,
		last_event_id: 1,
		server_time_ms: 1_700_000_000_000,
		has_transcript_entries: options.hasTranscriptEntries ?? activeLeafId !== null,
	};
}

function userEntry(id: string): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 1_700_000_000_000,
		sequence: 1,
		item: { type: "user_message", content: [{ type: "text", text: "restored" }] },
	};
}

function turnCard(entry: TranscriptEntry): TurnCard {
	return {
		id: entry.id,
		turn_id: 1,
		status: "open",
		active_leaf_id: entry.id,
		start_entry_id: null,
		boundary_entry_id: null,
		start_sequence: 1,
		end_sequence: 1,
		start_timestamp_ms: entry.timestamp_ms,
		timestamp_ms: entry.timestamp_ms,
		user_messages: [entry],
		assistant_message: null,
		can_resume: false,
	};
}

function turns(
	sessionId: string,
	options: {
		activeLeafId?: string | null;
		transcriptRevision?: number;
		cards?: TurnCard[];
	} = {},
): TranscriptTurnsResult {
	return {
		session_id: sessionId,
		active_leaf_id: options.activeLeafId ?? null,
		session_revision: 1,
		transcript_revision: options.transcriptRevision ?? 0,
		before_entry_id: null,
		next_before_entry_id: null,
		has_more_before: false,
		limit: 50,
		cards: options.cards ?? [],
	};
}

describe("selected session request ownership", () => {
	it("makes a background-warm partial snapshot blocking when selection turn-load fails", async () => {
		const sessionId = "session-a";
		const partialCache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot(sessionId, {
				activeLeafId: "entry-1",
				transcriptRevision: 1,
				hasTranscriptEntries: true,
			}),
		);
		expect(hasUsableSelectedSessionCache(partialCache, sessionId)).toBe(false);

		const coordinator = new SelectedSessionFetchCoordinator(initialState());
		coordinator.select(sessionId, hasUsableSelectedSessionCache(partialCache, sessionId));
		const load = coordinator.run(sessionId, false, async () => {
			throw new Error("turn load failed");
		});

		await expect(load).rejects.toThrow("turn load failed");
		expect(coordinator.getSnapshot()).toMatchObject({
			sessionId,
			loading: false,
			hadUsableCache: false,
			error: "turn load failed",
		});
	});

	it("remains blocking after deselect and reselect of a partial failed load", async () => {
		const sessionId = "session-a";
		const partialCache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot(sessionId, {
				activeLeafId: "entry-1",
				transcriptRevision: 1,
				hasTranscriptEntries: true,
			}),
		);
		const coordinator = new SelectedSessionFetchCoordinator(initialState());
		coordinator.select(sessionId, false);
		await expect(
			coordinator.run(sessionId, false, async () => {
				throw new Error("first failure");
			}),
		).rejects.toThrow("first failure");

		coordinator.select(null, false);
		coordinator.select(sessionId, hasUsableSelectedSessionCache(partialCache, sessionId));
		await expect(
			coordinator.run(sessionId, false, async () => {
				throw new Error("second failure");
			}),
		).rejects.toThrow("second failure");

		expect(coordinator.getSnapshot()).toMatchObject({
			sessionId,
			hadUsableCache: false,
			error: "second failure",
		});
	});

	it("treats a canonically loaded empty transcript as usable", () => {
		const sessionId = "session-empty";
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot(sessionId, { activeLeafId: null, transcriptRevision: 0 }),
		);
		expect(hasUsableSelectedSessionCache(cache, sessionId)).toBe(false);

		cache = applyTranscriptTurns(cache, turns(sessionId));

		expect(cache.transcriptTurnsLoaded).toBe(true);
		expect(hasUsableSelectedSessionCache(cache, sessionId)).toBe(true);
	});

	it("keeps the owned error mounted through retry, then restores usable content and clears it", async () => {
		const sessionId = "session-a";
		let cache = applySelectedSnapshot(
			emptySelectedSessionCache(sessionId),
			snapshot(sessionId, {
				activeLeafId: "entry-restored",
				transcriptRevision: 1,
				hasTranscriptEntries: true,
			}),
		);
		const coordinator = new SelectedSessionFetchCoordinator(initialState());
		coordinator.select(sessionId, false);
		await expect(
			coordinator.run(sessionId, false, async () => {
				throw new Error("load failed");
			}),
		).rejects.toThrow("load failed");

		const response = deferred<TranscriptTurnsResult>();
		const retry = coordinator.run(sessionId, false, async () => {
			cache = applyTranscriptTurns(cache, await response.promise);
			if (!hasUsableSelectedSessionCache(cache, sessionId)) throw new Error("incomplete retry");
			return cache;
		});
		expect(coordinator.getSnapshot()).toMatchObject({
			retrying: true,
			hadUsableCache: false,
			error: "load failed",
		});

		const entry = userEntry("entry-restored");
		response.resolve(turns(sessionId, {
			activeLeafId: entry.id,
			transcriptRevision: 1,
			cards: [turnCard(entry)],
		}));
		await expect(retry).resolves.toMatchObject({ transcriptTurnsLoaded: true });
		expect(hasUsableSelectedSessionCache(cache, sessionId)).toBe(true);
		expect(coordinator.getSnapshot()).toMatchObject({
			retrying: false,
			hadUsableCache: true,
			error: null,
		});
	});

	it("fences late selected success and failure after the selection changes", async () => {
		const successCoordinator = new SelectedSessionFetchCoordinator(initialState());
		successCoordinator.select("session-a", false);
		const lateSuccess = deferred<string>();
		let visibleContent = "session-a cached";
		const success = successCoordinator.run("session-a", false, async (selectionVersion) => {
			const content = await lateSuccess.promise;
			if (successCoordinator.isCurrent("session-a", selectionVersion)) visibleContent = content;
			return content;
		});
		successCoordinator.select("session-b", false);
		visibleContent = "session-b content";
		lateSuccess.resolve("late");
		await expect(success).resolves.toBe("late");
		expect(visibleContent).toBe("session-b content");
		expect(successCoordinator.getSnapshot()).toMatchObject({
			sessionId: "session-b",
			error: null,
		});

		const failureCoordinator = new SelectedSessionFetchCoordinator(initialState());
		failureCoordinator.select("session-a", false);
		const lateFailure = deferred<string>();
		const failure = failureCoordinator.run("session-a", false, () => lateFailure.promise);
		failureCoordinator.select("session-b", false);
		lateFailure.reject(new Error("late failure"));
		await expect(failure).rejects.toThrow("late failure");
		expect(failureCoordinator.getSnapshot()).toMatchObject({
			sessionId: "session-b",
			error: null,
		});
	});

	it("does not report the selected synchronization failure as a duplicate action notice", async () => {
		const coordinator = new SelectedSessionFetchCoordinator(initialState());
		coordinator.select("session-a", false);
		let notices = 0;

		try {
			await coordinator.run("session-a", false, async () => {
				throw new Error("synchronization failed");
			});
		} catch (error) {
			if (shouldReportActionError(error)) notices += 1;
		}

		expect(coordinator.getSnapshot().error).toBe("synchronization failed");
		expect(notices).toBe(0);
		expect(shouldReportActionError(new IntermediateUiStateError("conversation is loading"))).toBe(false);
		expect(shouldReportActionError(new Error("mutation failed"))).toBe(true);
	});
});
