import { describe, expect, it } from "vitest";
import { refreshPlanForEvent } from "./sessionEvents.ts";

const KNOWN_SESSION_EVENTS = [
	"session.created",
	"session.configured",
	"session.recovered",
	"session.idle",
	"session.work_cancelled",
	"input.queued",
	"input.promoted",
	"input.updated",
	"input.cancelled",
	"input.reordered",
	"input.consumed",
	"input.accepted",
	"input.ignored",
	"history.switched",
	"history.compacted",
	"action.requested",
	"model.requested",
	"model.completed",
	"model.error",
	"tool.requested",
	"tool.started",
	"tool.completed",
	"tool.error",
	"compaction.requested",
	"compaction.completed",
	"compaction.error",
	"transcript.appended",
	"turn.started",
	"turn.finished",
	"assistant.message",
] as const;

const LIST_ONLY_EVENTS = new Set([
	"session.created",
	"input.queued",
	"input.promoted",
	"input.updated",
	"input.cancelled",
	"input.reordered",
	"input.consumed",
	"input.accepted",
	"input.ignored",
	"action.requested",
	"model.requested",
	"model.completed",
	"model.error",
	"tool.requested",
	"tool.started",
	"tool.completed",
	"tool.error",
	"turn.finished",
]);

const SELECTED_AND_LIST_EVENTS = new Set([
	"session.configured",
	"session.recovered",
	"session.idle",
	"session.work_cancelled",
	"history.switched",
	"history.compacted",
	"compaction.requested",
	"compaction.completed",
	"compaction.error",
]);

const TRANSCRIPT_ONLY_EVENTS = new Set(["transcript.appended", "turn.started", "assistant.message"]);

describe("refreshPlanForEvent", () => {
	it("syncs the selected session only for canonical selected-session refresh events", () => {
		for (const event of KNOWN_SESSION_EVENTS) {
			expect(refreshPlanForEvent({ event }).syncSelected, event).toBe(SELECTED_AND_LIST_EVENTS.has(event));
		}
	});

	it("refreshes the session list for every known non-transcript-only event", () => {
		for (const event of KNOWN_SESSION_EVENTS) {
			expect(refreshPlanForEvent({ event }).refreshList, event).toBe(
				LIST_ONLY_EVENTS.has(event) || SELECTED_AND_LIST_EVENTS.has(event),
			);
		}
	});

	it("does not refresh the list for transcript-only updates", () => {
		for (const event of TRANSCRIPT_ONLY_EVENTS) {
			expect(refreshPlanForEvent({ event })).toEqual({
				syncSelected: false,
				refreshList: false,
			});
		}
	});

	it("syncs the selected session for unknown events without refreshing the list", () => {
		expect(refreshPlanForEvent({ event: "unknown.event" })).toEqual({
			syncSelected: true,
			refreshList: false,
		});
	});
});
