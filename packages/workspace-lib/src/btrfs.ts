// btrfs subvolume ops (port of workspaces/mod.rs btrfs calls). Every op shells
// to the btrfs CLI; existence checks use `btrfs subvolume show`.
import { existsSync } from "node:fs";
import { run, tryRun } from "./exec.ts";

/** True when `path` exists and is itself a btrfs subvolume root.
 * `btrfs subvolume show` needs elevated caps on some systems; the reliable
 * userspace check is inode number 256 (BTRFS_FIRST_FREE_OBJECTID) at the root
 * of every subvolume. */
export async function isSubvolume(path: string): Promise<boolean> {
	if (!existsSync(path)) return false;
	const res = await tryRun("stat", ["-c", "%i", path], { okCodes: [0, 1] });
	return res.code === 0 && res.stdout.trim() === "256";
}

export async function createSubvolume(path: string): Promise<void> {
	await run("btrfs", ["subvolume", "create", path]);
}

export async function deleteSubvolume(path: string): Promise<void> {
	await run("btrfs", ["subvolume", "delete", path]);
}

export async function snapshotSubvolume(src: string, dst: string): Promise<void> {
	await run("btrfs", ["subvolume", "snapshot", src, dst]);
}

/** Port of validate_root's probe: create + delete a probe subvolume. */
export async function probeBtrfs(root: string): Promise<boolean> {
	const probe = `${root}/.workspace-probe-${process.pid}-${Math.random().toString(36).slice(2)}`;
	try {
		await run("btrfs", ["subvolume", "create", probe], { timeoutMs: 15_000 });
		await run("btrfs", ["subvolume", "delete", probe], { timeoutMs: 15_000 });
		return true;
	} catch {
		try {
			await run("rm", ["-rf", probe], { timeoutMs: 15_000 });
		} catch {
			/* best effort */
		}
		return false;
	}
}
