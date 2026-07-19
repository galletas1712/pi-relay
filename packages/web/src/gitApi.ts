import type { GitStatusResponse } from "./types.ts";

export interface GitHttpApi {
	getStatus(sessionId: string, limit: number): Promise<GitStatusResponse>;
}

export class GitHttpError extends Error {
	constructor(
		message: string,
		readonly status: number,
		readonly code?: string,
	) {
		super(message);
		this.name = "GitHttpError";
	}
}

class SameOriginGitHttpApi implements GitHttpApi {
	async getStatus(sessionId: string, limit: number): Promise<GitStatusResponse> {
		const response = await fetch(
			`/api/sessions/${encodeURIComponent(sessionId)}/git?limit=${limit}`,
			{
				method: "GET",
				headers: { Accept: "application/json" },
				credentials: "same-origin",
			},
		);
		if (!response.ok) {
			const detail = await response.json().catch(() => null) as {
				error?: { code?: string; message?: string };
			} | null;
			throw new GitHttpError(
				detail?.error?.message ?? `Git request failed (${response.status})`,
				response.status,
				detail?.error?.code,
			);
		}
		return response.json() as Promise<GitStatusResponse>;
	}
}

export function createGitHttpApi(): GitHttpApi {
	return new SameOriginGitHttpApi();
}

