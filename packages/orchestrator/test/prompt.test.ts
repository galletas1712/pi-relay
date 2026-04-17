import {
	PromptAssembly,
	type PromptContext,
} from "@pi-relay/coding-agent";
import { describe, expect, it } from "vitest";
import {
	BackgroundCapabilitiesSource,
	MultiAgentInstructionsSource,
} from "../src/prompt/index.js";

function makeCtx(): PromptContext {
	return {
		sessionId: "test",
		cwd: "/tmp",
		now: new Date("2026-04-16T00:00:00Z"),
		toolNames: ["read", "bash", "spawn"],
	};
}

describe("BackgroundCapabilitiesSource", () => {
	it("contributes the background tool description under capabilities", () => {
		const assembled = new PromptAssembly([new BackgroundCapabilitiesSource()]).assemble(makeCtx());
		const capabilitiesFragments = assembled.sections.get("capabilities") ?? [];
		expect(capabilitiesFragments).toHaveLength(1);
		expect(capabilitiesFragments[0].content).toContain("## Background Tool Execution");
		expect(assembled.text).toContain("__background");
	});
});

describe("MultiAgentInstructionsSource", () => {
	it("uses the root role line when the agent has no parent", () => {
		const source = new MultiAgentInstructionsSource({ role: "root", hasParent: false });
		const assembled = new PromptAssembly([source]).assemble(makeCtx());
		const coordinationFragments = assembled.sections.get("coordination") ?? [];
		expect(coordinationFragments.length).toBeGreaterThan(0);
		expect(assembled.text).toContain("You are the root agent. Your current role label is root.");
		expect(assembled.text).toContain("## Agent Communication");
	});

	it("uses the child role line when the agent has a parent", () => {
		const source = new MultiAgentInstructionsSource({ role: "researcher", hasParent: true });
		const prompt = new PromptAssembly([source]).assemble(makeCtx()).text;
		expect(prompt).toContain("Your role in the current agent tree: researcher.");
		expect(prompt).not.toContain("You are the root agent.");
	});

	it("frames the roster as advisory coordination context", () => {
		const source = new MultiAgentInstructionsSource({ role: "root", hasParent: false });
		const prompt = new PromptAssembly([source]).assemble(makeCtx()).text;
		expect(prompt).toContain(
			"If you have running subagents, glance at the subagent roster in your context before interrupting them.",
		);
		expect(prompt).toContain("Treat the roster as advisory coordination context only.");
		expect(prompt).toContain("Prefer fresh `REPORT` and `IDLE` messages over the roster when they disagree.");
		expect(prompt).toContain("Do not restate or audit the roster unless it changes what you should do next.");
	});
});
