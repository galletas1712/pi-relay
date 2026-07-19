import { describe, expect, it } from "vitest";
import {
	DEFAULT_GIT_HISTORY_LIMIT,
	expandGitHistory,
	gitHistoryForSession,
} from "./gitQueryState.ts";

describe("Git query state", () => {
	it("resets identity and history when the selected session changes", () => {
		expect(gitHistoryForSession({ sessionId: "one", limit: 50 }, "two")).toEqual({
			sessionId: "two",
			limit: DEFAULT_GIT_HISTORY_LIMIT,
		});
		expect(gitHistoryForSession({ sessionId: "one", limit: 50 }, "one")).toEqual({
			sessionId: "one",
			limit: 50,
		});
	});

	it("uses bounded 12 to 50 to 100 load-more transitions", () => {
		const first = expandGitHistory({ sessionId: "one", limit: 12 }, "one");
		expect(first).toEqual({ sessionId: "one", limit: 50 });
		expect(expandGitHistory(first, "one")).toEqual({ sessionId: "one", limit: 100 });
		expect(expandGitHistory({ sessionId: "one", limit: 100 }, "one")).toEqual({
			sessionId: "one",
			limit: 100,
		});
	});
});

