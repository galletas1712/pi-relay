// @vitest-environment jsdom

import React from "react";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, describe, expect, it } from "vitest";
import {
	NewSessionSetup,
	type WorkspaceConfiguration,
} from "./newSessionSetup.tsx";
import { WorkspaceScopePicker } from "./workspaceScopePicker.tsx";

afterEach(cleanup);

const EMPTY_INVENTORY = { revision: "empty", servers: [] };
const WORKSPACE_SCOPE = [
	{ workspaceDir: "repo", kind: "git" as const, branch: "", included: true },
	{ workspaceDir: "docs", kind: "local" as const, branch: "", included: true },
];
const FINAL_WORKSPACE_SCOPE = [
	WORKSPACE_SCOPE[0],
	{ ...WORKSPACE_SCOPE[1], included: false },
];

function expectPresentAriaControlsResolve(container: HTMLElement): string[] {
	const ids = [...container.querySelectorAll<HTMLElement>("[aria-controls]")]
		.flatMap((control) => control.getAttribute("aria-controls")?.split(/\s+/) ?? []);
	for (const id of ids) {
		const target = document.getElementById(id);
		expect(target, `missing aria-controls target #${id}`).toBeTruthy();
		expect(container.contains(target)).toBe(true);
	}
	return ids;
}

function setup(
	readiness: {
		mcpReady: boolean;
		mcpAuthStatusReady: boolean;
		mcpAuthMutationBlockedReason?: string | null;
	},
	workspaceConfiguration: WorkspaceConfiguration = { status: "ready", scope: null },
) {
	return (
		<NewSessionSetup
			workspaceConfiguration={workspaceConfiguration}
			onWorkspaceScopeChange={() => {}}
			mcpInventory={EMPTY_INVENTORY}
			mcpSelection={new Map()}
			onMcpSelectionChange={() => {}}
			mcpLoading={false}
			mcpReady={readiness.mcpReady}
			mcpError={null}
			onRetryMcp={() => {}}
			mcpAuthStatus={[]}
			mcpAuthStatusReady={readiness.mcpAuthStatusReady}
			onMcpLogin={() => {}}
			onMcpLogout={() => {}}
			mcpAuthMutationBlockedReason={readiness.mcpAuthMutationBlockedReason}
			preparingWorkspaces={false}
		/>
	);
}

describe("NewSessionSetup readiness", () => {
	it("uses one concise page heading and prose-free disclosure labels", () => {
		const { container } = render(setup(
			{ mcpReady: true, mcpAuthStatusReady: true },
			{
				status: "ready",
				scope: WORKSPACE_SCOPE,
			},
		));

		expect(screen.getByRole("heading", { level: 1, name: "New session" })).toBeTruthy();
		expect(screen.getAllByRole("heading", { level: 1 })).toHaveLength(1);
		const workspaces = screen.getByRole("button", { name: /Workspaces/ });
		expect(workspaces.textContent).toContain("All 2 workspaces included");
		expect(workspaces.textContent).not.toContain("Choose which project folders");
		expect(container.querySelector(".setup-disclosure-description")).toBeNull();
		expect(screen.queryByText(/Your first message starts the session/)).toBeNull();
		expect(workspaces.hasAttribute("aria-controls")).toBe(false);
		expectPresentAriaControlsResolve(document.body);
	});

	it("uses instance-safe workspace panel IDs and only references mounted panels", async () => {
		const { container } = render(
			<>
				<WorkspaceScopePicker scope={WORKSPACE_SCOPE} onChange={() => {}} />
				<WorkspaceScopePicker scope={WORKSPACE_SCOPE} onChange={() => {}} />
			</>,
		);
		const toggles = screen.getAllByRole("button", { name: /Workspaces/ });

		expect(toggles).toHaveLength(2);
		expectPresentAriaControlsResolve(container);
		expect(container.querySelectorAll("[aria-controls]")).toHaveLength(0);

		await userEvent.click(toggles[0]);
		const firstIds = expectPresentAriaControlsResolve(container);
		expect(firstIds).toHaveLength(1);

		await userEvent.click(toggles[1]);
		const openIds = expectPresentAriaControlsResolve(container);
		expect(openIds).toHaveLength(2);
		expect(new Set(openIds).size).toBe(openIds.length);

		await userEvent.click(toggles[0]);
		expect(toggles[0].hasAttribute("aria-controls")).toBe(false);
		expectPresentAriaControlsResolve(container);
	});

	it("uses unique in-component visible descriptions for final disabled workspaces", () => {
		const { container } = render(
			<>
				<WorkspaceScopePicker scope={FINAL_WORKSPACE_SCOPE} onChange={() => {}} open />
				<WorkspaceScopePicker scope={FINAL_WORKSPACE_SCOPE} onChange={() => {}} open />
			</>,
		);
		const pickers = [...container.querySelectorAll<HTMLElement>(".workspace-scope")];
		const finalWorkspaces = screen.getAllByRole<HTMLInputElement>("checkbox", { name: /repo/ });
		const descriptionIds = finalWorkspaces.map((checkbox, index) => {
			expect(checkbox.disabled).toBe(true);
			const descriptionId = checkbox.getAttribute("aria-describedby");
			expect(descriptionId).toBeTruthy();
			const description = document.getElementById(descriptionId!);
			expect(description?.textContent).toBe("Minimum 1 workspace");
			expect(description?.classList.contains("workspace-scope-help")).toBe(true);
			expect(description?.closest(".sr-only")).toBeNull();
			expect(description?.hasAttribute("hidden")).toBe(false);
			expect(description?.getAttribute("aria-hidden")).toBeNull();
			expect(pickers[index].contains(description)).toBe(true);
			return descriptionId!;
		});

		expect(new Set(descriptionIds).size).toBe(descriptionIds.length);
	});

	it("announces complete workspace selection updates outside the disclosure button", async () => {
		function WorkspaceHarness() {
			const [scope, setScope] = React.useState(WORKSPACE_SCOPE);
			return <WorkspaceScopePicker scope={scope} onChange={setScope} open />;
		}
		render(<WorkspaceHarness />);

		const toggle = screen.getByRole("button", { name: /Workspaces/ });
		const status = screen.getByRole("status");
		expect(toggle.querySelector("[aria-live]")).toBeNull();
		expect(toggle.contains(status)).toBe(false);
		expect(status.textContent).toBe("Workspace selection: All 2 workspaces included.");

		await userEvent.click(screen.getByRole("checkbox", { name: /docs/ }));
		expect(status.textContent).toBe("Workspace selection: 1 of 2 workspaces included.");
	});

	it("waits for a selected project's workspace configuration", () => {
		render(setup(
			{ mcpReady: true, mcpAuthStatusReady: true },
			{ status: "loading" },
		));

		expect(screen.getByRole("status").textContent).toBe("Loading workspaces…");
		expect(screen.queryByRole("heading", { name: "Host context only" })).toBeNull();
	});

	it("reports an unavailable selected-project workspace configuration", () => {
		render(setup(
			{ mcpReady: true, mcpAuthStatusReady: true },
			{ status: "unavailable" },
		));

		expect(screen.getByText("Workspaces unavailable")).toBeTruthy();
		expect(screen.getByText("Retry in Projects")).toBeTruthy();
		expect(screen.queryByRole("heading", { name: "Host context only" })).toBeNull();
	});

	it.each([
		{ mcpReady: false, mcpAuthStatusReady: false },
		{ mcpReady: true, mcpAuthStatusReady: false },
		{ mcpReady: false, mcpAuthStatusReady: true },
	])("does not claim optional context is empty before all MCP data is ready", (readiness) => {
		render(setup(readiness));

		expect(screen.getByRole("status").textContent).toBe("Loading MCP tools…");
		expect(screen.queryByRole("heading", { name: "Host context only" })).toBeNull();
	});

	it("reports the connection dependency while MCP readiness is unknown", () => {
		render(setup({
			mcpReady: false,
			mcpAuthStatusReady: false,
			mcpAuthMutationBlockedReason: "Waiting for connection",
		}));

		expect(screen.getByRole("status").textContent).toBe("Waiting for connection");
		expect(screen.queryByRole("heading", { name: "Host context only" })).toBeNull();
	});

	it("shows the definitive host empty state after inventory and auth status are ready", () => {
		render(setup({ mcpReady: true, mcpAuthStatusReady: true }));

		expect(screen.getByRole("heading", { name: "Host context only" })).toBeTruthy();
		expect(screen.queryByText(/Write your first message below/)).toBeNull();
		expect(screen.queryByRole("status")).toBeNull();
	});
});
