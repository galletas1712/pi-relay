// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import {
	NewSessionSetup,
	type WorkspaceConfiguration,
} from "./newSessionSetup.tsx";

afterEach(cleanup);

const EMPTY_INVENTORY = { revision: "empty", servers: [] };

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
	it("waits for a selected project's workspace configuration", () => {
		render(setup(
			{ mcpReady: true, mcpAuthStatusReady: true },
			{ status: "loading" },
		));

		expect(screen.getByRole("status").textContent).toBe("Loading project workspaces…");
		expect(screen.queryByRole("heading", { name: "No optional context configured" })).toBeNull();
	});

	it("reports an unavailable selected-project workspace configuration", () => {
		render(setup(
			{ mcpReady: true, mcpAuthStatusReady: true },
			{ status: "unavailable" },
		));

		expect(screen.getByText("Workspace configuration unavailable. Retry from the Projects panel.")).toBeTruthy();
		expect(screen.queryByRole("heading", { name: "No optional context configured" })).toBeNull();
	});

	it.each([
		{ mcpReady: false, mcpAuthStatusReady: false },
		{ mcpReady: true, mcpAuthStatusReady: false },
		{ mcpReady: false, mcpAuthStatusReady: true },
	])("does not claim optional context is empty before all MCP data is ready", (readiness) => {
		render(setup(readiness));

		expect(screen.getByRole("status").textContent).toBe("Waiting for MCP configuration…");
		expect(screen.queryByRole("heading", { name: "No optional context configured" })).toBeNull();
	});

	it("reports the connection dependency while MCP readiness is unknown", () => {
		render(setup({
			mcpReady: false,
			mcpAuthStatusReady: false,
			mcpAuthMutationBlockedReason: "Waiting for connection",
		}));

		expect(screen.getByRole("status").textContent).toBe("Waiting for connection");
		expect(screen.queryByRole("heading", { name: "No optional context configured" })).toBeNull();
	});

	it("shows the definitive host empty state after inventory and auth status are ready", () => {
		render(setup({ mcpReady: true, mcpAuthStatusReady: true }));

		expect(screen.getByRole("heading", { name: "No optional context configured" })).toBeTruthy();
		expect(screen.queryByRole("status")).toBeNull();
	});
});
