import { describe, expect, it } from "vitest";
import { refreshPlanForEvent } from "./sessionEvents.ts";

describe("refreshPlanForEvent", () => {
	it("refreshes both the selected session and session list for common state changes", () => {
		expect(refreshPlanForEvent({ event: "session.configured" })).toEqual({
			syncSelected: true,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "input.queued" })).toEqual({
			syncSelected: true,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "history.rewound" })).toEqual({
			syncSelected: true,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "history.forked" })).toEqual({
			syncSelected: true,
			refreshList: true,
		});
	});

	it("refreshes the active branch for transcript updates", () => {
		expect(refreshPlanForEvent({ event: "transcript.appended" })).toEqual({
			syncSelected: true,
			refreshList: false,
		});
		expect(refreshPlanForEvent({ event: "assistant.message" })).toEqual({
			syncSelected: true,
			refreshList: false,
		});
	});

	it("syncs the selected session for unknown events without refreshing the list", () => {
		expect(refreshPlanForEvent({ event: "unknown.event" })).toEqual({
			syncSelected: true,
			refreshList: false,
		});
	});
});
