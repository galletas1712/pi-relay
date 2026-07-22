import { describe, expect, it } from "vitest";
import { emptySelectedSessionCache } from "./selectedSessionCache.ts";
import {
	backgroundSessionNeedsWarm,
	canWarmBackgroundSession,
	firstKnownProjectId,
	projectIdFromEventData,
	sessionListRefreshKey,
	subagentStatusNeedsWarm,
} from "./sessionInventory.ts";
import type { EventFrame, SessionSummary } from "./types.ts";

function session(overrides: Partial<SessionSummary> = {}): SessionSummary {
	return {
		session_id: "session-1",
		project_id: null,
		parent_session_id: null,
		runtime_id: "runtime-1",
		workspace_id: "workspace-1",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-5.6-sol" },
		metadata: {},
		created_at: "2026-01-01T00:00:00.000Z",
		updated_at: "2026-01-01T00:00:00.000Z",
		...overrides,
	};
}

function event(data: Record<string, unknown>): EventFrame {
	return { event_id: 1, event: "session.idle", session_id: "session-1", data };
}

describe("session inventory policies", () => {
	it("uses distinct host and project session-list keys", () => {
		expect(sessionListRefreshKey(null)).toBe("__host__");
		expect(sessionListRefreshKey("project-1")).toBe("project-1");
	});

	it("reads event project IDs with string, null, and unknown precedence", () => {
		expect(projectIdFromEventData(event({ project_id: "project-1" }))).toBe("project-1");
		expect(projectIdFromEventData(event({ project_id: null }))).toBeNull();
		expect(projectIdFromEventData(event({ project_id: 42 }))).toBeUndefined();
		expect(firstKnownProjectId(undefined, null, "project-1")).toBeNull();
		expect(firstKnownProjectId(undefined, "project-1", null)).toBe("project-1");
		expect(firstKnownProjectId(undefined, undefined)).toBeUndefined();
	});

	it("excludes child, hidden, archived, and subagent sessions from background warming", () => {
		expect(canWarmBackgroundSession(session())).toBe(true);
		for (const overrides of [
			{ parent_session_id: "parent-1" },
			{ metadata: { hidden: true } },
			{ metadata: { archived: true } },
			{ metadata: { subagent: true } },
		]) {
			expect(canWarmBackgroundSession(session(overrides))).toBe(false);
		}
	});

	it("warms missing or stale cache snapshots and skips fresh matching cache", () => {
		const listed = session({
			activity: "running",
			active_leaf_id: "leaf-1",
			updated_at: "2026-01-01T00:01:00.000Z",
		});
		expect(backgroundSessionNeedsWarm(listed, null, undefined)).toBe(true);

		const cache = emptySelectedSessionCache("session-1");
		cache.snapshot = {
			...listed,
			pending_actions: [],
			queued_inputs: [],
			last_event_id: 1,
			server_time_ms: 1,
		};
		cache.turnOrder.push("turn-1");
		expect(backgroundSessionNeedsWarm(listed, cache, listed.updated_at)).toBe(false);
		expect(backgroundSessionNeedsWarm(listed, cache, "older")).toBe(true);
		expect(backgroundSessionNeedsWarm(
			{ ...listed, activity: "idle" },
			cache,
			listed.updated_at,
		)).toBe(true);
		expect(backgroundSessionNeedsWarm(
			{ ...listed, active_leaf_id: "leaf-2" },
			cache,
			listed.updated_at,
		)).toBe(true);
	});

	it("warms transcript-bearing sessions when no turn data is cached", () => {
		const listed = session({
			has_transcript_entries: true,
			updated_at: "2026-01-01T00:01:00.000Z",
		});
		const cache = emptySelectedSessionCache("session-1");
		cache.snapshot = {
			...listed,
			pending_actions: [],
			queued_inputs: [],
			last_event_id: 1,
			server_time_ms: 1,
		};
		expect(backgroundSessionNeedsWarm(listed, cache, listed.updated_at)).toBe(true);
		cache.turnOrder.push("turn-1");
		expect(backgroundSessionNeedsWarm(listed, cache, listed.updated_at)).toBe(false);
	});

	it("warms queued or running subagent status and activity only", () => {
		expect(subagentStatusNeedsWarm("running")).toBe(true);
		expect(subagentStatusNeedsWarm("queued")).toBe(true);
		expect(subagentStatusNeedsWarm("done")).toBe(false);
		expect(subagentStatusNeedsWarm("idle", "running")).toBe(true);
		expect(subagentStatusNeedsWarm("done", "queued")).toBe(true);
		expect(subagentStatusNeedsWarm("done", "idle")).toBe(false);
	});
});
