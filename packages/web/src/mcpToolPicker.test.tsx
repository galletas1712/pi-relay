// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { McpToolPicker } from "./mcpToolPicker.tsx";
import type { McpAuthServerStatus, McpInventory } from "./types.ts";

afterEach(cleanup);

const INVENTORY: McpInventory = {
	revision: "inventory-1",
	servers: [{
		server: "workspace",
		revision: "workspace-1",
		health: "healthy",
		tools: [
			{ raw_name: "read", description: "Read a file", context_token_estimate: 12 },
			{ raw_name: "write", description: "Write a file", context_token_estimate: 18 },
		],
	}],
};

function oauthStatus(
	authState: McpAuthServerStatus["auth_state"],
	patch: Partial<McpAuthServerStatus> = {},
): McpAuthServerStatus {
	return {
		server: "oauth",
		auth_kind: "oauth",
		auth_state: authState,
		can_login:
			authState === "login_required" ||
			authState === "reauthentication_required",
		can_logout: authState === "ready" || authState === "authorization_pending",
		...patch,
	};
}

describe("McpToolPicker", () => {
	it("omits markup when there are no configured servers", () => {
		expect(renderToStaticMarkup(
			<McpToolPicker inventory={{ revision: "empty", servers: [] }} selection={new Map()} onChange={() => {}} />,
		)).toBe("");
	});

	it("omits closed inventory controls while reporting aria-expanded false", () => {
		const markup = renderToStaticMarkup(
			<McpToolPicker
				inventory={INVENTORY}
				selection={new Map([["workspace", new Set(["read"])]])}
				onChange={() => {}}
			/>,
		);
		expect(markup).toContain("1 selected");
		expect(markup).toContain("MCP context tokens added");
		expect(markup).toContain('aria-expanded="false"');
		expect(markup).not.toContain("mcp-picker-list");
		expect(markup).not.toContain('aria-checked="mixed"');
		expect(markup).not.toContain("workspace");
		expect(markup).toContain("All full and read-only subagents inherit these tools");
		expect(markup).toContain("Read-only restricts local files only");
		expect(markup).toContain("MCP tools may cause remote side effects");
	});

	it("summarizes a selected unavailable server without rendering closed controls", () => {
		const markup = renderToStaticMarkup(
			<McpToolPicker
				inventory={{
					...INVENTORY,
					servers: [{ ...INVENTORY.servers[0], health: "unavailable" }],
				}}
				selection={new Map([["workspace", new Set(["read"])]])}
				onChange={() => {}}
			/>,
		);
		expect(markup).toContain("1 selected");
		expect(markup).toContain("MCP tools may cause remote side effects");
		expect(markup).not.toContain("mcp-picker-list");
	});

	it("omits selection controls and warnings for a healthy zero-tool server", () => {
		const markup = renderToStaticMarkup(
			<McpToolPicker
				inventory={{
					revision: "empty",
					servers: [{
						server: "empty",
						revision: "empty-1",
						health: "healthy",
						tools: [],
					}],
				}}
				selection={new Map([["empty", new Set()]])}
				onChange={() => {}}
				open
			/>,
		);
		expect(markup).toContain("empty");
		expect(markup).toContain("0/0 tools");
		expect(markup).not.toContain('type="checkbox"');
		expect(markup).not.toContain("MCP tools may cause remote side effects");
	});

	it("cannot interactively select a healthy zero-tool server", async () => {
		const onChange = vi.fn();
		render(
			<McpToolPicker
				inventory={{
					revision: "empty",
					servers: [{
						server: "empty",
						revision: "empty-1",
						health: "healthy",
						tools: [],
					}],
				}}
				selection={new Map()}
				onChange={onChange}
				open
			/>,
		);
		expect(screen.queryByRole("checkbox")).toBeNull();
		await userEvent.click(screen.getByRole("button", { name: "expand empty tools" }));
		expect(screen.queryByRole("checkbox")).toBeNull();
		expect(onChange).not.toHaveBeenCalled();
	});

	it("keeps a login-required OAuth server visible without inventory and starts login", async () => {
		const onLogin = vi.fn();
		render(
			<McpToolPicker
				inventory={{ revision: "", servers: [] }}
				selection={new Map()}
				onChange={() => {}}
				authStatus={[oauthStatus("login_required")]}
				onLogin={onLogin}
				open
			/>,
		);
		expect(screen.getByText("oauth")).toBeTruthy();
		expect(screen.getByText("login required")).toBeTruthy();
		expect(screen.queryByRole("checkbox")).toBeNull();
		await userEvent.click(screen.getByRole("button", { name: "Login" }));
		expect(onLogin).toHaveBeenCalledWith("oauth");
	});

	it("shows ready, pending, reauthentication, unsupported, and unknown OAuth states", () => {
		render(
			<McpToolPicker
				inventory={{ revision: "", servers: [] }}
				selection={new Map()}
				onChange={() => {}}
				authStatus={[
					oauthStatus("ready", { server: "ready" }),
					oauthStatus("authorization_pending", { server: "pending" }),
					oauthStatus("reauthentication_required", { server: "reauth" }),
					oauthStatus("unsupported", {
						server: "unsupported",
						can_login: false,
						can_logout: false,
					}),
					oauthStatus("unknown", {
						server: "unknown",
						can_login: false,
						can_logout: false,
						failure: "discovery_failed",
					}),
				]}
				open
			/>,
		);
		expect(screen.getByText("OAuth ready")).toBeTruthy();
		expect(screen.getByText("login pending")).toBeTruthy();
		expect(screen.getByText("login expired")).toBeTruthy();
		expect(screen.getByText("OAuth unsupported")).toBeTruthy();
		expect(screen.getByText(/OAuth unknown · OAuth discovery failed/)).toBeTruthy();
		expect(screen.getByText(/cancel it and start again/)).toBeTruthy();
	});

	it("renders daemon-advertised actions, including both recovery actions", () => {
		render(
			<McpToolPicker
				inventory={{ revision: "", servers: [] }}
				selection={new Map()}
				onChange={() => {}}
				authStatus={[
					oauthStatus("ready", { server: "oauth" }),
					oauthStatus("reauthentication_required", {
						server: "reauth",
						can_login: true,
						can_logout: true,
					}),
					oauthStatus("unknown", {
						server: "recoverable",
						can_login: true,
						can_logout: false,
					}),
					{
						server: "bearer",
						auth_kind: "bearer",
						auth_state: "not_applicable",
						can_login: false,
						can_logout: false,
					},
					{
						server: "plain",
						auth_kind: "none",
						auth_state: "not_applicable",
						can_login: false,
						can_logout: false,
					},
				]}
				open
			/>,
		);
		expect(screen.getAllByRole("button", { name: "Logout" })).toHaveLength(2);
		expect(screen.getAllByRole("button", { name: "Login" })).toHaveLength(2);
		expect(screen.getAllByText("bearer")).toHaveLength(2);
		expect(screen.getByText("no auth")).toBeTruthy();
	});

	it("confirms logout when the server has selected draft tools", async () => {
		const onLogout = vi.fn();
		const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
		render(
			<McpToolPicker
				inventory={{
					...INVENTORY,
					servers: [{ ...INVENTORY.servers[0], server: "oauth" }],
				}}
				selection={new Map([["oauth", new Set(["read"])]])}
				onChange={() => {}}
				authStatus={[oauthStatus("ready")]}
				onLogout={onLogout}
				open
			/>,
		);
		await userEvent.click(screen.getByRole("button", { name: "Logout" }));
		expect(confirm).toHaveBeenCalled();
		expect(onLogout).not.toHaveBeenCalled();
		confirm.mockReturnValue(true);
		await userEvent.click(screen.getByRole("button", { name: "Logout" }));
		expect(onLogout).toHaveBeenCalledWith("oauth");
		confirm.mockRestore();
	});

	it("fails closed before status, on status failure, and after reauthentication while allowing removal", async () => {
		const onChange = vi.fn();
		const selected = new Map([["oauth", new Set(["read"])]]);
		const inventory = {
			...INVENTORY,
			servers: [{ ...INVENTORY.servers[0], server: "oauth" }],
		};
		const { rerender } = render(
			<McpToolPicker
				inventory={inventory}
				selection={new Map()}
				onChange={onChange}
				authStatus={[]}
				authStatusReady={false}
				open
			/>,
		);
		expect(screen.getByRole<HTMLInputElement>("checkbox", { name: "oauth" }).disabled).toBe(true);

		rerender(
			<McpToolPicker
				inventory={inventory}
				selection={selected}
				onChange={onChange}
				authStatus={[oauthStatus("ready")]}
				authStatusReady={false}
				open
			/>,
		);
		expect(screen.getByRole<HTMLInputElement>("checkbox", { name: "oauth" }).disabled).toBe(false);
		await userEvent.click(screen.getByRole("checkbox", { name: "oauth" }));
		expect(onChange).toHaveBeenLastCalledWith(new Map());

		rerender(
			<McpToolPicker
				inventory={inventory}
				selection={selected}
				onChange={onChange}
				authStatus={[oauthStatus("reauthentication_required")]}
				authStatusReady
				open
			/>,
		);
		await userEvent.click(screen.getByRole("button", { name: "expand oauth tools" }));
		expect(screen.getByRole<HTMLInputElement>("checkbox", { name: /^read/i }).disabled).toBe(false);
		expect(screen.getByRole<HTMLInputElement>("checkbox", { name: /^write/i }).disabled).toBe(true);
	});

	it("confirms pending cleanup and disables auth mutations while offline", async () => {
		const onLogout = vi.fn();
		const confirm = vi.spyOn(window, "confirm").mockReturnValue(false);
		render(
			<McpToolPicker
				inventory={{
					...INVENTORY,
					servers: [{ ...INVENTORY.servers[0], server: "oauth" }],
				}}
				selection={new Map([["oauth", new Set(["read"])]])}
				onChange={() => {}}
				authStatus={[oauthStatus("authorization_pending")]}
				onLogout={onLogout}
				open
			/>,
		);
		await userEvent.click(screen.getByRole("button", { name: "Cancel" }));
		expect(confirm).toHaveBeenCalled();
		expect(onLogout).not.toHaveBeenCalled();
		cleanup();

		render(
			<McpToolPicker
				inventory={{ revision: "", servers: [] }}
				selection={new Map()}
				onChange={() => {}}
				authStatus={[oauthStatus("login_required")]}
				onLogin={() => {}}
				authMutationBlockedReason="Waiting for connection"
				open
			/>,
		);
		const login = screen.getByRole<HTMLButtonElement>("button", { name: "Login" });
		expect(login.disabled).toBe(true);
		expect(login.title).toBe("Waiting for connection");
	});
});
