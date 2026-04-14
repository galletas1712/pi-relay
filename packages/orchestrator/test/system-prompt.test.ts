import { describe, expect, it } from "vitest";
import { buildAgentSystemPrompt } from "../src/system-prompt.js";

describe("buildAgentSystemPrompt", () => {
	it("tells agents to use children for fresh direct-child state", () => {
		const prompt = buildAgentSystemPrompt("Base prompt", {
			role: "root",
			hasParent: false,
		});

		expect(prompt).toContain("`children`: list your current direct child agents and their statuses");
		expect(prompt).toContain("The root agent has `spawn`, `children`, `message`, and `terminate`.");
		expect(prompt).toContain(
			"Use `children` when you need a fresh list of your direct child IDs or statuses before you `message` or `terminate` them.",
		);
	});

	it("describes terminate as a permanent cascading shutdown", () => {
		const prompt = buildAgentSystemPrompt("Base prompt", {
			role: "planner",
			hasParent: true,
		});

		expect(prompt).toContain("`terminate`: permanently stop a direct child agent and its descendants");
		expect(prompt).toContain("Child agents have all five tools.");
		expect(prompt).toContain(
			"If a direct child subtree is no longer needed and you do not expect to reactivate it, terminate it instead of leaving it idle indefinitely.",
		);
		expect(prompt).toContain("Termination cascades through that child's descendants.");
		expect(prompt).toContain("Terminated agents cannot be reactivated with `message`.");
		expect(prompt).toContain(
			"Use `children` to inspect idle direct children and terminate the ones whose results are no longer needed.",
		);
	});
});
