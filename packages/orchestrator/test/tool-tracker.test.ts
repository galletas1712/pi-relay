import { describe, expect, it } from "vitest";
import { ToolCallTracker } from "../src/tool-tracker.js";

describe("ToolCallTracker", () => {
	it("tracks and clears in-flight tool calls", () => {
		const tracker = new ToolCallTracker();
		tracker.register("agent-a", "tool-1", "bash");
		expect(tracker.getInFlightForAgent("agent-a")).toHaveLength(1);
		tracker.complete("tool-1");
		expect(tracker.getInFlightForAgent("agent-a")).toHaveLength(0);
	});

	it("aborts tracked background tools during killAllForAgent", () => {
		const tracker = new ToolCallTracker();
		const abortController = new AbortController();
		tracker.register("agent-a", "tool-1", "bash");
		tracker.attachAbortController("tool-1", abortController);
		tracker.killAllForAgent("agent-a");
		expect(abortController.signal.aborted).toBe(true);
		expect(tracker.getInFlightForAgent("agent-a")).toHaveLength(0);
	});
});
