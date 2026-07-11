import { describe, expect, it } from "vitest";
import {
	activeSessionCountsByProject,
	projectTitle,
	sessionStatusWithDelegations,
	sessionTitle,
} from "./sessionList.ts";
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

describe("activeSessionCountsByProject", () => {
	it("counts running, queued, and idle-delegating summaries separately by project", () => {
		const sessions = [
			{ session_id: "running", project_id: "project-a", activity: "running", has_running_delegations: false },
			{ session_id: "queued", project_id: "project-a", activity: "queued", has_running_delegations: false },
			{ session_id: "delegating", project_id: "project-a", activity: "idle", has_running_delegations: true },
			{ session_id: "idle", project_id: "project-a", activity: "idle", has_running_delegations: false },
			{ session_id: "other-project", project_id: "project-b", activity: "running", has_running_delegations: false },
			{ session_id: "inactive-project", project_id: "project-c", activity: "idle", has_running_delegations: false },
			{ session_id: "host", project_id: null, activity: "running", has_running_delegations: false },
		] as SessionSummary[];

		const counts = activeSessionCountsByProject(sessions);

		expect(counts.get("project-a")).toBe(3);
		expect(counts.get("project-b")).toBe(1);
		expect(counts.get("project-c")).toBeUndefined();
		expect(counts.has("host")).toBe(false);
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
