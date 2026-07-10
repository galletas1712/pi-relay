// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, describe, expect, it, vi } from "vitest";
import { McpToolPicker } from "./mcpToolPicker.tsx";
import type { McpInventory } from "./types.ts";

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
});
