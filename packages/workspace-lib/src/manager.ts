// WorkspaceManager — TypeScript port of pi-relay's
// rust/crates/agent-runtime/src/workspaces/mod.rs session lifecycle:
//
//   <stateRoot>/workspace-bases/<projectId>/<workspaceDir>/{metadata.json,base/}
//   <stateRoot>/sessions/<sessionId>/cwd          ← btrfs subvolume (per session)
//
// Session materialize: refresh each declared project base (git fetch+reset or
// rsync --delete for local), then `cp -a --reflink=always` into the fresh cwd
// subvolume; git workspaces get a `pi/session/<sid>/<wdir>` local branch.
// Fork = `btrfs subvolume snapshot` of the parent's cwd. Destroy deletes the
// subvolume. Non-btrfs filesystems fall back to plain dirs + `cp -a
// --reflink=auto` (validateRoot reports `btrfs:false`; ops stay correct, just
// not cheap). Bridge boot calls validateRoot and logs the mode.
import { existsSync, lstatSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { mkdir, readdir, rm } from "node:fs/promises";
import { join } from "node:path";
import { createSubvolume, deleteSubvolume, isSubvolume, probeBtrfs, snapshotSubvolume } from "./btrfs.ts";
import {
	WORKSPACE_BASE_DIR,
	WORKSPACE_BASE_METADATA,
	type WorkspaceBaseConfig,
	branchComponent,
	pathComponent,
	readWorkspaceBaseConfig,
	validateWorkspaceDir,
	workspaceBaseConfig,
	writeWorkspaceBaseConfig,
} from "./config.ts";
import { WorkspaceError } from "./errors.ts";
import { run } from "./exec.ts";
import { checkoutSessionBranch, fetchSessionBranchHead, refreshGitWorkspaceBase, revParse } from "./git.ts";
import { refreshLocalWorkspaceBase, sanitizeCopiedTree } from "./local.ts";
import { KeyedMutex } from "./locks.ts";
import type { MaterializedSession, SessionWorkspace, WorkspaceDecl } from "./types.ts";

export interface ValidateRootReport {
	stateRoot: string;
	btrfs: boolean;
}

export class WorkspaceManager {
	readonly stateRoot: string;
	private btrfs: boolean | null = null;
	private readonly mutex = new KeyedMutex();

	constructor(stateRoot: string) {
		this.stateRoot = stateRoot;
	}

	// ---- layout -------------------------------------------------------------

	sessionRoot(sessionId: string): string {
		return join(this.stateRoot, "sessions", pathComponent(sessionId));
	}

	/** Session cwd (the subvolume). Rust: resolve(workspace_id). */
	resolve(sessionId: string): string {
		return join(this.sessionRoot(sessionId), "cwd");
	}

	private workspaceBasesRoot(projectId: string): string {
		return join(this.stateRoot, "workspace-bases", pathComponent(projectId));
	}

	private baseSlot(projectId: string, workspaceDir: string): string {
		return join(this.workspaceBasesRoot(projectId), workspaceDir);
	}

	// ---- root validation (port of validate_root) -----------------------------

	async validateRoot(): Promise<ValidateRootReport> {
		mkdirSync(this.stateRoot, { recursive: true });
		mkdirSync(join(this.stateRoot, "sessions"), { recursive: true });
		mkdirSync(join(this.stateRoot, "workspace-bases"), { recursive: true });
		this.btrfs = await probeBtrfs(this.stateRoot);
		return { stateRoot: this.stateRoot, btrfs: this.btrfs };
	}

	private async useBtrfs(): Promise<boolean> {
		if (this.btrfs === null) await this.validateRoot();
		return this.btrfs === true;
	}

	// ---- session lifecycle ----------------------------------------------------

	/** Port of materialize_session: create the cwd subvolume, then materialize
	 * each selected workspace. Any failure tears the whole session tree down. */
	async materializeSession(sessionId: string, projectId: string, decls: WorkspaceDecl[]): Promise<MaterializedSession> {
		const root = this.sessionRoot(sessionId);
		if (existsSync(root)) throw new WorkspaceError("workspace_state", `session workspace already exists: ${root}`);
		mkdirSync(root, { recursive: true });
		const cwd = join(root, "cwd");
		const btrfs = await this.useBtrfs();
		if (btrfs) {
			await createSubvolume(cwd);
		} else {
			mkdirSync(cwd, { recursive: true });
		}
		try {
			const workspaces = await this.mutex.with(`project-bases:${projectId}`, async () => {
				await this.removeStaleWorkspaceBases(projectId, decls);
				const out: SessionWorkspace[] = [];
				for (const decl of decls) {
					out.push(await this.materializeWorkspace(projectId, sessionId, cwd, decl));
				}
				return out;
			});
			const meta = { sessionId, projectId, subvolume: btrfs, workspaces, materializedAt: new Date().toISOString() };
			writeFileSync(join(root, "materialize.json"), JSON.stringify(meta, null, 2) + "\n");
			return { sessionId, sessionRoot: root, cwd, subvolume: btrfs, workspaces };
		} catch (err) {
			await this.destroySession(sessionId).catch(() => {});
			throw err;
		}
	}

	private async materializeWorkspace(projectId: string, sessionId: string, cwd: string, decl: WorkspaceDecl): Promise<SessionWorkspace> {
		return this.withRefreshedBase(projectId, decl, async (base) => {
			const workspaceDir = base.config.workspace_dir;
			const target = join(cwd, workspaceDir);
			if (existsSync(target)) throw new WorkspaceError("workspace_state", `session workspace already exists: ${target}`);
			await this.populateWorkspace(base.path, target);
			if (base.config.kind === "git") {
				const remoteUrl = base.config.remote_url ?? "";
				const defaultBranch = base.config.remote_branch ?? "";
				const localBranch = `pi/session/${branchComponent(sessionId)}/${branchComponent(workspaceDir)}`;
				let sessionBranch = defaultBranch;
				let baseSha: string;
				if (decl.branchOverride && decl.branchOverride !== defaultBranch) {
					baseSha = await fetchSessionBranchHead(target, decl.branchOverride);
					sessionBranch = decl.branchOverride;
				} else {
					baseSha = await revParse(target, "HEAD");
				}
				await checkoutSessionBranch(target, localBranch, baseSha);
				return { workspaceDir, kind: "git", commitOid: baseSha, branch: sessionBranch, localBranch };
			}
			return { workspaceDir, kind: "local" };
		});
	}

	/** Port of populate_workspace: cp -a --reflink (always on btrfs) + sanitize. */
	private async populateWorkspace(source: string, target: string): Promise<void> {
		await mkdir(target);
		const btrfs = await this.useBtrfs();
		try {
			await run("cp", ["-a", btrfs ? "--reflink=always" : "--reflink=auto", `${source}/.`, target]);
		} catch (err) {
			if (btrfs) {
				throw new WorkspaceError("workspace_state", `required btrfs reflink failed from ${source} to ${target}: ${String(err)}`);
			}
			throw err;
		}
		sanitizeCopiedTree(target);
	}

	/** Port of refresh_workspace_base: keyed slot lock, wipe changed/stale slots,
	 * refresh (git fetch+reset / rsync), write metadata — and run `fn` while the
	 * slot lock is still held (Rust's WorkspaceBase carries the slot guard so the
	 * session copy reads a stable base tree). */
	private async withRefreshedBase<T>(
		projectId: string,
		decl: WorkspaceDecl,
		fn: (base: { path: string; config: WorkspaceBaseConfig }) => Promise<T>,
	): Promise<T> {
		const config = workspaceBaseConfig(decl);
		const slot = this.baseSlot(projectId, config.workspace_dir);
		const metadataPath = join(slot, WORKSPACE_BASE_METADATA);
		const basePath = join(slot, WORKSPACE_BASE_DIR);
		return this.mutex.with(`base:${projectId}:${config.workspace_dir}`, async () => {
			const existing = readWorkspaceBaseConfig(metadataPath);
			const same = existing !== null && JSON.stringify(existing) === JSON.stringify(config);
			if (existsSync(slot) && (!same || !existsSync(basePath))) {
				rmSync(slot, { recursive: true, force: true });
			}
			mkdirSync(basePath, { recursive: true });
			if (config.kind === "git") {
				await refreshGitWorkspaceBase(basePath, config);
			} else {
				await refreshLocalWorkspaceBase(basePath, config);
			}
			writeWorkspaceBaseConfig(metadataPath, config);
			return fn({ path: basePath, config });
		});
	}

	/** Port of ensure_session: all declared workspace dirs (and .git for git
	 * workspaces) must exist. Used on respawn/restore. */
	async ensureSession(sessionId: string, workspaces: SessionWorkspace[]): Promise<void> {
		if (workspaces.length === 0) return;
		const cwd = this.resolve(sessionId);
		for (const ws of workspaces) {
			const dir = validateWorkspaceDir(ws.workspaceDir);
			const target = join(cwd, dir);
			if (!existsSync(target) || !lstatSync(target).isDirectory()) {
				throw new WorkspaceError("workspace_missing", `session workspace is missing: ${target}`);
			}
			if (ws.kind === "git" && !existsSync(join(target, ".git"))) {
				throw new WorkspaceError("workspace_missing", `session git workspace is missing .git: ${target}`);
			}
		}
	}

	/** Port of ensure_session_owns_cwd: root+cwd must be real directories
	 * (no symlink shenanigans). */
	ensureSessionOwnsCwd(sessionId: string): void {
		const root = this.sessionRoot(sessionId);
		const cwd = this.resolve(sessionId);
		const rootMeta = lstatSync(root);
		if (rootMeta.isSymbolicLink() || !rootMeta.isDirectory()) {
			throw new WorkspaceError("workspace_state", `managed session root is not a directory: ${root}`);
		}
		const cwdMeta = lstatSync(cwd);
		if (cwdMeta.isSymbolicLink() || !cwdMeta.isDirectory()) {
			throw new WorkspaceError("workspace_state", `managed session cwd is not a directory: ${cwd}`);
		}
	}

	/** Port of fork_session_from_parent: btrfs snapshot of the parent cwd into a
	 * fresh child root; strip .pi-handoff; git workspaces get a child-local branch
	 * (and are validated to have an isolated .git). */
	async forkSessionFromParent(
		parentSessionId: string,
		parentWorkspaces: SessionWorkspace[],
		childSessionId: string,
	): Promise<MaterializedSession> {
		if (parentSessionId === childSessionId) {
			throw new WorkspaceError("bad_workspace", "child session id must differ from parent session id");
		}
		await this.ensureSession(parentSessionId, parentWorkspaces);
		const childRoot = this.sessionRoot(childSessionId);
		const parentCwd = this.resolve(parentSessionId);
		if (childRoot.startsWith(parentCwd)) {
			throw new WorkspaceError("bad_workspace", `child session root ${childRoot} must not be inside parent cwd ${parentCwd}`);
		}
		if (existsSync(childRoot)) {
			throw new WorkspaceError("workspace_state", `child session workspace already exists: ${childRoot}`);
		}
		await mkdir(childRoot, { recursive: true });
		const btrfs = await this.useBtrfs();
		const childCwd = join(childRoot, "cwd");
		try {
			this.ensureSessionOwnsCwd(parentSessionId);
			if (btrfs) {
				await snapshotSubvolume(parentCwd, childCwd);
			} else {
				await mkdir(childCwd);
				await run("cp", ["-a", "--reflink=auto", `${parentCwd}/.`, childCwd]);
			}
			await rm(join(childCwd, ".pi-handoff"), { recursive: true, force: true });
			const childWorkspaces: SessionWorkspace[] = [];
			for (const ws of parentWorkspaces) {
				const dir = validateWorkspaceDir(ws.workspaceDir);
				const childWsRoot = join(childCwd, dir);
				const childWs: SessionWorkspace = { ...ws };
				if (ws.kind === "git") {
					await this.validateGitWorkspaceIsolated(childWsRoot);
					const localBranch = `pi/session/${branchComponent(childSessionId)}/${branchComponent(dir)}`;
					const head = await revParse(childWsRoot, "HEAD");
					await checkoutSessionBranch(childWsRoot, localBranch, head);
					childWs.localBranch = localBranch;
				}
				childWorkspaces.push(childWs);
			}
			return { sessionId: childSessionId, sessionRoot: childRoot, cwd: childCwd, subvolume: btrfs, workspaces: childWorkspaces };
		} catch (err) {
			await this.destroySession(childSessionId).catch(() => {});
			throw err;
		}
	}

	private async validateGitWorkspaceIsolated(workspaceRoot: string): Promise<void> {
		if (!existsSync(workspaceRoot)) {
			throw new WorkspaceError("workspace_state", `child git workspace is missing: ${workspaceRoot}`);
		}
		const { realpath } = await import("node:fs/promises");
		const gitDir = (await run("git", ["rev-parse", "--git-dir"], { cwd: workspaceRoot })).stdout.trim();
		const commonDir = (await run("git", ["rev-parse", "--git-common-dir"], { cwd: workspaceRoot })).stdout.trim();
		const rootReal = await realpath(workspaceRoot);
		const resolveGit = async (p: string) => realpath(p.startsWith("/") ? p : join(rootReal, p));
		const gd = await resolveGit(gitDir);
		const cd = await resolveGit(commonDir);
		if (!gd.startsWith(rootReal)) {
			throw new WorkspaceError("workspace_state", `child git dir ${gd} escapes workspace ${rootReal}`);
		}
		if (!cd.startsWith(rootReal)) {
			throw new WorkspaceError("workspace_state", `child git common dir ${cd} escapes workspace ${rootReal}`);
		}
	}

	/** Port of destroy_session_workspaces: delete the cwd subvolume + the session
	 * root. Idempotent. */
	async destroySession(sessionId: string): Promise<void> {
		const root = this.sessionRoot(sessionId);
		const cwd = join(root, "cwd");
		if (existsSync(cwd)) {
			if (await isSubvolume(cwd)) {
				await deleteSubvolume(cwd);
			} else {
				rmSync(cwd, { recursive: true, force: true });
			}
		}
		try {
			await rm(root, { recursive: true });
		} catch {
			/* NotFound is fine: teardown is idempotent */
		}
	}

	private async removeStaleWorkspaceBases(projectId: string, decls: WorkspaceDecl[]): Promise<void> {
		const root = this.workspaceBasesRoot(projectId);
		if (!existsSync(root)) return;
		const expected = new Set(decls.map((d) => workspaceBaseConfig(d).workspace_dir));
		for (const entry of await readdir(root)) {
			if (!expected.has(entry)) {
				rmSync(join(root, entry), { recursive: true, force: true });
			}
		}
	}

	async removeProjectBases(projectId: string): Promise<void> {
		const root = this.workspaceBasesRoot(projectId);
		if (existsSync(root)) rmSync(root, { recursive: true, force: true });
	}

	/** Persisted materialize.json for restore-after-bridge-restart. */
	readMaterialized(sessionId: string): MaterializedSession | null {
		const path = join(this.sessionRoot(sessionId), "materialize.json");
		if (!existsSync(path)) return null;
		try {
			const meta = JSON.parse(readFileSync(path, "utf8"));
			return {
				sessionId,
				sessionRoot: this.sessionRoot(sessionId),
				cwd: this.resolve(sessionId),
				subvolume: meta.subvolume !== false,
				workspaces: (meta.workspaces ?? []) as SessionWorkspace[],
			};
		} catch {
			return null;
		}
	}
}
