import { describe, expect, it } from "vitest";
import { buildAgentSystemPrompt } from "../src/system-prompt.js";

describe("buildAgentSystemPrompt", () => {
	it("frames the roster as advisory coordination context", () => {
		const prompt = buildAgentSystemPrompt("Base prompt", {
			role: "root",
			hasParent: false,
		});

		expect(prompt).toContain("If you have running subagents, glance at the subagent roster in your context before interrupting them.");
		expect(prompt).toContain("Treat the roster as advisory coordination context only.");
		expect(prompt).toContain("Prefer fresh `REPORT` and `IDLE` messages over the roster when they disagree.");
		expect(prompt).toContain("Do not restate or audit the roster unless it changes what you should do next.");
	});
});
