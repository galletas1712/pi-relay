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
