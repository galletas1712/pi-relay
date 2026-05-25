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

const TRANSCRIPT_ONLY_EVENTS = new Set(["transcript.appended", "turn.started", "assistant.message"]);

describe("refreshPlanForEvent", () => {
	it("syncs the selected session for every known event", () => {
		for (const event of KNOWN_SESSION_EVENTS) {
			expect(refreshPlanForEvent({ event }).syncSelected, event).toBe(true);
		}
	});

	it("refreshes the session list for every known non-transcript-only event", () => {
		for (const event of KNOWN_SESSION_EVENTS) {
			expect(refreshPlanForEvent({ event }).refreshList, event).toBe(!TRANSCRIPT_ONLY_EVENTS.has(event));
		}
	});

	it("does not refresh the list for transcript-only updates", () => {
		for (const event of TRANSCRIPT_ONLY_EVENTS) {
			expect(refreshPlanForEvent({ event })).toEqual({
				syncSelected: true,
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
