import { describe, expect, it } from "vitest";
import { reduceSessionEvent } from "./sessionEvents.ts";
import type { EventFrame } from "./types.ts";

describe("reduceSessionEvent", () => {
	it("patches rename metadata and reconciles the list without refreshing selected transcript", () => {
		const operations = reduceSessionEvent(frame("session.configured", { title: "Renamed" }));

		expect(operations).toEqual([
			{
				type: "metadata",
				sessionId: "session_1",
				patch: { title: "Renamed" },
				remove: [],
			},
			{ type: "invalidate_list", reason: "session.configured" },
		]);
	});

	it("patches queued input payloads without forcing a selected transcript refresh", () => {
		const event = frame("input.queued", { input_id: "input_1", client_input_id: "client_1", content: [{ type: "text", text: "queued" }] });
		const operations = reduceSessionEvent(event);

		expect(operations).toEqual([
			{ type: "activity", sessionId: "session_1", activity: "queued" },
			{ type: "queued_inputs", sessionId: "session_1", event },
			{ type: "invalidate_list", reason: "input.queued" },
		]);
	});

	it("patches queued-input promotion without forcing a selected transcript refresh", () => {
		const event = frame("input.promoted", {
			input_id: "input_1",
			promoted_at: "now",
		});
		const operations = reduceSessionEvent(event);

		expect(operations).toEqual([
			{ type: "queued_inputs", sessionId: "session_1", event },
			{ type: "invalidate_list", reason: "input.promoted" },
		]);
	});

	it("marks transcript append events stale because current payloads lack full entries", () => {
		const operations = reduceSessionEvent(frame("transcript.appended", { entry_id: "entry_1" }));

		expect(operations).toEqual([
			{
				type: "invalidate_session",
				sessionId: "session_1",
				reason: "transcript append event lacks full entry data",
			},
		]);
	});
});

function frame(event: string, data: Record<string, unknown>): EventFrame {
	return {
		event_id: 1,
		event,
		session_id: "session_1",
		data,
	};
}
