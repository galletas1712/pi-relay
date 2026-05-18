import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { queryKeys } from "./queryKeys.ts";
import {
	applyServerViewUpdate,
	mergeSnapshotIntoSessionList,
	patchSessionListActivity,
	patchSessionListMetadata,
	patchSessionListProvider,
} from "./sessionQueryCache.ts";
import type { EventFrame, ProviderConfig, SessionSnapshot, SessionSummary, TranscriptEntry } from "./types.ts";

const projectId = "project_1";
const sessionId = "session_1";
const provider: ProviderConfig = { kind: "openai", model: "gpt-5.1" };
const nextProvider: ProviderConfig = {
	kind: "claude",
	model: "claude-sonnet-5",
};

describe("session query cache helpers", () => {
	it("patches metadata only in the session list", () => {
		const queryClient = seededClient();

		patchSessionListMetadata(queryClient, projectId, sessionId, { title: "Renamed" }, ["archived"]);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0].metadata).toEqual({ title: "Renamed" });
		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"))?.metadata).toEqual({
			title: "Old",
			archived: true,
		});
	});

	it("can replace metadata in the session list", () => {
		const queryClient = seededClient();

		patchSessionListMetadata(queryClient, projectId, sessionId, { title: "Only" }, [], true);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0].metadata).toEqual({ title: "Only" });
	});

	it("patches provider and activity only in the session list", () => {
		const queryClient = seededClient();

		patchSessionListProvider(queryClient, projectId, sessionId, nextProvider);
		patchSessionListActivity(queryClient, projectId, sessionId, "running");

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			provider: nextProvider,
			activity: "running",
		});
		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"))).toMatchObject({
			provider,
			activity: "idle",
		});
	});

	it("merges authoritative selected snapshots into the session list", () => {
		const sessions = [summary()];
		const snapshot = { ...snapshotFixture(), activity: "running" as const, metadata: { title: "Snapshot" } };

		expect(mergeSnapshotIntoSessionList(sessions, snapshot)?.[0]).toMatchObject({
			activity: "running",
			metadata: { title: "Snapshot" },
		});
	});

	it("applies an append_entry view update when it extends the active branch", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_1";
		snapshot.entries = [entry("entry_1", null, "first")];
		const next = entry("entry_2", "entry_1", "second");

		expect(applyServerViewUpdate(snapshot, appendEvent(next, 2))).toBe("applied");

		expect(snapshot.active_leaf_id).toBe("entry_2");
		expect(snapshot.entries?.map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
		expect(snapshot.last_event_id).toBe(2);
	});

	it("asks for active branch reload when append_entry does not extend the cached leaf", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_other";
		snapshot.entries = [entry("entry_other", null, "other")];

		expect(applyServerViewUpdate(snapshot, appendEvent(entry("entry_2", "entry_1", "second"), 2))).toBe("reload_active_branch");
		expect(snapshot.active_leaf_id).toBe("entry_other");
	});

	it("uses persisted transcript.appended entry as a replay fallback", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_1";
		snapshot.entries = [entry("entry_1", null, "first")];
		const next = entry("entry_2", "entry_1", "second");

		expect(applyServerViewUpdate(snapshot, { ...appendEvent(next, 2), view_update: undefined })).toBe("applied");
		expect(snapshot.active_leaf_id).toBe("entry_2");
	});

	it("requests overview reload for state events without a live view update", () => {
		const snapshot = snapshotFixture();
		expect(applyServerViewUpdate(snapshot, event("tool.started", 2, { activity: "running" }))).toBe("reload_overview");
	});

	it("ignores old events", () => {
		const snapshot = { ...snapshotFixture(), last_event_id: 3 };
		expect(applyServerViewUpdate(snapshot, event("tool.started", 2, { activity: "running" }))).toBe("ignored");
	});
});

function seededClient(): QueryClient {
	const queryClient = new QueryClient();
	queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), [summary()]);
	queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"), snapshotFixture());
	return queryClient;
}

function summary(): SessionSummary {
	return {
		session_id: sessionId,
		project_id: projectId,
		starting_cwd: "/repo",
		activity: "idle",
		active_leaf_id: null,
		provider,
		metadata: { title: "Old", archived: true },
		created_at: "2026-01-01T00:00:00Z",
		updated_at: "2026-01-01T00:00:00Z",
		has_transcript_entries: false,
	};
}

function entry(id: string, parentId: string | null, text: string): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: 1_700_000_000_000,
		item: { type: "user_message", content: [{ type: "text", text }] },
	};
}

function event(name: string, eventId: number, data: EventFrame["data"]): EventFrame {
	return {
		event_id: eventId,
		event: name,
		session_id: sessionId,
		data,
	};
}

function appendEvent(entry: TranscriptEntry, eventId: number): EventFrame {
	return {
		...event("transcript.appended", eventId, { entry_id: entry.id, entry }),
		view_update: {
			overview: {
				active_leaf_id: entry.id,
				has_transcript_entries: true,
			},
			active_branch: {
				kind: "append_entry",
				entry,
			},
		},
	};
}

function snapshotFixture(): SessionSnapshot {
	return {
		session_id: sessionId,
		project_id: projectId,
		starting_cwd: "/repo",
		activity: "idle",
		active_leaf_id: null,
		provider,
		metadata: { title: "Old", archived: true },
		pending_actions: [],
		has_transcript_entries: false,
		queued_inputs: [
			{
				input_id: "input_1",
				priority: "follow_up",
				status: "queued",
				content: [{ type: "text", text: "hello" }],
				created_at: "2026-01-01T00:00:00Z",
			},
		],
		last_event_id: 1,
		entries: [],
	};
}
