// Git base refresh + session branch ops (port of workspaces/git.rs).
import { existsSync } from "node:fs";
import { WorkspaceError } from "./errors.ts";
import { run } from "./exec.ts";
import type { WorkspaceBaseConfig } from "./config.ts";

function gitEnv(): NodeJS.ProcessEnv {
	return {
		...process.env,
		GIT_TERMINAL_PROMPT: "0",
		GIT_AUTHOR_NAME: "pi-relay",
		GIT_AUTHOR_EMAIL: "pi-relay@example.invalid",
		GIT_COMMITTER_NAME: "pi-relay",
		GIT_COMMITTER_EMAIL: "pi-relay@example.invalid",
	};
}

async function git(cwd: string, args: string[]): Promise<string> {
	const res = await run("git", args, { cwd, env: gitEnv() });
	return res.stdout.trim();
}

async function gitRemoteExists(cwd: string, name: string): Promise<boolean> {
	try {
		await git(cwd, ["remote", "get-url", name]);
		return true;
	} catch {
		return false;
	}
}

/** refresh_git_workspace_base: init if needed, fetch the configured branch,
 * hard-reset the base to origin/<branch>, clean -ffdx. Base stays detached. */
export async function refreshGitWorkspaceBase(base: string, config: WorkspaceBaseConfig): Promise<void> {
	const remoteUrl = config.remote_url ?? "";
	const remoteBranch = config.remote_branch ?? "";
	const branchRefspec = `+refs/heads/${remoteBranch}:refs/remotes/origin/${remoteBranch}`;
	if (!existsSync(`${base}/.git`)) {
		await git(base, ["init"]);
	}
	if (await gitRemoteExists(base, "origin")) {
		await git(base, ["remote", "set-url", "origin", remoteUrl]);
	} else {
		await git(base, ["remote", "add", "origin", remoteUrl]);
	}
	await git(base, ["fetch", "--prune", "origin", branchRefspec]);
	const originRef = `refs/remotes/origin/${remoteBranch}`;
	const baseSha = await git(base, ["rev-parse", originRef]);
	await git(base, ["checkout", "--detach", baseSha]);
	await git(base, ["reset", "--hard", baseSha]);
	await git(base, ["clean", "-ffdx"]);
}

/** fetch_session_branch_head: fetch a per-session branch override into an
 * already-materialized git workspace; returns the commit sha. */
export async function fetchSessionBranchHead(workspace: string, branch: string): Promise<string> {
	const trimmed = branch.trim();
	if (trimmed === "") throw new WorkspaceError("bad_workspace", "session branch override is required");
	try {
		await run("git", ["check-ref-format", "--branch", trimmed], { env: gitEnv() });
	} catch {
		throw new WorkspaceError("bad_workspace", `session branch override is not a valid git branch name: ${trimmed}`);
	}
	const branchRefspec = `+refs/heads/${trimmed}:refs/remotes/origin/${trimmed}`;
	try {
		await run("git", ["fetch", "--prune", "origin", branchRefspec], { cwd: workspace, env: gitEnv() });
	} catch {
		throw new WorkspaceError("bad_workspace", `session branch override not found on remote: ${trimmed}`);
	}
	return git(workspace, ["rev-parse", `refs/remotes/origin/${trimmed}`]);
}

export async function validateRemoteBranch(remoteUrl: string, remoteBranch: string): Promise<void> {
	const url = remoteUrl.trim();
	const branch = remoteBranch.trim();
	if (url === "") throw new WorkspaceError("bad_workspace", "workspace remote_url is required");
	if (branch === "") throw new WorkspaceError("bad_workspace", "workspace remote_branch is required");
	try {
		await run("git", ["check-ref-format", "--branch", branch], { env: gitEnv() });
	} catch {
		throw new WorkspaceError("bad_workspace", `workspace remote_branch is not a valid git branch name: ${branch}`);
	}
	const res = await run("git", ["ls-remote", "--heads", url, branch], { env: gitEnv() });
	if (res.stdout.trim() === "") {
		throw new WorkspaceError("bad_workspace", `remote branch not found: ${url} ${branch}`);
	}
}

export async function revParse(cwd: string, ref: string): Promise<string> {
	return git(cwd, ["rev-parse", ref]);
}

/** checkout_session_branch: create (or reset) the per-session branch at sha. */
export async function checkoutSessionBranch(workspace: string, branch: string, sha: string): Promise<void> {
	await git(workspace, ["checkout", "-B", branch, sha]);
}

export async function mergeBase(workspace: string, a: string, b: string): Promise<string> {
	return git(workspace, ["merge-base", a, b]);
}

export async function currentBranch(workspace: string): Promise<string | null> {
	const name = await git(workspace, ["branch", "--show-current"]);
	return name === "" ? null : name;
}
