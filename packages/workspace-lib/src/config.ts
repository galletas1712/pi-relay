// Workspace declaration validation + base metadata (port of workspaces/config.rs).
// pi-relay stored base metadata as metadata.json (serde); we keep that name/format.
import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import { WorkspaceError } from "./errors.ts";
import type { WorkspaceDecl, WorkspaceKind } from "./types.ts";

export const WORKSPACE_BASE_METADATA = "metadata.json";
export const WORKSPACE_BASE_DIR = "base";

export interface WorkspaceBaseConfig {
	kind: WorkspaceKind;
	workspace_dir: string;
	remote_url?: string;
	remote_branch?: string;
	source_path?: string;
}

/** validate_workspace_dir: direct-child name, ASCII [A-Za-z0-9_-], no leading dot. */
export function validateWorkspaceDir(workspaceDir: string): string {
	const dir = workspaceDir.trim();
	if (dir === "") throw new WorkspaceError("bad_workspace", "workspace_dir is required");
	if (dir.startsWith(".")) throw new WorkspaceError("bad_workspace", `workspace_dir must not start with '.': ${dir}`);
	if (dir.includes("/") || dir.includes("\\") || dir === ".." || dir === ".") {
		throw new WorkspaceError("bad_workspace", `workspace_dir must be a direct child name: ${dir}`);
	}
	if (!/^[A-Za-z0-9_-]+$/.test(dir)) {
		throw new WorkspaceError("bad_workspace", `workspace_dir may only contain ASCII letters, digits, '_' and '-': ${dir}`);
	}
	return dir;
}

function required(value: string | undefined, field: string): string {
	const v = (value ?? "").trim();
	if (v === "") throw new WorkspaceError("bad_workspace", `workspace ${field} is required`);
	return v;
}

/** Port of workspace_base_config: validate + normalize a decl for base storage. */
export function workspaceBaseConfig(decl: WorkspaceDecl): WorkspaceBaseConfig {
	const workspaceDir = validateWorkspaceDir(decl.workspaceDir);
	if (decl.kind === "git") {
		return {
			kind: "git",
			workspace_dir: workspaceDir,
			remote_url: required(decl.remoteUrl, "remote_url"),
			remote_branch: required(decl.remoteBranch, "remote_branch"),
		};
	}
	if (decl.kind === "local") {
		const source = required(decl.sourcePath, "source_path");
		if (!existsSync(source) || !statSync(source).isDirectory()) {
			throw new WorkspaceError("bad_workspace", `local workspace source_path is not a directory: ${source}`);
		}
		return { kind: "local", workspace_dir: workspaceDir, source_path: source };
	}
	throw new WorkspaceError("bad_workspace", `unknown workspace kind: ${String(decl.kind)}`);
}

export function readWorkspaceBaseConfig(path: string): WorkspaceBaseConfig | null {
	if (!existsSync(path)) return null;
	try {
		const parsed = JSON.parse(readFileSync(path, "utf8")) as WorkspaceBaseConfig;
		if (parsed && (parsed.kind === "git" || parsed.kind === "local") && typeof parsed.workspace_dir === "string") {
			return parsed;
		}
	} catch {
		/* fall through */
	}
	throw new WorkspaceError("workspace_state", `decode workspace base metadata ${path} failed`);
}

export function writeWorkspaceBaseConfig(path: string, config: WorkspaceBaseConfig): void {
	mkdirSync(dirname(path), { recursive: true });
	writeFileSync(path, JSON.stringify(config, null, 2) + "\n");
}

/** path_component: percent-encode non [A-Za-z0-9_-] bytes (project dir names). */
export function pathComponent(value: string): string {
	let out = "";
	for (const byte of Buffer.from(value, "utf8")) {
		const ch = String.fromCharCode(byte);
		out += /^[A-Za-z0-9_-]$/.test(ch) ? ch : `%${byte.toString(16).padStart(2, "0")}`;
	}
	return out === "" ? "%00" : out;
}

/** branch_component: sanitize a workspace dir for use inside a branch name. */
export function branchComponent(value: string): string {
	const mapped = [...value].map((ch) => (/^[A-Za-z0-9_-]$/.test(ch) ? ch : "-")).join("");
	return mapped === "" ? "%00" : mapped;
}
