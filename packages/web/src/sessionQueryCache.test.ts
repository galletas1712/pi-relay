import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { queryKeys } from "./queryKeys.ts";
import {
	mergeSnapshotIntoSessionList,
	patchSessionListActivity,
	patchSessionListMetadata,
	patchSessionListProvider,
} from "./sessionQueryCache.ts";
import type { ProviderConfig, SessionSnapshot, SessionSummary } from "./types.ts";

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
