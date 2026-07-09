// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { SessionRow } from "./panels.tsx";
import type { PullRequestSummary, SessionSummary } from "./types.ts";

beforeAll(() => {
	class ResizeObserver {
		observe() {}
		unobserve() {}
		disconnect() {}
	}
	vi.stubGlobal("ResizeObserver", ResizeObserver);
	HTMLElement.prototype.scrollIntoView ??= () => {};
	HTMLElement.prototype.hasPointerCapture ??= () => false;
	HTMLElement.prototype.setPointerCapture ??= () => {};
	HTMLElement.prototype.releasePointerCapture ??= () => {};
});

afterEach(cleanup);

describe("session pull request associations", () => {
	it("opens one direct workspace-aware PR without selecting the session", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup();
		renderRow([pr({
			number: 14,
			status: "draft",
			url: "https://github.com/example/repo/pull/14",
			workspace_dirs: ["very-long-workspace-directory"],
		})], onSelect);

		const link = screen.getByRole("link", {
			name: "very-long-workspace-directory: draft pull request #14 from example/repo",
		});
		expect(link.getAttribute("href")).toBe("https://github.com/example/repo/pull/14");
		expect(link.getAttribute("title")).toBe(
			"very-long-workspace-directory: draft pull request #14 from example/repo",
		);
		expect(link.querySelector(".lucide-git-pull-request-draft")).toBeTruthy();
		link.addEventListener("click", (event) => event.preventDefault(), { once: true });
		await user.click(link);
		expect(onSelect).not.toHaveBeenCalled();
	});

	it("groups multiple PRs by sorted workspace with status icons and links", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup();
		renderRow([
			pr({ number: 18, status: "open", workspace_dirs: ["repo-z", "repo-a"] }),
			pr({
				number: 11,
				status: "merged",
				url: "https://github.com/example/repo/pull/11",
				workspace_dirs: ["repo-a"],
			}),
		], onSelect);

		const trigger = screen.getByRole("button", {
			name: "2 PRs across 2 workspaces; show pull request details",
		});
		expect(trigger.getAttribute("aria-haspopup")).toBe("menu");
		await user.click(trigger);
		const menu = await screen.findByRole("menu");
		expect(menu.getAttribute("aria-label")).toBe("Pull requests by workspace");
		const groups = menu.querySelectorAll('[role="group"]');
		expect(groups).toHaveLength(2);
		expect(groups[0].textContent).toContain("repo-a");
		expect(groups[0].textContent).toContain("#11");
		expect(groups[0].textContent).toContain("#18");
		expect(groups[1].textContent).toContain("repo-z");
		expect(groups[1].textContent).toContain("#18");
		const mergedLink = screen.getByRole("menuitem", {
			name: "repo-a: merged pull request #11 from example/repo",
		});
		expect(mergedLink.getAttribute("href")).toBe("https://github.com/example/repo/pull/11");
		expect(mergedLink.querySelector(".lucide-git-merge")).toBeTruthy();
		expect(
			screen.getByRole("menuitem", {
				name: "repo-a: ready for review pull request #18 from example/repo",
			}).querySelector(".lucide-git-pull-request"),
		).toBeTruthy();
		await user.click(mergedLink);
		await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
		expect(onSelect).not.toHaveBeenCalled();
	});

	it("honestly summarizes one PR shared by workspaces and supports dismissal", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup();
		renderRow([pr({ workspace_dirs: ["repo-b", "repo-a"] })], onSelect);

		const trigger = screen.getByRole("button", {
			name: "1 PR across 2 workspaces; show pull request details",
		});
		trigger.focus();
		await user.keyboard("{Enter}");
		expect(await screen.findByRole("menu")).toBeTruthy();
		expect(screen.getAllByRole("menuitem")).toHaveLength(2);
		await user.keyboard("{Escape}");
		await waitFor(() => {
			expect(screen.queryByRole("menu")).toBeNull();
			expect(document.activeElement).toBe(trigger);
		});
		await user.click(trigger);
		expect(await screen.findByRole("menu")).toBeTruthy();
		fireEvent.pointerDown(document.body);
		await waitFor(() => expect(screen.queryByRole("menu")).toBeNull());
		expect(onSelect).not.toHaveBeenCalled();
	});
});

function renderRow(pullRequests: PullRequestSummary[], onSelect: () => void) {
	return render(
		<ul>
			<SessionRow
				session={session(pullRequests)}
				selected={false}
				onSelect={onSelect}
				onRename={vi.fn()}
				onArchiveToggle={vi.fn()}
				onDelete={vi.fn()}
			/>
		</ul>,
	);
}

function pr(overrides: Partial<PullRequestSummary> = {}): PullRequestSummary {
	return {
		number: 18,
		status: "open",
		url: "https://github.com/example/repo/pull/18",
		workspace_dirs: ["repo-a"],
		source_repository: "example/repo",
		...overrides,
	};
}

function session(pullRequests: PullRequestSummary[]): SessionSummary {
	return {
		session_id: "session-1",
		project_id: null,
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: { title: "PR session" },
		created_at: "2024-01-01T00:00:00Z",
		updated_at: "2024-01-01T00:00:00Z",
		pull_requests: pullRequests,
	};
}
