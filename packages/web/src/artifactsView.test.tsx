// @vitest-environment jsdom

import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ArtifactsView } from "./artifactsView.tsx";
import type { AgentApi } from "./agentApi.ts";
import type { ConnectionStatus } from "./rpc.ts";
import type { ArtifactsSnapshot, SessionSnapshot } from "./types.ts";

afterEach(cleanup);

function session(kind: "git" | "local" = "git"): SessionSnapshot {
	return {
		session_id: "session-1",
		project_id: null,
		runtime_id: "runtime-1",
		workspace_id: "workspace-1",
		workspaces: [kind === "git"
			? { kind, workspace_dir: "repo", base_sha: "a".repeat(40) }
			: { kind, workspace_dir: "repo", source_path: "/srv/docs" }],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "test" },
		metadata: {},
		pending_actions: [],
		queued_inputs: [],
		last_event_id: 1,
		server_time_ms: 1,
	};
}

function api(overrides: Partial<AgentApi> = {}): AgentApi {
	return {
		getArtifactsSnapshot: vi.fn(async () => ({
			workspace_dir: "repo",
			tree: [
				{ path: "README.md", kind: "file", size: 14 },
				{ path: ".pi-handoff/secret.md", kind: "file", size: 99 },
			],
			git: {
				head: "head",
				branch: "main",
				baseline: "a".repeat(40),
				changes: [{ path: "README.md", status: " M" }],
				truncated: false,
			},
		})),
		readArtifactFile: vi.fn(async () => ({
			path: "README.md",
			contents: "readme contents",
			truncated: false,
		})),
		getArtifactDiff: vi.fn(async () => ({
			path: "README.md",
			contents: "@@ -1 +1 @@",
			truncated: false,
		})),
		listDelegations: vi.fn(async () => ({
			parent_session_id: "session-1",
			delegations: [],
			has_more: false,
		})),
		...overrides,
	} as AgentApi;
}

function renderView(
	apiValue: AgentApi,
	connection: ConnectionStatus = "open",
	workspaceKind: "git" | "local" = "git",
) {
	const client = new QueryClient({
		defaultOptions: { queries: { retry: false, gcTime: Infinity } },
	});
	return render(
		<QueryClientProvider client={client}>
			<ArtifactsView api={apiValue} session={session(workspaceKind)} connection={connection} />
		</QueryClientProvider>,
	);
}

describe("ArtifactsView", () => {
	it("renders the current read-only files and excludes handoff entries", async () => {
		const apiValue = api();
		renderView(apiValue);
		expect(await screen.findByRole("button", { name: /README\.md/ })).toBeTruthy();
		expect(screen.queryByText(".pi-handoff/secret.md")).toBeNull();
		const user = userEvent.setup();
		await user.click(screen.getByRole("button", { name: /README\.md/ }));
		expect(await screen.findByText("readme contents")).toBeTruthy();
		expect(apiValue.readArtifactFile).toHaveBeenCalledWith("session-1", "repo", "README.md");
	});

	it("renders Git changes and reports handoff query errors", async () => {
		const apiValue = api({
			listDelegations: vi.fn(async () => {
				throw new Error("handoff lookup failed");
			}),
		});
		renderView(apiValue);
		const user = userEvent.setup();
		await screen.findByRole("button", { name: /README\.md/ });
		await user.click(screen.getByRole("button", { name: "Changes" }));
		await user.click(screen.getByRole("button", { name: /README\.md/ }));
		expect(await screen.findByText("@@ -1 +1 @@")).toBeTruthy();
		expect(apiValue.getArtifactDiff).toHaveBeenCalledWith("session-1", "repo", "README.md");
		await user.click(screen.getByRole("button", { name: "Handoffs" }));
		expect((await screen.findByRole("alert")).textContent).toContain("handoff lookup failed");
	});

	it("disables workspace selection while disconnected", () => {
		renderView(api(), "connecting");
		expect(screen.getByRole("combobox")).toHaveProperty("disabled", true);
	});

	it("keeps Changes read-only for local workspaces without requesting a diff", async () => {
		const apiValue = api({
			getArtifactsSnapshot: vi.fn(async (): Promise<ArtifactsSnapshot> => ({
				workspace_dir: "repo",
				tree: [{ path: "README.md", kind: "file", size: 14 }],
				git: null,
			})),
		});
		renderView(apiValue, "open", "local");
		const user = userEvent.setup();
		await user.click(await screen.findByRole("button", { name: "Changes" }));
		expect(await screen.findByText(/unavailable because this is a local, non-Git workspace/)).toBeTruthy();
		expect(apiValue.getArtifactDiff).not.toHaveBeenCalled();
	});

	it("filters handoff paths from both sides of a Git rename", async () => {
		const apiValue = api({
			getArtifactsSnapshot: vi.fn(async () => ({
				workspace_dir: "repo",
				tree: [],
				git: {
					head: "head",
					branch: "main",
					baseline: "a".repeat(40),
					changes: [
						{ path: "visible.md", status: "R ", old_path: "old.md" },
						{ path: ".pi-handoff", status: "R ", old_path: "visible.md" },
						{ path: "visible-too.md", status: "R ", old_path: ".pi-handoff" },
					],
					truncated: false,
				},
			})),
		});
		renderView(apiValue);
		const user = userEvent.setup();
		await user.click(await screen.findByRole("button", { name: "Changes" }));
		expect(await screen.findByRole("button", { name: /visible\.md/ })).toBeTruthy();
		expect(screen.queryByText(".pi-handoff")).toBeNull();
		expect(screen.queryByText("visible-too.md")).toBeNull();
	});
});
