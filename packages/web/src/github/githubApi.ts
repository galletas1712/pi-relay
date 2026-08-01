import type { GitHubPullRequest, GitHubUser } from "./types.ts";

export type { GitHubPullRequest, GitHubUser };

async function readJson<T>(response: Response): Promise<T> {
	const body = (await response.json()) as { error?: string } & T;
	if (!response.ok) {
		throw new Error(typeof body.error === "string" ? body.error : `GitHub proxy ${response.status}`);
	}
	return body;
}

export async function fetchGitHubViewer(): Promise<GitHubUser> {
	const response = await fetch("/api/github/user");
	return readJson<GitHubUser>(response);
}

export async function fetchOpenPullRequests(owner: string, repo: string): Promise<GitHubPullRequest[]> {
	const response = await fetch(
		`/api/github/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/pulls`,
	);
	const body = await readJson<{ pulls: GitHubPullRequest[] }>(response);
	return body.pulls;
}
