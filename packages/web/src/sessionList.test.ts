import { describe, expect, it } from "vitest";
import { projectTitle, sessionStatusWithDelegations, sessionTitle } from "./sessionList.ts";
import type { Project, SessionSummary } from "./types.ts";

describe("sessionStatusWithDelegations", () => {
	it("is idle when the parent is idle and no delegations run", () => {
		expect(sessionStatusWithDelegations("idle", false)).toBe("idle");
	});

	it("is delegating when the parent is idle but subagents are in flight", () => {
		expect(sessionStatusWithDelegations("idle", true)).toBe("delegating");
	});

	it("is running when the parent itself is running, regardless of delegations", () => {
		expect(sessionStatusWithDelegations("running", false)).toBe("running");
		expect(sessionStatusWithDelegations("running", true)).toBe("running");
	});

	it("treats a queued parent as running even with delegations in flight", () => {
		expect(sessionStatusWithDelegations("queued", true)).toBe("running");
	});
});

describe("primary identity fallbacks", () => {
	it("never promotes technical ids into default product labels", () => {
		const session = {
			session_id: "session_technical_123",
			metadata: {},
		} as SessionSummary;
		const project = {
			project_id: "project_technical_123",
			name: "",
		} as Project;

		expect(sessionTitle(session)).toBe("Untitled session");
		expect(sessionTitle(session, "Agent")).toBe("Agent");
		expect(projectTitle(project)).toBe("Untitled project");
	});
});
