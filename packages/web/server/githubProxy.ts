import { execFile } from "node:child_process";
import type { IncomingMessage, ServerResponse } from "node:http";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

export interface GitHubUser {
	login: string;
}

export interface GitHubPullRequest {
	number: number;
	title: string;
	state: string;
	html_url: string;
	updated_at: string;
	user: { login: string };
	head: { ref: string; label: string };
	base: { ref: string };
}

const GITHUB_API = "https://api.github.com";
const GH_TIMEOUT_MS = 10_000;

let cachedToken: { value: string; expiresAtMs: number } | null = null;
const TOKEN_TTL_MS = 60_000;

export async function ghAuthToken(): Promise<string> {
	const now = Date.now();
	if (cachedToken && cachedToken.expiresAtMs > now) {
		return cachedToken.value;
	}
	const { stdout } = await execFileAsync("gh", ["auth", "token"], { timeout: GH_TIMEOUT_MS });
	const value = stdout.trim();
	if (!value) {
		throw new Error("gh auth token returned an empty token. Run `gh auth login`.");
	}
	cachedToken = { value, expiresAtMs: now + TOKEN_TTL_MS };
	return value;
}

async function githubFetch<T>(path: string): Promise<T> {
	const token = await ghAuthToken();
	const response = await fetch(`${GITHUB_API}${path}`, {
		headers: {
			Accept: "application/vnd.github+json",
			Authorization: `Bearer ${token}`,
			"X-GitHub-Api-Version": "2022-11-28",
			"User-Agent": "pi-relay-web",
		},
	});
	if (!response.ok) {
		const body = await response.text().catch(() => "");
		throw new Error(`GitHub API ${response.status}${body ? `: ${body.slice(0, 240)}` : ""}`);
	}
	return (await response.json()) as T;
}

export async function fetchGitHubViewer(): Promise<GitHubUser> {
	return githubFetch<GitHubUser>("/user");
}

export async function fetchOpenPullRequests(owner: string, repo: string): Promise<GitHubPullRequest[]> {
	return githubFetch<GitHubPullRequest[]>(
		`/repos/${encodeURIComponent(owner)}/${encodeURIComponent(repo)}/pulls?state=open&per_page=100`,
	);
}

function sendJson(res: ServerResponse, status: number, body: unknown): void {
	res.statusCode = status;
	res.setHeader("Content-Type", "application/json; charset=utf-8");
	res.end(JSON.stringify(body));
}

function readPathname(url: string | undefined): string {
	if (!url) return "/";
	const question = url.indexOf("?");
	return question === -1 ? url : url.slice(0, question);
}

export async function handleGithubProxyRequest(
	req: IncomingMessage,
	res: ServerResponse,
): Promise<boolean> {
	const pathname = readPathname(req.url);
	if (!pathname.startsWith("/api/github")) return false;
	if (req.method !== "GET") {
		sendJson(res, 405, { error: "method_not_allowed" });
		return true;
	}

	try {
		if (pathname === "/api/github/user") {
			const user = await fetchGitHubViewer();
			sendJson(res, 200, user);
			return true;
		}

		const pullsMatch = pathname.match(/^\/api\/github\/repos\/([^/]+)\/([^/]+)\/pulls$/u);
		if (pullsMatch) {
			const owner = decodeURIComponent(pullsMatch[1]);
			const repo = decodeURIComponent(pullsMatch[2]);
			const pulls = await fetchOpenPullRequests(owner, repo);
			sendJson(res, 200, { pulls });
			return true;
		}

		sendJson(res, 404, { error: "not_found" });
		return true;
	} catch (error) {
		const message = error instanceof Error ? error.message : String(error);
		const status = message.includes("gh auth") || message.includes("executable file not found") ? 503 : 502;
		sendJson(res, status, { error: message });
		return true;
	}
}
