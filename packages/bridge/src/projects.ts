// M8: projects — product grouping for sessions + default workspace decls.
// PG is authoritative; validation of WorkspaceDecls mirrors workspace-lib's
// config.ts (the manager re-validates at materialize time).
import { randomUUID } from "node:crypto";
import { validateWorkspaceDir, type WorkspaceDecl } from "@pi-relay/workspace-lib";
import { BridgeError } from "./supervisor.ts";
import * as db from "./db.ts";

/** Parse + validate a client-supplied workspaces array (session.create /
 * project.create/update). Structural validation only; source reachability is
 * validated when the session materializes. */
export function parseWorkspaceDecls(raw: unknown): WorkspaceDecl[] {
	if (!Array.isArray(raw)) throw new BridgeError("bad_request", "params.workspaces must be an array");
	if (raw.length > 16) throw new BridgeError("bad_request", "params.workspaces: max 16 workspace dirs");
	const out: WorkspaceDecl[] = raw.map((w, i) => {
		if (typeof w !== "object" || w === null) throw new BridgeError("bad_request", `params.workspaces[${i}] must be an object`);
		const o = w as Record<string, unknown>;
		const kind = o.kind === "git" ? "git" : o.kind === "local" ? "local" : null;
		if (!kind) throw new BridgeError("bad_request", `params.workspaces[${i}].kind must be "git" or "local"`);
		if (typeof o.workspaceDir !== "string") throw new BridgeError("bad_request", `params.workspaces[${i}].workspaceDir is required`);
		validateWorkspaceDir(o.workspaceDir); // throws WorkspaceError (typed) on bad input
		const decl: WorkspaceDecl = { kind, workspaceDir: o.workspaceDir };
		if (o.remoteUrl !== undefined) decl.remoteUrl = str(o, "remoteUrl", i);
		if (o.remoteBranch !== undefined) decl.remoteBranch = str(o, "remoteBranch", i);
		if (o.branchOverride !== undefined) decl.branchOverride = str(o, "branchOverride", i);
		if (o.sourcePath !== undefined) decl.sourcePath = str(o, "sourcePath", i);
		if (kind === "git" && (!decl.remoteUrl || !decl.remoteBranch)) {
			throw new BridgeError("bad_request", `params.workspaces[${i}]: git workspaces require remoteUrl + remoteBranch`);
		}
		if (kind === "local" && !decl.sourcePath) {
			throw new BridgeError("bad_request", `params.workspaces[${i}]: local workspaces require sourcePath`);
		}
		return decl;
	});
	return out;
}

function str(o: Record<string, unknown>, key: string, i: number): string {
	const v = o[key];
	if (typeof v !== "string" || v === "") throw new BridgeError("bad_request", `params.workspaces[${i}].${key} must be a non-empty string`);
	return v;
}

function shape(row: db.ProjectRow): Record<string, unknown> {
	return {
		projectId: row.id,
		name: row.name,
		workspaces: row.workspaces ?? [],
		createdAt: row.created_at,
		updatedAt: row.updated_at,
	};
}

export async function listProjects(): Promise<Array<Record<string, unknown>>> {
	return (await db.listProjects()).map(shape);
}

export async function createProject(name: string, decls: WorkspaceDecl[]): Promise<Record<string, unknown>> {
	const id = randomUUID();
	await db.insertProject({ id, name, workspaces: decls });
	return { projectId: id, name, workspaces: decls };
}

export async function updateProject(
	id: string,
	fields: { name?: string; workspaces?: WorkspaceDecl[] },
): Promise<Record<string, unknown>> {
	const row = await db.getProject(id);
	if (!row) throw new BridgeError("project_not_found", `unknown project ${id}`);
	await db.updateProject(id, { name: fields.name, workspaces: fields.workspaces });
	const updated = await db.getProject(id);
	return shape(updated!);
}

export async function deleteProject(id: string): Promise<{ deleted: true }> {
	const row = await db.getProject(id);
	if (!row) throw new BridgeError("project_not_found", `unknown project ${id}`);
	await db.deleteProject(id);
	return { deleted: true };
}
