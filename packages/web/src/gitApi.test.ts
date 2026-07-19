import { afterEach, describe, expect, it, vi } from "vitest";
import { createGitHttpApi, GitHttpError } from "./gitApi.ts";

afterEach(() => vi.unstubAllGlobals());

describe("GitHttpApi", () => {
	it("uses a separate same-origin GET client with an encoded session identity", async () => {
		const response = {
			session_id: "session/one",
			limit: 50,
			workspaces: [],
			workspaces_truncated: false,
		};
		const fetch = vi.fn(async () => new Response(JSON.stringify(response), {
			status: 200,
			headers: { "content-type": "application/json" },
		}));
		vi.stubGlobal("fetch", fetch);

		await expect(createGitHttpApi().getStatus("session/one", 50)).resolves.toEqual(response);
		expect(fetch).toHaveBeenCalledWith(
			"/api/sessions/session%2Fone/git?limit=50",
			{
				method: "GET",
				headers: { Accept: "application/json" },
				credentials: "same-origin",
			},
		);
	});

	it("surfaces coherent server error details", async () => {
		vi.stubGlobal("fetch", vi.fn(async () => new Response(JSON.stringify({
			error: { code: "invalid_limit", message: "limit is invalid" },
		}), { status: 400 })));

		const error = await createGitHttpApi().getStatus("session", 101).catch((value) => value);
		expect(error).toBeInstanceOf(GitHttpError);
		expect(error).toMatchObject({
			message: "limit is invalid",
			status: 400,
			code: "invalid_limit",
		});
	});
});

