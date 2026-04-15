import { describe, expect, it } from "vitest";
import { buildAgentSystemPrompt } from "../src/system-prompt.js";

describe("buildAgentSystemPrompt", () => {
	it("tells agents to use children for fresh direct-child state", () => {
		const prompt = buildAgentSystemPrompt("Base prompt", {
			role: "root",
			hasParent: false,
		});

		expect(prompt).toContain("`children`: list your current direct child agents and their statuses");
		expect(prompt).toContain("The root agent has `spawn`, `children`, and `message`.");
		expect(prompt).toContain(
			"Use `children` when you need a fresh list of your direct child IDs or statuses before you `message` them.",
		);
	});

	it("does not expose terminate tool in agent guidance", () => {
		const prompt = buildAgentSystemPrompt("Base prompt", {
			role: "planner",
			hasParent: true,
		});

		expect(prompt).not.toContain("terminate");
		expect(prompt).toContain("Child agents have all four tools.");
	});
});
