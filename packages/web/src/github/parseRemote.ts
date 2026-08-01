/** Parse a GitHub git remote URL into owner/repo. GitLab and other hosts return null. */
export function parseGithubRemoteUrl(remoteUrl: string): { owner: string; repo: string } | null {
	const trimmed = remoteUrl.trim();
	const httpsMatch = trimmed.match(/^https:\/\/github\.com\/([^/]+)\/([^/.]+?)(?:\.git)?\/?$/iu);
	if (httpsMatch) {
		return { owner: httpsMatch[1], repo: httpsMatch[2] };
	}
	const sshMatch = trimmed.match(/^git@github\.com:([^/]+)\/([^/.]+?)(?:\.git)?$/iu);
	if (sshMatch) {
		return { owner: sshMatch[1], repo: sshMatch[2] };
	}
	return null;
}

export function githubRepoLabel(owner: string, repo: string): string {
	return `${owner}/${repo}`;
}
