// W1: workspace lifecycle on real btrfs — validate root probe, git+local base
// refresh, per-session subvolume materialize, fork snapshot, destroy; plus
// confined browse (list/read/write/search) and git status/diff.
// Requires: btrfs filesystem (packages/ is on /home = btrfs here).
import { execFileSync } from "node:child_process";
import { mkdtempSync, mkdirSync, writeFileSync, symlinkSync, existsSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import assert from "node:assert/strict";
import {
	WorkspaceManager,
	isSubvolume,
	listDir,
	readFileRange,
	writeFileConfined,
	search,
	gitStatus,
	gitDiff,
	gitDiffAll,
	validateBrowsePath,
	WorkspaceError,
} from "../src/index.ts";

const RUN = mkdtempSync(join(import.meta.dirname, ".tmp-w1-"));
const STATE = join(RUN, "state");
const PROJECT = "proj-1";

function sh(cmd, args, cwd) {
	return execFileSync(cmd, ["-c", "commit.gpgsign=false", ...args], { cwd, encoding: "utf8", env: { ...process.env, GIT_TERMINAL_PROMPT: "0", GIT_AUTHOR_NAME: "t", GIT_AUTHOR_EMAIL: "t@t", GIT_COMMITTER_NAME: "t", GIT_COMMITTER_EMAIL: "t@t" } }).trim();
}

// fixture: a git "remote" (local path) + a local source dir
const REMOTE = join(RUN, "remote-repo");
mkdirSync(REMOTE, { recursive: true });
sh("git", ["init", "-b", "main"], REMOTE);
writeFileSync(join(REMOTE, "README.md"), "hello from git\n");
writeFileSync(join(REMOTE, "AGENTS.md"), "GIT-ROOT-INSTRUCTIONS\n");
mkdirSync(join(REMOTE, "src"));
writeFileSync(join(REMOTE, "src", "app.ts"), "export const v = 1;\n");
sh("git", ["add", "-A"], REMOTE);
sh("git", ["commit", "-m", "init"], REMOTE);
// a second branch for the override path
sh("git", ["checkout", "-b", "feature-x"], REMOTE);
writeFileSync(join(REMOTE, "FEATURE.md"), "feature branch file\n");
sh("git", ["add", "-A"], REMOTE);
sh("git", ["commit", "-m", "feature"], REMOTE);
sh("git", ["checkout", "main"], REMOTE);

const LOCAL_SRC = join(RUN, "local-src");
mkdirSync(join(LOCAL_SRC, "docs"), { recursive: true });
writeFileSync(join(LOCAL_SRC, "docs", "guide.md"), "local guide\n");
writeFileSync(join(LOCAL_SRC, "AGENTS.md"), "LOCAL-DOCS-INSTRUCTIONS\n");
symlinkSync("/etc/passwd", join(LOCAL_SRC, "evil-link")); // must be sanitized away

const mgr = new WorkspaceManager(STATE);

test("validateRoot probes btrfs", async () => {
	const report = await mgr.validateRoot();
	assert.equal(report.btrfs, true, "packages/ tree must be on btrfs for W1");
});

let mat;
test("materializeSession: git + local workspaces into a fresh subvolume", async () => {
	mat = await mgr.materializeSession("sess-a", PROJECT, [
		{ kind: "git", workspaceDir: "repo", remoteUrl: REMOTE, remoteBranch: "main" },
		{ kind: "local", workspaceDir: "docs", sourcePath: LOCAL_SRC },
	]);
	assert.equal(mat.subvolume, true);
	assert.equal(await isSubvolume(mat.cwd), true, "cwd must be a real btrfs subvolume");
	assert.equal(readFileSync(join(mat.cwd, "repo", "README.md"), "utf8"), "hello from git\n");
	assert.equal(readFileSync(join(mat.cwd, "docs", "docs", "guide.md"), "utf8"), "local guide\n");
	// sanitize: absolute symlink replaced by marker text
	const marker = readFileSync(join(mat.cwd, "docs", "evil-link"), "utf8");
	assert.match(marker, /skipped external symlink target/);
	// session branch created on the git copy
	const branch = sh("git", ["branch", "--show-current"], join(mat.cwd, "repo"));
	assert.equal(branch, "pi/session/sess-a/repo");
	// reflink copy carries the git objects (commit oid recorded)
	const gitWs = mat.workspaces.find((w) => w.workspaceDir === "repo");
	assert.match(gitWs.commitOid ?? "", /^[0-9a-f]{40}$/);
});

test("ensureSession passes; missing dir fails typed", async () => {
	await mgr.ensureSession("sess-a", mat.workspaces);
	await assert.rejects(
		mgr.ensureSession("sess-a", [{ workspaceDir: "nope", kind: "local" }]),
		(err) => err instanceof WorkspaceError && err.code === "workspace_missing",
	);
});

test("browse: list/read/write/search confined to the session cwd", async () => {
	const root = await listDir(mat.cwd, "", undefined, 50);
	assert.deepEqual(root.entries.map((e) => e.name).sort(), ["docs", "repo"]);

	const prefix = await readFileRange(mat.cwd, "repo/README.md");
	assert.equal(Buffer.from(prefix.contentBase64, "base64").toString(), "hello from git\n");
	assert.equal(prefix.eof, true);

	// paging
	const page1 = await listDir(mat.cwd, "", undefined, 1);
	assert.equal(page1.entries.length, 1);
	const page2 = await listDir(mat.cwd, "", page1.nextAfterName, 1);
	assert.equal(page2.entries.length, 1);
	assert.notEqual(page1.entries[0].name, page2.entries[0].name);

	// write (control-plane) then read back
	await writeFileConfined(mat.cwd, "repo/notes.txt", Buffer.from("note body\n").toString("base64"));
	assert.equal(readFileSync(join(mat.cwd, "repo", "notes.txt"), "utf8"), "note body\n");

	// confinement rejections
	for (const bad of ["../escape", "/abs", "a//b", "a/./b", "a/../b", "trail/"]) {
		assert.throws(() => validateBrowsePath(bad), /path/);
	}
	await assert.rejects(readFileRange(mat.cwd, "../state"), (err) => err instanceof WorkspaceError);

	// symlink refusal: make an in-cwd symlink and confirm read refuses it
	symlinkSync("README.md", join(mat.cwd, "repo", "link.md"));
	await assert.rejects(readFileRange(mat.cwd, "repo/link.md"), /symlink/);

	const hits = await search(mat.cwd, "hello from git", { fixedString: true });
	assert.equal(hits.matches.length, 1);
	assert.equal(hits.matches[0].path, "repo/README.md");
	const reHits = await search(mat.cwd, "GIT-ROOT-INSTR.*");
	assert.ok(reHits.matches.some((m) => m.path === "repo/AGENTS.md"));
});

test("git status/diff against working_tree and branch", async () => {
	writeFileSync(join(mat.cwd, "repo", "README.md"), "hello from git\nchanged line\n");
	writeFileSync(join(mat.cwd, "repo", "newfile.ts"), "export const n = 2;\n");
	const roots = mat.workspaces;
	const st = await gitStatus(mat.cwd, roots, "working_tree");
	const repoRoot = st.roots.find((r) => r.workspaceDir === "repo");
	assert.equal(repoRoot.error, null);
	const byPath = Object.fromEntries(repoRoot.entries.map((e) => [e.path, e.status]));
	assert.equal(byPath["repo/README.md"], "modified");
	assert.equal(byPath["repo/newfile.ts"], "untracked");
	// notes.txt from the previous test is also untracked
	assert.equal(byPath["repo/notes.txt"], "untracked");

	const d = await gitDiff(mat.cwd, "repo/README.md", roots, "working_tree");
	assert.match(d.unified, /\+changed line/);
	const dn = await gitDiff(mat.cwd, "repo/newfile.ts", roots, "working_tree");
	assert.match(dn.unified, /\+export const n = 2/);
	const all = await gitDiffAll(mat.cwd, roots, "working_tree");
	assert.match(all.unified, /README\.md/);

	const stb = await gitStatus(mat.cwd, roots, "branch");
	assert.ok(stb.roots.find((r) => r.workspaceDir === "repo").comparison.mergeBaseOid);
});

test("fork: btrfs snapshot, handoff stripped, child branch renamed", async () => {
	mkdirSync(join(mat.cwd, ".pi-handoff"));
	writeFileSync(join(mat.cwd, ".pi-handoff", "note.md"), "handoff\n");
	const child = await mgr.forkSessionFromParent("sess-a", mat.workspaces, "sess-b");
	assert.equal(await isSubvolume(child.cwd), true);
	assert.equal(existsSync(join(child.cwd, ".pi-handoff")), false, "handoff must not leak into the child");
	const branch = sh("git", ["branch", "--show-current"], join(child.cwd, "repo"));
	assert.equal(branch, "pi/session/sess-b/repo");
	// writes to the child do not touch the parent (CoW isolation)
	writeFileSync(join(child.cwd, "repo", "child-only.txt"), "child\n");
	assert.equal(existsSync(join(mat.cwd, "repo", "child-only.txt")), false);
});

test("branch override materializes the override head", async () => {
	const s = await mgr.materializeSession("sess-c", PROJECT, [
		{ kind: "git", workspaceDir: "repo", remoteUrl: REMOTE, remoteBranch: "main", branchOverride: "feature-x" },
	]);
	assert.equal(readFileSync(join(s.cwd, "repo", "FEATURE.md"), "utf8"), "feature branch file\n");
	const branch = sh("git", ["branch", "--show-current"], join(s.cwd, "repo"));
	assert.equal(branch, "pi/session/sess-c/repo");
	await mgr.destroySession("sess-c");
});

test("destroySession removes subvolume + root (idempotent)", async () => {
	await mgr.destroySession("sess-b");
	await mgr.destroySession("sess-a");
	assert.equal(existsSync(join(STATE, "sessions", "sess-a")), false);
	assert.equal(existsSync(join(STATE, "sessions", "sess-b")), false);
	await mgr.destroySession("sess-a"); // idempotent
	rmSync(RUN, { recursive: true, force: true });
});
