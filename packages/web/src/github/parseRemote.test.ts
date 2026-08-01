import { describe, expect, it } from "vitest";
import { parseGithubRemoteUrl } from "./parseRemote.ts";

describe("parseGithubRemoteUrl", () => {
	it("accepts https GitHub remotes", () => {
		expect(parseGithubRemoteUrl("https://github.com/org/repo-a.git")).toEqual({
			owner: "org",
			repo: "repo-a",
		});
	});

	it("accepts ssh GitHub remotes", () => {
		expect(parseGithubRemoteUrl("git@github.com:org/repo-b.git")).toEqual({
			owner: "org",
			repo: "repo-b",
		});
	});

	it("rejects GitLab remotes", () => {
		expect(parseGithubRemoteUrl("https://gitlab.com/org/repo.git")).toBeNull();
	});
});
