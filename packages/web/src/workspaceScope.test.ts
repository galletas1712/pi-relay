import { describe, expect, it } from "vitest";
import {
	rememberWorkspaceScope,
	startWorkspacesFromScope,
	workspaceScopeForProject,
	type WorkspaceScopeEntry,
	type WorkspaceScopeStorage,
} from "./workspaceScope.ts";
import type { ProjectWorkspace } from "./types.ts";

const PROJECT_WORKSPACES: ProjectWorkspace[] = [
	{ kind: "git", workspace_dir: "repo-a", remote_url: "https://example.test/a.git", remote_branch: "main" },
	{ kind: "local", workspace_dir: "docs", source_path: "/srv/docs" },
	{ kind: "git", workspace_dir: "repo-b", remote_url: "https://example.test/b.git", remote_branch: "main" },
];

describe("workspace scope storage", () => {
	it("defaults to every workspace included with no branch override", () => {
		const scope = workspaceScopeForProject("project_1", PROJECT_WORKSPACES, memoryStorage());
		expect(scope).toEqual([
			{ workspaceDir: "repo-a", kind: "git", included: true, branch: "" },
			{ workspaceDir: "docs", kind: "local", included: true, branch: "" },
			{ workspaceDir: "repo-b", kind: "git", included: true, branch: "" },
		]);
	});

	it("round-trips exclusions and branch overrides per project", () => {
		const storage = memoryStorage();
		const edited: WorkspaceScopeEntry[] = [
			{ workspaceDir: "repo-a", kind: "git", included: true, branch: "feature/login" },
			{ workspaceDir: "docs", kind: "local", included: false, branch: "" },
			{ workspaceDir: "repo-b", kind: "git", included: true, branch: "" },
		];
		rememberWorkspaceScope("project_1", edited, storage);

		expect(workspaceScopeForProject("project_1", PROJECT_WORKSPACES, storage)).toEqual(edited);
		// A different project is unaffected and falls back to the default scope.
		expect(workspaceScopeForProject("project_2", PROJECT_WORKSPACES, storage).every((entry) => entry.included)).toBe(true);
	});

	it("drops stored entries for workspaces the project no longer declares", () => {
		const storage = memoryStorage();
		rememberWorkspaceScope(
			"project_1",
			[{ workspaceDir: "removed", kind: "git", included: false, branch: "stale" }],
			storage,
		);
		const scope = workspaceScopeForProject("project_1", PROJECT_WORKSPACES, storage);
		expect(scope.map((entry) => entry.workspaceDir)).toEqual(["repo-a", "docs", "repo-b"]);
		expect(scope.every((entry) => entry.included)).toBe(true);
	});

	it("recovers legacy storage that excluded every declared workspace", () => {
		const storage = memoryStorage();
		rememberWorkspaceScope(
			"project_1",
			PROJECT_WORKSPACES.map((workspace) => ({
				workspaceDir: workspace.workspace_dir,
				kind: workspace.kind ?? "git",
				included: false,
				branch: "",
			})),
			storage,
		);

		expect(
			workspaceScopeForProject("project_1", PROJECT_WORKSPACES, storage).every(
				(entry) => entry.included,
			),
		).toBe(true);
	});
});

describe("startWorkspacesFromScope", () => {
	it("returns undefined when the scope matches the default", () => {
		const scope = workspaceScopeForProject("project_1", PROJECT_WORKSPACES, memoryStorage());
		expect(startWorkspacesFromScope(scope)).toBeUndefined();
	});

	it("serializes only included workspaces with trimmed git branch overrides", () => {
		const scope: WorkspaceScopeEntry[] = [
			{ workspaceDir: "repo-a", kind: "git", included: true, branch: "  feature/login  " },
			{ workspaceDir: "docs", kind: "local", included: false, branch: "" },
			{ workspaceDir: "repo-b", kind: "git", included: true, branch: "" },
		];
		expect(startWorkspacesFromScope(scope)).toEqual([
			{ workspaceDir: "repo-a", branch: "feature/login" },
			{ workspaceDir: "repo-b", branch: undefined },
		]);
	});

	it("rejects an impossible all-excluded scope instead of treating it as all", () => {
		expect(() =>
			startWorkspacesFromScope([
				{ workspaceDir: "repo-a", kind: "git", included: false, branch: "" },
			]),
		).toThrow("At least one project workspace must be selected");
	});
});

function memoryStorage(): WorkspaceScopeStorage {
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
