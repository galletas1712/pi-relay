// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { WorkspaceScopeEntry } from "./workspaceScope.ts";
import { WorkspaceScopePicker } from "./workspaceScopePicker.tsx";

afterEach(cleanup);

const scope: WorkspaceScopeEntry[] = [
	{ workspaceDir: "repo-a", kind: "git", included: true, branch: "" },
	{ workspaceDir: "docs", kind: "local", included: false, branch: "" },
];

describe("WorkspaceScopePicker preparation", () => {
	it("shows an accessible spinner only for included workspaces and clears it when idle", () => {
		const { rerender } = render(
			<WorkspaceScopePicker
				scope={scope}
				onChange={() => {}}
				preparingWorkspaceDirs={["repo-a"]}
				open
			/>,
		);

		const preparing = screen.getByRole("status", { name: "Preparing workspace repo-a" });
		expect(preparing.textContent).toBe("Preparing");
		expect(preparing.querySelector("svg")?.classList.contains("spin")).toBe(true);
		expect(screen.queryByRole("status", { name: "Preparing workspace docs" })).toBeNull();

		rerender(
			<WorkspaceScopePicker
				scope={scope}
				onChange={() => {}}
				preparingWorkspaceDirs={[]}
				open
			/>,
		);

		expect(screen.queryByRole("status")).toBeNull();
	});
});
