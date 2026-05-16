import { describe, expect, it } from "vitest";
import { refreshPlanForEvent } from "./sessionEvents.ts";

describe("refreshPlanForEvent", () => {
	it("refreshes both the selected session and session list for common state changes", () => {
		expect(refreshPlanForEvent({ event: "session.configured" })).toEqual({
			refreshSession: true,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "input.queued" })).toEqual({
			refreshSession: true,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "history.rewound" })).toEqual({
			refreshSession: true,
			refreshList: true,
		});
	});

	it("refreshes the selected session for transcript-only updates", () => {
		expect(refreshPlanForEvent({ event: "transcript.appended" })).toEqual({
			refreshSession: true,
			refreshList: false,
		});
		expect(refreshPlanForEvent({ event: "assistant.message" })).toEqual({
			refreshSession: true,
			refreshList: false,
		});
	});

	it("ignores unknown events", () => {
		expect(refreshPlanForEvent({ event: "unknown.event" })).toEqual({
			refreshSession: false,
			refreshList: false,
		});
	});
});
