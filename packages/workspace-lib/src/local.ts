// Local workspace base refresh (port of workspaces/local.rs) + tree sanitize
// (port of sanitize.rs: strip absolute/escaping symlinks, drop special files).
import { lstatSync, readdirSync, readlinkSync, rmSync, writeFileSync } from "node:fs";
import { join, isAbsolute } from "node:path";
import { run } from "./exec.ts";
import type { WorkspaceBaseConfig } from "./config.ts";

export async function refreshLocalWorkspaceBase(base: string, config: WorkspaceBaseConfig): Promise<void> {
	const source = config.source_path ?? "";
	// rsync -a --delete --delete-excluded --numeric-ids --no-owner --no-group src/. dst
	await run("rsync", ["-a", "--delete", "--delete-excluded", "--numeric-ids", "--no-owner", "--no-group", `${source}/.`, base]);
	sanitizeCopiedTree(base);
}

function isSafeRelativeSymlink(target: string): boolean {
	if (isAbsolute(target)) return false;
	for (const part of target.split("/")) {
		if (part === "..") return false;
		if (part === "" || part === ".") continue;
	}
	return true;
}

function writeSkippedSymlinkMarker(path: string, target: string): void {
	writeFileSync(path, `pi-relay local workspace copy skipped external symlink target: ${target}\n`);
}

export function sanitizeCopiedTree(dir: string): void {
	for (const entry of readdirSync(dir, { withFileTypes: true })) {
		const child = join(dir, entry.name);
		if (entry.isDirectory()) {
			sanitizeCopiedTree(child);
		} else if (entry.isSymbolicLink()) {
			const target = readlinkSync(child);
			if (!isSafeRelativeSymlink(target)) {
				rmSync(child, { force: true });
				writeSkippedSymlinkMarker(child, target);
			}
		} else if (!entry.isFile()) {
			rmSync(child, { force: true, recursive: true });
		}
	}
}
