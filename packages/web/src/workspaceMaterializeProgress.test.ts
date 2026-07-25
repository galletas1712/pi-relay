import { describe, expect, it } from "vitest";
import {
	formatWorkspacePreparationStatus,
	isUncertainSessionStartError,
} from "./workspaceMaterializeProgress.ts";

describe("workspace materialize progress", () => {
	it("formats a default status without progress", () => {
		expect(formatWorkspacePreparationStatus(null)).toBe("Preparing workspaces…");
	});

	it("formats per-workspace progress", () => {
		expect(
			formatWorkspacePreparationStatus({
				workspace_dir: "repo-a",
				phase: "refreshing_base",
				index: 2,
				total: 5,
			}),
		).toBe("Refreshing repo-a (2/5)…");
	});

	it("recognizes uncertain session-start transport errors", () => {
		expect(isUncertainSessionStartError(new Error("websocket request timed out"))).toBe(true);
		expect(isUncertainSessionStartError(new Error("websocket closed"))).toBe(true);
		expect(isUncertainSessionStartError(new Error("response lost"))).toBe(false);
		expect(isUncertainSessionStartError(new Error("start failed"))).toBe(false);
	});
});
