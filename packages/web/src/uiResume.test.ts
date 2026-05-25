import { describe, expect, it } from "vitest";
import {
	loadUiSelection,
	rememberSelectedSession,
	rememberUiSelection,
	selectedSessionForProject,
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
