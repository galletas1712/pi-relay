import { describe, expect, it } from "vitest";
import { refreshPlanForEvent } from "./sessionEvents.ts";

describe("refreshPlanForEvent", () => {
	it("refreshes both the selected session and session list for common state changes", () => {
		expect(refreshPlanForEvent({ event: "session.configured" })).toEqual({
			refreshSession: true,
			refreshActiveBranch: false,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "input.queued" })).toEqual({
			refreshSession: true,
			refreshActiveBranch: false,
			refreshList: true,
		});
		expect(refreshPlanForEvent({ event: "history.rewound" })).toEqual({
			refreshSession: true,
			refreshActiveBranch: true,
			refreshList: true,
		});
	});

	it("refreshes the active branch for transcript updates", () => {
		expect(refreshPlanForEvent({ event: "transcript.appended" })).toEqual({
			refreshSession: true,
			refreshActiveBranch: true,
			refreshList: false,
		});
		expect(refreshPlanForEvent({ event: "assistant.message" })).toEqual({
			refreshSession: true,
			refreshActiveBranch: true,
			refreshList: false,
		});
	});

	it("ignores unknown events", () => {
		expect(refreshPlanForEvent({ event: "unknown.event" })).toEqual({
			refreshSession: false,
			refreshActiveBranch: false,
			refreshList: false,
		});
	});
});
