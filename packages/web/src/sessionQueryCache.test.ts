import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { queryKeys } from "./queryKeys.ts";
import {
	mergeSnapshotIntoSessionList,
	patchSessionListEventSummary,
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

	it("patches provider only in the session list", () => {
		const queryClient = seededClient();

		patchSessionListProvider(queryClient, projectId, sessionId, nextProvider);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			provider: nextProvider,
		});
		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"))).toMatchObject({
			provider,
			activity: "idle",
		});
	});

	it("patches session list metadata, provider, and activity from configured events", () => {
		const queryClient = seededClient();

		patchSessionListEventSummary(
			queryClient,
			projectId,
			eventFrame("session.configured", {
				metadata: { title: "Generated title" },
				provider: nextProvider,
				activity: "running",
			}),
			"running",
		);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			metadata: { title: "Generated title" },
			provider: nextProvider,
			activity: "running",
		});
	});

	it("patches active leaf ids from transcript events in the session list", () => {
		const queryClient = seededClient();

		patchSessionListEventSummary(
			queryClient,
			projectId,
			eventFrame("transcript.appended", {
				active_leaf_id: "entry_2",
				activity: "running",
				entry: entry("entry_2", null, "hello", 1_700_000_000_123),
			}),
			"running",
		);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			active_leaf_id: "entry_2",
			activity: "running",
			last_user_message_timestamp_ms: 1_700_000_000_123,
			has_transcript_entries: true,
		});
	});

	it("re-sorts session list patches by latest user message timestamp", () => {
		const queryClient = new QueryClient();
		queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), [
			summary({
				sessionId: "session_old",
				lastUserMessageTimestampMs: 1_700_000_000_000,
			}),
			summary({
				sessionId,
				lastUserMessageTimestampMs: 1_699_999_999_999,
			}),
		]);

		patchSessionListEventSummary(
			queryClient,
			projectId,
			eventFrame("transcript.appended", {
				active_leaf_id: "entry_new",
				entry: entry("entry_new", null, "new", 1_700_000_000_500),
			}),
			"running",
		);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.map((session) => session.session_id)).toEqual([
			sessionId,
			"session_old",
		]);
	});

	it("patches null active leaf ids from history events in the session list", () => {
		const queryClient = seededClient();
		patchSessionListEventSummary(
			queryClient,
			projectId,
			eventFrame("transcript.appended", { active_leaf_id: "entry_2" }),
			null,
		);

		patchSessionListEventSummary(
			queryClient,
			projectId,
			eventFrame("history.switched", {
				active_leaf_id: null,
				activity: "idle",
			}),
			"idle",
		);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			active_leaf_id: null,
			activity: "idle",
			has_transcript_entries: true,
		});
	});

	it("merges authoritative selected snapshots into the session list", () => {
		const sessions = [summary()];
		const snapshot = {
			...snapshotFixture(),
			activity: "running" as const,
			metadata: { title: "Snapshot" },
			last_user_message_timestamp_ms: 1_700_000_000_999,
		};

		expect(mergeSnapshotIntoSessionList(sessions, snapshot)?.[0]).toMatchObject({
			activity: "running",
			metadata: { title: "Snapshot" },
			last_user_message_timestamp_ms: 1_700_000_000_999,
		});
	});

});

function seededClient(): QueryClient {
	const queryClient = new QueryClient();
	queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), [summary()]);
	queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, "active_branch"), snapshotFixture());
	return queryClient;
}

function eventFrame(event: string, data: Record<string, unknown>): EventFrame {
	return {
		event_id: 2,
		event,
		session_id: sessionId,
		data,
	};
}

function summary(
	options: {
		sessionId?: string;
		lastUserMessageTimestampMs?: number | null;
	} = {},
): SessionSummary {
	return {
		session_id: options.sessionId ?? sessionId,
		project_id: projectId,
		runtime_id: "runtime-test",
	workspace_id: "workspace-test",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider,
		metadata: { title: "Old", archived: true },
		created_at: "2026-01-01T00:00:00Z",
		updated_at: "2026-01-01T00:00:00Z",
		last_user_message_timestamp_ms: options.lastUserMessageTimestampMs ?? null,
		has_transcript_entries: false,
	};
}

function entry(id: string, parentId: string | null, text: string, timestampMs = 1_700_000_000_000): TranscriptEntry {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: timestampMs,
		item: { type: "user_message", content: [{ type: "text", text }] },
	};
}

function snapshotFixture(): SessionSnapshot {
	return {
		session_id: sessionId,
		project_id: projectId,
		runtime_id: "runtime-test",
	workspace_id: "workspace-test",
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
		last_user_message_timestamp_ms: null,
		entries: [],
	};
}
