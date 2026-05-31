import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { queryKeys } from "./queryKeys.ts";
import {
	applyActiveBranchSync,
	mergeSnapshotIntoSessionList,
	patchSessionListActivity,
	patchSessionListMetadata,
	patchSessionListProvider,
	patchSessionSnapshot,
} from "./sessionQueryCache.ts";
import type { ActiveBranchSyncResponse, ProviderConfig, SessionSnapshot, SessionSummary, TranscriptEntry } from "./types.ts";

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

	it("patches selected snapshots without touching transcript entries", () => {
		const queryClient = seededClient();

		patchSessionSnapshot(queryClient, sessionId, "active_branch", (snapshot) => ({
			...snapshot,
			provider: nextProvider,
			metadata: { title: "Patched" },
			activity: "running",
		}));

		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"))).toMatchObject({
			provider: nextProvider,
			metadata: { title: "Patched" },
			activity: "running",
			entries: [],
		});
		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			provider,
			metadata: { title: "Old", archived: true },
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

	it("applies an active branch sync suffix when it extends the cached leaf", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_1";
		snapshot.entries = [entry("entry_1", null, "first")];
		const next = entry("entry_2", "entry_1", "second");

		expect(applyActiveBranchSync(snapshot, syncResponse("extended", [next], "entry_2", 2))).toBe("applied");

		expect(snapshot.active_leaf_id).toBe("entry_2");
		expect(snapshot.entries?.map((candidate) => candidate.id)).toEqual(["entry_1", "entry_2"]);
		expect(snapshot.last_event_id).toBe(2);
	});

	it("applies an active branch sync suffix when compaction continues from the cached leaf", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_1";
		snapshot.entries = [entry("entry_1", null, "first")];
		const compact = compactionEntry("compact_1", "entry_1");

		expect(applyActiveBranchSync(snapshot, syncResponse("extended", [compact], "compact_1", 2))).toBe("applied");

		expect(snapshot.active_leaf_id).toBe("compact_1");
		expect(snapshot.entries?.map((candidate) => candidate.id)).toEqual(["entry_1", "compact_1"]);
	});

	it("asks for a reload when a sync suffix does not extend the cached leaf", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_other";
		snapshot.entries = [entry("entry_other", null, "other")];

		expect(applyActiveBranchSync(snapshot, syncResponse("extended", [entry("entry_2", "entry_1", "second")], "entry_2", 2))).toBe("reload");
		expect(snapshot.active_leaf_id).toBe("entry_other");
	});

	it("keeps entries and applies overview when active branch is unchanged", () => {
		const snapshot = snapshotFixture();
		snapshot.active_leaf_id = "entry_1";
		snapshot.entries = [entry("entry_1", null, "first")];
		const response = syncResponse("unchanged", [], "entry_1", 3);
		response.overview.activity = "running";

		expect(applyActiveBranchSync(snapshot, response)).toBe("applied");
		expect(snapshot.active_leaf_id).toBe("entry_1");
		expect(snapshot.entries?.map((candidate) => candidate.id)).toEqual(["entry_1"]);
		expect(snapshot.activity).toBe("running");
		expect(snapshot.last_event_id).toBe(3);
	});

	it("asks for a reload when the server reports a branch change", () => {
		const snapshot = snapshotFixture();
		expect(applyActiveBranchSync(snapshot, syncResponse("branch_changed", [], "entry_new", 4))).toBe("reload");
	});
});

function seededClient(): QueryClient {
	const queryClient = new QueryClient();
	queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), [summary()]);
	queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"), snapshotFixture());
	return queryClient;
}

function compactionEntry(id: string, sourceLeafId: string): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 1_700_000_000_001,
		item: {
			type: "compaction_summary",
			source_session_id: sessionId,
			source_leaf_id: sourceLeafId,
			summary: "summarized",
			tokens_before: null,
			last_turn_id: 1,
		},
	};
}

function summary(): SessionSummary {
	return {
			session_id: sessionId,
			project_id: projectId,
			outer_cwd: "/repo",
			workspaces: [],
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

function snapshotFixture(): SessionSnapshot {
	return {
			session_id: sessionId,
			project_id: projectId,
			outer_cwd: "/repo",
			workspaces: [],
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
		server_time_ms: 1_700_000_000_000,
		entries: [],
	};
}

function syncResponse(
	status: ActiveBranchSyncResponse["status"],
	entries: TranscriptEntry[],
	activeLeafId: string | null,
	eventId: number,
): ActiveBranchSyncResponse {
	return {
		session_id: sessionId,
		base_leaf_id: "entry_1",
		active_leaf_id: activeLeafId,
		status,
		entries,
		overview: {
			...snapshotFixture(),
			active_leaf_id: activeLeafId,
			last_event_id: eventId,
			has_transcript_entries: Boolean(activeLeafId),
		},
	};
}
