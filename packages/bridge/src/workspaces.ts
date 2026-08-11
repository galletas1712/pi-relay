// M8 bridge workspace integration: owns the WorkspaceManager (pi-relay port in
// packages/workspace-lib) and the workspace.* control-plane operations. Browse
// works from PG state alone (host may be down); materialize/destroy run at
// session lifecycle points in the supervisor.
import { existsSync } from "node:fs";
import {
	WorkspaceManager,
	WorkspaceError,
	gitDiff,
	gitDiffAll,
	gitStatus,
	listDir,
	readFileRange,
	search,
	writeFileConfined,
	type MaterializedSession,
	type SessionWorkspace,
	type WorkspaceDecl,
} from "@pi-relay/workspace-lib";
import { assertWorkspaceStateRootNotLiveBase, config } from "./config.ts";
import * as db from "./db.ts";

export const workspaceManager = new WorkspaceManager(config.workspaceStateRoot);

let bootReport: { btrfs: boolean } | null = null;

/** Boot validation (port of validate_root). Called once from index.ts. */
export async function validateWorkspaceRoot(): Promise<{ btrfs: boolean }> {
	assertWorkspaceStateRootNotLiveBase(config.workspaceStateRoot);
	const report = await workspaceManager.validateRoot();
	bootReport = report;
	if (!report.btrfs) {
		const msg = `[workspaces] ${config.workspaceStateRoot} is NOT btrfs — falling back to plain dirs + cp -a --reflink=auto`;
		if (config.workspaceRequireBtrfs) throw new Error(msg);
		console.error(msg);
	} else {
		console.error(`[workspaces] state root ${config.workspaceStateRoot} verified btrfs (probe subvolume ok)`);
	}
	return report;
}

export function workspaceBtrfs(): boolean | null {
	return bootReport?.btrfs ?? null;
}

// ---- helpers --------------------------------------------------------------------

function sessionWorkspaces(row: db.SessionRow): SessionWorkspace[] {
	return (Array.isArray(row.workspaces) ? row.workspaces : []) as SessionWorkspace[];
}

const UUID_RE = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

async function requireSession(sessionId: string): Promise<db.SessionRow> {
	// A non-UUID id can never exist; answer session_not_found without letting
	// PG's invalid-text-representation escape as an internal error.
	if (!UUID_RE.test(sessionId)) throw new WorkspaceError("session_not_found", `unknown session ${sessionId}`);
	const row = await db.getSession(sessionId);
	if (!row) throw new WorkspaceError("session_not_found", `unknown session ${sessionId}`);
	return row;
}

function mapErr(err: unknown): never {
	if (err instanceof WorkspaceError) throw err;
	throw err instanceof Error ? err : new Error(String(err));
}

// ---- lifecycle (called by the supervisor) ----------------------------------------

export async function materializeForSession(sessionId: string, projectId: string, decls: WorkspaceDecl[]): Promise<MaterializedSession> {
	return workspaceManager.materializeSession(sessionId, projectId, decls);
}

export async function ensureForSession(sessionId: string, workspaces: SessionWorkspace[]): Promise<void> {
	await workspaceManager.ensureSession(sessionId, workspaces);
}

export async function destroyForSession(sessionId: string): Promise<void> {
	await workspaceManager.destroySession(sessionId);
}

/** M11b: fork-time btrfs snapshot of the parent session cwd into a fresh
 * child root (port of fork_session_from_parent; reflink copy fallback on
 * non-btrfs). Only for MANAGED sessions (workspaces.length > 0). */
export async function forkForSession(
	parentSessionId: string,
	parentWorkspaces: SessionWorkspace[],
	childSessionId: string,
): Promise<MaterializedSession> {
	return workspaceManager.forkSessionFromParent(parentSessionId, parentWorkspaces, childSessionId);
}

// ---- workspace.* contract operations ----------------------------------------------

/** workspace.list: the session's workspace roots (multi-dir model). */
export async function workspaceList(sessionId: string): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	const ws = sessionWorkspaces(row);
	return {
		sessionId,
		cwd: row.cwd,
		managed: ws.length > 0,
		btrfs: workspaceBtrfs(),
		workspaces: ws.map((w) => ({
			workspaceDir: w.workspaceDir,
			kind: w.kind,
			commitOid: w.commitOid ?? null,
			branch: w.branch ?? null,
			localBranch: w.localBranch ?? null,
			exists: existsSync(`${row.cwd}/${w.workspaceDir}`),
		})),
	};
}

export async function workspaceListDir(sessionId: string, params: Record<string, unknown>): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	const path = typeof params.path === "string" ? params.path : "";
	const afterName = typeof params.afterName === "string" ? params.afterName : undefined;
	const limit = typeof params.limit === "number" ? params.limit : undefined;
	try {
		return (await listDir(row.cwd, path, afterName, limit)) as unknown as Record<string, unknown>;
	} catch (err) {
		mapErr(err);
	}
}

export async function workspaceRead(sessionId: string, params: Record<string, unknown>): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	if (typeof params.path !== "string" || params.path === "") throw new WorkspaceError("bad_request", "params.path is required");
	const offset = typeof params.offset === "number" ? params.offset : 0;
	const maxBytes = typeof params.maxBytes === "number" ? params.maxBytes : undefined;
	try {
		return (await readFileRange(row.cwd, params.path, offset, maxBytes)) as unknown as Record<string, unknown>;
	} catch (err) {
		mapErr(err);
	}
}

export async function workspaceWrite(sessionId: string, params: Record<string, unknown>): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	if (typeof params.path !== "string" || params.path === "") throw new WorkspaceError("bad_request", "params.path is required");
	if (typeof params.contentBase64 !== "string") throw new WorkspaceError("bad_request", "params.contentBase64 is required");
	try {
		return (await writeFileConfined(row.cwd, params.path, params.contentBase64)) as unknown as Record<string, unknown>;
	} catch (err) {
		mapErr(err);
	}
}

export async function workspaceSearch(sessionId: string, params: Record<string, unknown>): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	if (typeof params.query !== "string" || params.query === "") throw new WorkspaceError("bad_request", "params.query is required");
	try {
		return (await search(row.cwd, params.query, {
			fixedString: params.fixedString === true,
			maxMatches: typeof params.maxMatches === "number" ? params.maxMatches : undefined,
			include: typeof params.include === "string" ? params.include : undefined,
		})) as unknown as Record<string, unknown>;
	} catch (err) {
		mapErr(err);
	}
}

function parseAgainst(params: Record<string, unknown>): "working_tree" | "branch" {
	return params.against === "branch" ? "branch" : "working_tree";
}

export async function workspaceGitStatus(sessionId: string, params: Record<string, unknown>): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	try {
		return (await gitStatus(row.cwd, sessionWorkspaces(row), parseAgainst(params))) as unknown as Record<string, unknown>;
	} catch (err) {
		mapErr(err);
	}
}

/** workspace.gitDiff: per-path when params.path is set; otherwise the
 * concatenated multi-file unified patch the SPA's DiffPanel consumes. */
export async function workspaceGitDiff(sessionId: string, params: Record<string, unknown>): Promise<Record<string, unknown>> {
	const row = await requireSession(sessionId);
	const against = parseAgainst(params);
	const roots = sessionWorkspaces(row);
	try {
		if (typeof params.path === "string" && params.path !== "") {
			const d = await gitDiff(row.cwd, params.path, roots, against);
			return { diff: d.unified, path: d.path, binary: d.binary, truncated: d.truncated, comparison: d.comparison };
		}
		const d = await gitDiffAll(row.cwd, roots, against);
		return { diff: d.unified, truncated: d.truncated };
	} catch (err) {
		mapErr(err);
	}
}
