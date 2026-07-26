import { describe, expect, it } from "vitest";
import { formatWorkspacePreparationStatus } from "./workspaceMaterializeProgress.ts";

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
});
