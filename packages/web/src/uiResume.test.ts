import { describe, expect, it } from "vitest";
import {
	forgetDeletedSessions,
	loadUiSelection,
	rememberActiveUiSelection,
	rememberSelectedSession,
	rememberSelectedSubagent,
	rememberUiSelection,
	selectedSessionForProject,
	selectedSubagentForSession,
	type UiResumeStorage,
} from "./uiResume.ts";

describe("ui resume storage", () => {
	it("remembers the selected session per project", () => {
		const storage = memoryStorage();

		rememberUiSelection("project_a", "session_a", storage);
		rememberUiSelection("project_b", "session_b", storage);

		expect(loadUiSelection(storage)).toMatchObject({
			projectId: "project_b",
			sessionId: "session_b",
		});
		expect(selectedSessionForProject("project_a", storage)).toBe("session_a");
		expect(selectedSessionForProject("project_b", storage)).toBe("session_b");
	});

	it("remembers the selected host session", () => {
		const storage = memoryStorage();

		rememberUiSelection(null, "session_host", storage);
		rememberUiSelection("project_a", "session_a", storage);

		expect(selectedSessionForProject(null, storage)).toBe("session_host");
		expect(loadUiSelection(storage)).toMatchObject({
			projectId: "project_a",
			sessionId: "session_a",
		});

		rememberUiSelection(null, selectedSessionForProject(null, storage), storage);

		expect(loadUiSelection(storage)).toMatchObject({
			projectId: null,
			sessionId: "session_host",
		});
	});

	it("clears only the current project's remembered session", () => {
		const storage = memoryStorage();

		rememberUiSelection("project_a", "session_a", storage);
		rememberUiSelection("project_b", "session_b", storage);
		rememberSelectedSession("project_b", null, storage);

		expect(loadUiSelection(storage)).toMatchObject({
			projectId: "project_b",
			sessionId: null,
		});
		expect(selectedSessionForProject("project_a", storage)).toBe("session_a");
	});

	it("persists intentional no-focus without erasing the project's last root", () => {
		const storage = memoryStorage();

		rememberUiSelection("project_a", "session_a", storage);
		rememberActiveUiSelection("project_a", null, storage);

		expect(loadUiSelection(storage)).toEqual({
			projectId: "project_a",
			sessionId: null,
		});
		expect(selectedSessionForProject("project_a", storage)).toBe("session_a");
	});

	it("remembers the selected subagent independently per parent session", () => {
		const storage = memoryStorage();

		rememberSelectedSubagent("parent_a", "subagent_a", storage);
		rememberSelectedSubagent("parent_b", "subagent_b", storage);

		expect(selectedSubagentForSession("parent_a", storage)).toBe("subagent_a");
		expect(selectedSubagentForSession("parent_b", storage)).toBe("subagent_b");

		rememberSelectedSubagent("parent_b", null, storage);

		expect(selectedSubagentForSession("parent_a", storage)).toBe("subagent_a");
		expect(selectedSubagentForSession("parent_b", storage)).toBeNull();
	});

	it("does not read inherited object properties as remembered subagents", () => {
		const storage = memoryStorage();

		for (const parentId of ["constructor", "toString", "__proto__"]) {
			expect(selectedSubagentForSession(parentId, storage)).toBeNull();
		}

		rememberSelectedSubagent("__proto__", "subagent_proto", storage);
		expect(selectedSubagentForSession("__proto__", storage)).toBe("subagent_proto");
		expect(selectedSubagentForSession("constructor", storage)).toBeNull();
	});

	it("evicts deleted roots and children without disturbing unrelated focus", () => {
		const storage = memoryStorage();
		rememberUiSelection("project_a", "root_a", storage);
		rememberSelectedSession("project_b", "root_b", storage);
		rememberSelectedSubagent("root_a", "child_a", storage);
		rememberSelectedSubagent("root_b", "child_b", storage);

		forgetDeletedSessions(["root_a", "child_a"], storage);

		expect(loadUiSelection(storage)).toEqual({
			projectId: "project_a",
			sessionId: null,
		});
		expect(selectedSessionForProject("project_a", storage)).toBeNull();
		expect(selectedSessionForProject("project_b", storage)).toBe("root_b");
		expect(selectedSubagentForSession("root_a", storage)).toBeNull();
		expect(selectedSubagentForSession("root_b", storage)).toBe("child_b");
	});

	it("hydrates project sessions and parent-scoped subagents from persisted state", () => {
		const storage = memoryStorage();
		storage.setItem("piRelayUiResume:v1", JSON.stringify({
			selectedProjectId: "project_b",
			selectedSessionIdByProject: {
				project_a: "session_a",
				project_b: "session_b",
			},
			selectedSubagentIdByParentSession: {
				session_a: "subagent_a",
				session_b: "subagent_b",
			},
			activeRootSessionId: "session_b",
			updatedAt: 123,
		}));

		expect(loadUiSelection(storage)).toEqual({
			projectId: "project_b",
			sessionId: "session_b",
		});
		expect(selectedSessionForProject("project_a", storage)).toBe("session_a");
		expect(selectedSubagentForSession("session_a", storage)).toBe("subagent_a");
		expect(selectedSubagentForSession("session_b", storage)).toBe("subagent_b");
	});

	it("derives the active root when reading existing v1 state without an active field", () => {
		const storage = memoryStorage();
		storage.setItem("piRelayUiResume:v1", JSON.stringify({
			selectedProjectId: "project_a",
			selectedSessionIdByProject: { project_a: "session_a" },
			updatedAt: 123,
		}));

		expect(loadUiSelection(storage)).toEqual({
			projectId: "project_a",
			sessionId: "session_a",
		});
	});

	it("ignores malformed persisted state", () => {
		const storage = memoryStorage();
		storage.setItem("piRelayUiResume:v1", "{not json");

		expect(loadUiSelection(storage)).toEqual({
			projectId: null,
			sessionId: null,
		});
	});
});

function memoryStorage(): UiResumeStorage {
	const data = new Map<string, string>();
	return {
		getItem: (key) => data.get(key) ?? null,
		setItem: (key, value) => {
			data.set(key, value);
		},
		removeItem: (key) => {
			data.delete(key);
		},
	};
}
