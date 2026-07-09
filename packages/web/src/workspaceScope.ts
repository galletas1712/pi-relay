import type { ProjectWorkspace } from "./types.ts";
import type { StartSessionWorkspace } from "./agentApi.ts";

const WORKSPACE_SCOPE_STORAGE_KEY = "piRelayWorkspaceScope:v1";

export type WorkspaceScopeStorage = Pick<Storage, "getItem" | "setItem" | "removeItem">;

/** Per-workspace choice for a new session: whether to include it and an optional git branch override. */
export interface WorkspaceScopeEntry {
	workspaceDir: string;
	kind: "git" | "local";
	included: boolean;
	/** Branch override for git workspaces; empty string means the project's default branch. */
	branch: string;
}

interface StoredProjectScope {
	excluded: string[];
	branches: Record<string, string>;
}

interface StoredScopeState {
	byProject: Record<string, StoredProjectScope>;
}

const EMPTY_STORED: StoredProjectScope = { excluded: [], branches: {} };

/**
 * Build the editable scope for a project's current workspaces, layering any persisted
 * exclusions/branch overrides on top. Unknown stored entries are dropped so renamed or
 * removed workspaces never leak into the picker.
 */
export function workspaceScopeForProject(
	projectId: string | null,
	workspaces: ProjectWorkspace[],
	storage = browserStorage(),
): WorkspaceScopeEntry[] {
	const stored = projectId ? readProjectScope(projectId, storage) : EMPTY_STORED;
	const excluded = new Set(stored.excluded);
	const excludesEveryWorkspace =
		workspaces.length > 0 &&
		workspaces.every((workspace) => excluded.has(workspace.workspace_dir));
	return workspaces.map((workspace) => {
		const kind = workspace.kind ?? "git";
		return {
			workspaceDir: workspace.workspace_dir,
			kind,
			included: excludesEveryWorkspace || !excluded.has(workspace.workspace_dir),
			branch: kind === "git" ? (stored.branches[workspace.workspace_dir] ?? "") : "",
		};
	});
}

/** Persist a project's scope, recording only the deviations from "all included, default branch". */
export function rememberWorkspaceScope(
	projectId: string | null,
	scope: WorkspaceScopeEntry[],
	storage = browserStorage(),
): void {
	if (!projectId || !storage) return;
	const excluded = scope.filter((entry) => !entry.included).map((entry) => entry.workspaceDir);
	const branches: Record<string, string> = {};
	for (const entry of scope) {
		const branch = entry.branch.trim();
		if (entry.kind === "git" && branch) branches[entry.workspaceDir] = branch;
	}
	const state = readScopeState(storage);
	if (!excluded.length && !Object.keys(branches).length) delete state.byProject[projectId];
	else state.byProject[projectId] = { excluded, branches };
	writeScopeState(state, storage);
}

/**
 * Convert the editable scope into the `session.start` `workspaces` payload, or `undefined`
 * when the scope matches the default (every workspace included at its default branch) so the
 * daemon keeps its existing behavior.
 */
export function startWorkspacesFromScope(scope: WorkspaceScopeEntry[]): StartSessionWorkspace[] | undefined {
	const included = scope.filter((entry) => entry.included);
	if (scope.length > 0 && included.length === 0) {
		throw new Error("At least one project workspace must be selected");
	}
	const hasBranchOverride = included.some((entry) => entry.kind === "git" && entry.branch.trim());
	if (included.length === scope.length && !hasBranchOverride) return undefined;
	return included.map((entry) => ({
		workspaceDir: entry.workspaceDir,
		branch: entry.kind === "git" && entry.branch.trim() ? entry.branch.trim() : undefined,
	}));
}

function readProjectScope(projectId: string, storage: WorkspaceScopeStorage | null): StoredProjectScope {
	return readScopeState(storage).byProject[projectId] ?? EMPTY_STORED;
}

function readScopeState(storage: WorkspaceScopeStorage | null): StoredScopeState {
	if (!storage) return { byProject: {} };
	try {
		const raw = storage.getItem(WORKSPACE_SCOPE_STORAGE_KEY);
		if (!raw) return { byProject: {} };
		return normalizeState(JSON.parse(raw) as unknown);
	} catch {
		return { byProject: {} };
	}
}

function writeScopeState(state: StoredScopeState, storage: WorkspaceScopeStorage | null): void {
	if (!storage) return;
	try {
		storage.setItem(WORKSPACE_SCOPE_STORAGE_KEY, JSON.stringify(state));
	} catch {
		// localStorage can be unavailable or full; scope persistence is best-effort.
	}
}

function normalizeState(value: unknown): StoredScopeState {
	if (!isRecord(value) || !isRecord(value.byProject)) return { byProject: {} };
	const byProject: Record<string, StoredProjectScope> = {};
	for (const [projectId, raw] of Object.entries(value.byProject)) {
		if (!isRecord(raw)) continue;
		const excluded = Array.isArray(raw.excluded)
			? raw.excluded.filter((dir): dir is string => typeof dir === "string")
			: [];
		const branches: Record<string, string> = {};
		if (isRecord(raw.branches)) {
			for (const [dir, branch] of Object.entries(raw.branches)) {
				if (typeof branch === "string" && branch.trim()) branches[dir] = branch;
			}
		}
		byProject[projectId] = { excluded, branches };
	}
	return { byProject };
}

function isRecord(value: unknown): value is Record<string, unknown> {
	return typeof value === "object" && value !== null && !Array.isArray(value);
}

function browserStorage(): WorkspaceScopeStorage | null {
	if (typeof window === "undefined") return null;
	try {
		return window.localStorage ?? null;
	} catch {
		return null;
	}
}

export { WORKSPACE_SCOPE_STORAGE_KEY };
