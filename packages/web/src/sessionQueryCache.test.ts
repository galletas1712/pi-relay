import { QueryClient } from "@tanstack/react-query";
import { describe, expect, it } from "vitest";
import { queryKeys } from "./queryKeys.ts";
import {
	patchQueuedInputsInSnapshot,
	patchSessionActivityEverywhere,
	patchSessionMetadataEverywhere,
	patchSessionProviderEverywhere,
} from "./sessionQueryCache.ts";
import type { ProviderConfig, SessionSnapshot, SessionSummary } from "./types.ts";

const projectId = "project_1";
const sessionId = "session_1";
const provider: ProviderConfig = { kind: "openai", model: "gpt-5.1" };
const nextProvider: ProviderConfig = {
	kind: "claude",
	model: "claude-sonnet-5",
};

describe("session query cache patch helpers", () => {
	it("patches metadata in the session list and session snapshot", () => {
		const queryClient = seededClient();

		patchSessionMetadataEverywhere(queryClient, projectId, sessionId, { title: "Renamed" }, ["archived"]);

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0].metadata).toEqual({ title: "Renamed" });
		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "full_tree"))?.metadata).toEqual({ title: "Renamed" });
	});

	it("patches provider and activity in both cached session shapes", () => {
		const queryClient = seededClient();

		patchSessionProviderEverywhere(queryClient, projectId, sessionId, nextProvider);
		patchSessionActivityEverywhere(queryClient, projectId, sessionId, "running");

		expect(queryClient.getQueryData<SessionSummary[]>(queryKeys.sessions(projectId))?.[0]).toMatchObject({
			provider: nextProvider,
			activity: "running",
		});
		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "full_tree"))).toMatchObject({
			provider: nextProvider,
			activity: "running",
		});
	});

	it("patches queued-input events only on the matching session snapshot", () => {
		const queryClient = seededClient();

		patchQueuedInputsInSnapshot(queryClient, {
			event_id: 2,
			event: "input.promoted",
			session_id: sessionId,
			data: { input_id: "input_1", promoted_at: "now" },
		});

		expect(queryClient.getQueryData<SessionSnapshot>(queryKeys.session(sessionId, "full_tree"))?.queued_inputs).toEqual([
			expect.objectContaining({
				input_id: "input_1",
				priority: "steer",
				status: "queued",
				promoted_at: "now",
			}),
		]);
	});
});

function seededClient(): QueryClient {
	const queryClient = new QueryClient();
	queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), [summary()]);
	queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, "full_tree"), snapshot());
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
	};
}

function snapshot(): SessionSnapshot {
	return {
		session_id: sessionId,
		project_id: projectId,
		starting_cwd: "/repo",
		activity: "idle",
		active_leaf_id: null,
		provider,
		metadata: { title: "Old", archived: true },
		pending_actions: [],
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
