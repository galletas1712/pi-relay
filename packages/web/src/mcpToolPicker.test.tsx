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

	it("omits closed inventory controls and unresolved ID references while reporting aria-expanded false", () => {
		const markup = renderToStaticMarkup(
			<McpToolPicker
				inventory={INVENTORY}
				selection={new Map([["workspace", new Set(["read"])]])}
				onChange={() => {}}
			/>,
		);
		expect(markup).toContain("1 tool selected");
		expect(markup).toContain("Adds about 12 context tokens");
		expect(markup).toContain('aria-expanded="false"');
		expect(markup).not.toContain("aria-controls");
		expect(markup).not.toContain("mcp-picker-list");
		expect(markup).not.toContain('aria-checked="mixed"');
		expect(markup).not.toContain("workspace");
		expect(markup).toContain("Every full and read-only subagent inherits these tools");
		expect(markup).toContain("Read-only limits local files only");
		expect(markup).toContain("MCP tools can affect remote systems");
		expect(markup).not.toContain("·");
		expect(markup).not.toContain("≈");
	});

	it("uses instance-safe picker and server panel IDs that always resolve when present", async () => {
		const { container } = render(
			<>
				<McpToolPicker inventory={INVENTORY} selection={new Map()} onChange={() => {}} />
				<McpToolPicker inventory={INVENTORY} selection={new Map()} onChange={() => {}} />
			</>,
		);
		const pickerToggles = screen.getAllByRole("button", { name: /MCP tools/ });

		expectPresentAriaControlsResolve(container);
		expect(container.querySelectorAll("[aria-controls]")).toHaveLength(0);

		await userEvent.click(pickerToggles[0]);
		expect(expectPresentAriaControlsResolve(container)).toHaveLength(1);
		expect(screen.getByRole("button", { name: "expand workspace tools" }).hasAttribute("aria-controls"))
			.toBe(false);

		await userEvent.click(pickerToggles[1]);
		const pickerIds = expectPresentAriaControlsResolve(container);
		expect(pickerIds).toHaveLength(2);
		expect(new Set(pickerIds).size).toBe(pickerIds.length);

		const expanders = screen.getAllByRole("button", { name: "expand workspace tools" });
		await userEvent.click(expanders[0]);
		await userEvent.click(expanders[1]);
		const allIds = expectPresentAriaControlsResolve(container);
		expect(allIds).toHaveLength(4);
		expect(new Set(allIds).size).toBe(allIds.length);

		await userEvent.click(screen.getAllByRole("button", { name: "collapse workspace tools" })[0]);
		expectPresentAriaControlsResolve(container);
		expect(container.querySelectorAll("[aria-controls]")).toHaveLength(3);
	});

	it("announces complete MCP selection updates outside the disclosure button", () => {
		const { rerender } = render(
			<McpToolPicker inventory={INVENTORY} selection={new Map()} onChange={() => {}} />,
		);
		const toggle = screen.getByRole("button", { name: /MCP tools/ });
		const status = screen.getByRole("status");
		expect(toggle.querySelector("[aria-live]")).toBeNull();
		expect(toggle.contains(status)).toBe(false);
		expect(status.textContent).toBe("MCP tool selection: No remote tools will be added.");

		rerender(
			<McpToolPicker
				inventory={INVENTORY}
				selection={new Map([["workspace", new Set(["read"])]])}
				onChange={() => {}}
			/>,
		);
		expect(screen.getByRole("status").textContent).toBe(
			"MCP tool selection: 1 tool selected. Adds about 12 context tokens.",
		);
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
		expect(markup).toContain("1 tool selected");
		expect(markup).toContain("MCP tools can affect remote systems");
		expect(markup).not.toContain("mcp-picker-list");
	});

	it("uses plain-language summaries and omits selection controls for a healthy zero-tool server", () => {
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
		expect(markup).toContain("No remote tools will be added");
		expect(markup).toContain("No tools available");
		expect(markup).not.toContain("0 selected");
		expect(markup).not.toContain("0/0");
		expect(markup).not.toContain("·");
		expect(markup).not.toContain("≈");
		expect(markup).not.toContain('type="checkbox"');
		expect(markup).not.toContain("MCP tools can affect remote systems");
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
		expect(screen.queryByRole("button", { name: /empty tools/ })).toBeNull();
		expect(screen.queryByRole("checkbox")).toBeNull();
		expect(onChange).not.toHaveBeenCalled();
	});

	it("uses natural zero, one, and many server summaries with singular context tokens", () => {
		const inventory: McpInventory = {
			revision: "summary",
			servers: [
				{ server: "empty", revision: "empty", health: "healthy", tools: [] },
				{
					server: "single",
					revision: "single",
					health: "healthy",
					tools: [{ raw_name: "one", description: "One", context_token_estimate: 1 }],
				},
				{
					server: "many",
					revision: "many",
					health: "healthy",
					tools: [
						{ raw_name: "first", description: "First", context_token_estimate: 2 },
						{ raw_name: "second", description: "Second", context_token_estimate: 3 },
					],
				},
			],
		};
		render(
			<McpToolPicker
				inventory={inventory}
				selection={new Map([
					["single", new Set(["one"])],
					["many", new Set(["first", "second"])],
				])}
				onChange={() => {}}
				open
			/>,
		);

		expect(screen.getByText("No tools available")).toBeTruthy();
		expect(screen.getByText("1 tool selected")).toBeTruthy();
		expect(screen.queryByText("All 1 tool selected")).toBeNull();
		expect(screen.getByText("All 2 tools selected")).toBeTruthy();
		expect(screen.getAllByText("Adds about 1 context token").length).toBeGreaterThan(0);
		expect(screen.getByRole("status").textContent).toBe(
			"MCP tool selection: 3 tools selected. Adds about 6 context tokens.",
		);
	});

	it("uses the same singular and plural context-token grammar everywhere", async () => {
		const inventory: McpInventory = {
			revision: "tokens",
			servers: [{
				server: "tokens",
				revision: "tokens",
				health: "healthy",
				tools: [
					{ raw_name: "singular", description: "Singular", context_token_estimate: 1 },
					{ raw_name: "plural", description: "Plural", context_token_estimate: 2 },
				],
			}],
		};
		render(
			<McpToolPicker
				inventory={inventory}
				selection={new Map([["tokens", new Set(["singular"])]])}
				onChange={() => {}}
				open
			/>,
		);

		expect(screen.getAllByText("Adds about 1 context token")).toHaveLength(2);
		expect(screen.getByRole("status").textContent).toBe(
			"MCP tool selection: 1 tool selected. Adds about 1 context token.",
		);
		await userEvent.click(screen.getByRole("button", { name: "expand tokens tools" }));
		expect(screen.getByText("About 1 context token")).toBeTruthy();
		expect(screen.getByText("About 2 context tokens")).toBeTruthy();
		expect(screen.queryByText(/1 context tokens/)).toBeNull();
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
		expect(screen.getByText("OAuth unknown")).toBeTruthy();
		expect(screen.getByText("OAuth discovery failed")).toBeTruthy();
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
