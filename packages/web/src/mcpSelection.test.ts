import { describe, expect, it } from "vitest";
import {
	clearMcpServerSelection,
	mcpSelectionPayload,
	mcpSelectionForProviderChange,
	mcpSelectionPayloadForProvider,
	mcpSelectedToolCount,
	mcpSelectionTotals,
	reconcileMcpSelection,
	serverSelectionState,
	toggleServer,
	toggleTool,
} from "./mcpSelection.ts";
import type { McpAuthServerStatus, McpInventory } from "./types.ts";

const INVENTORY: McpInventory = {
	revision: "inventory-1",
	servers: [
		{
			server: "zeta",
			revision: "zeta-1",
			health: "healthy",
			tools: [
				{ raw_name: "write", description: "Write", context_token_estimate: 20 },
				{ raw_name: "read", description: "Read", context_token_estimate: 10 },
			],
		},
		{
			server: "alpha",
			revision: "alpha-1",
			health: "healthy",
			tools: [{ raw_name: "search", description: "Search", context_token_estimate: 30 }],
		},
	],
};
const AUTH_STATUS: McpAuthServerStatus[] = INVENTORY.servers.map((server) => ({
	server: server.server,
	auth_kind: "none",
	auth_state: "not_applicable",
	can_login: false,
	can_logout: false,
}));

describe("MCP selection", () => {
	it("selects a whole server by default and tracks tri-state tool changes", () => {
		const all = toggleServer(INVENTORY, new Map(), "zeta");
		expect(serverSelectionState(INVENTORY, all, "zeta")).toBe("all");
		expect(all.get("zeta")).toEqual(new Set(["write", "read"]));

		const some = toggleTool(all, "zeta", "read");
		expect(serverSelectionState(INVENTORY, some, "zeta")).toBe("some");
		expect(toggleTool(some, "zeta", "write")).toEqual(new Map());
	});

	it("clears only the logged-out server's selected draft tools", () => {
		const selected = new Map([
			["zeta", new Set(["read"])],
			["alpha", new Set(["search"])],
		]);
		expect(clearMcpServerSelection(selected, "zeta")).toEqual(
			new Map([["alpha", new Set(["search"])]]),
		);
		expect(clearMcpServerSelection(selected, "missing")).toBe(selected);
	});

	it("emits sorted raw identities and omits an empty selection", () => {
		const selected = new Map([
			["zeta", new Set(["write", "read"])],
			["alpha", new Set(["search"])],
		]);
		expect(mcpSelectionPayload(INVENTORY, selected)).toEqual({
			inventoryRevision: "inventory-1",
			servers: [
				{ server: "alpha", tools: ["search"] },
				{ server: "zeta", tools: ["read", "write"] },
			],
		});
		expect(mcpSelectionPayload(INVENTORY, new Map())).toBeUndefined();
		expect(mcpSelectionTotals(INVENTORY, selected)).toEqual({ tools: 3, contextTokens: 60 });
	});

	it("clears a changed server revision without selecting new contracts", () => {
		const selected = new Map([
			["zeta", new Set(["read"])],
			["alpha", new Set(["search"])],
		]);
		const refreshed: McpInventory = {
			revision: "inventory-2",
			servers: [
				{ ...INVENTORY.servers[0], revision: "zeta-2", tools: [...INVENTORY.servers[0].tools, { raw_name: "new", description: "New", context_token_estimate: 5 }] },
				INVENTORY.servers[1],
			],
		};
		expect(reconcileMcpSelection(INVENTORY, refreshed, selected)).toEqual(
			new Map([["alpha", new Set(["search"])]]),
		);
	});

	it("removes retained unavailable selections without allowing new unhealthy tools", () => {
		const unavailable: McpInventory = {
			...INVENTORY,
			servers: INVENTORY.servers.map((server) =>
				server.server === "zeta" ? { ...server, health: "unavailable" } : server
			),
		};
		const retained = reconcileMcpSelection(
			INVENTORY,
			unavailable,
			new Map([["zeta", new Set(["read", "write"])]]),
		);
		expect(toggleTool(retained, "zeta", "read")).toEqual(
			new Map([["zeta", new Set(["write"])]]),
		);
		expect(toggleServer(unavailable, retained, "zeta")).toEqual(new Map());
		expect(toggleServer(unavailable, new Map(), "zeta")).toEqual(new Map());
	});

	it("clears selection across providers and fails closed for a pending provider inventory", () => {
		const selected = new Map([["zeta", new Set(["read"])]]);
		expect(mcpSelectionForProviderChange("openai", "claude", selected)).toEqual({
			selection: new Map(),
			reset: true,
		});
		expect(() =>
			mcpSelectionPayloadForProvider(
				"claude",
				"openai",
				null,
				null,
				false,
				selected,
				AUTH_STATUS,
				true,
			)
		).toThrow("MCP inventory for the selected provider is still loading");
		expect(
			mcpSelectionPayloadForProvider(
				"claude",
				"claude",
				"claude",
				INVENTORY,
				true,
				selected,
				AUTH_STATUS,
				true,
			)
		).toEqual({
			inventoryRevision: INVENTORY.revision,
			servers: [{ server: "zeta", tools: ["read"] }],
		});
	});

	it("fails closed while retained inventory is fetching or errored", () => {
		const selected = new Map([["zeta", new Set(["read"])]]);
		expect(() =>
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				INVENTORY,
				false,
				selected,
				AUTH_STATUS,
				true,
			)
		).toThrow("MCP inventory for the selected provider is still loading");
		expect(
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				INVENTORY,
				false,
				new Map(),
				[],
				false,
			)
		).toBeUndefined();
	});

	it("fails closed for missing, loading, and non-ready OAuth status", () => {
		const selected = new Map([["zeta", new Set(["read"])]]);
		const oauth = {
			...AUTH_STATUS[0],
			auth_kind: "oauth" as const,
			auth_state: "ready" as const,
		};
		expect(() =>
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				INVENTORY,
				true,
				selected,
				[],
				true,
			)
		).toThrow("not authorized");
		expect(() =>
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				INVENTORY,
				true,
				selected,
				[oauth],
				false,
			)
		).toThrow("authorization status is still loading");
		expect(() =>
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				INVENTORY,
				true,
				selected,
				[{ ...oauth, auth_state: "reauthentication_required" }],
				true,
			)
		).toThrow("not authorized");
		expect(
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				INVENTORY,
				true,
				selected,
				[oauth, AUTH_STATUS[1]],
				true,
			),
		).toEqual({
			inventoryRevision: INVENTORY.revision,
			servers: [{ server: "zeta", tools: ["read"] }],
		});
	});

	it("never creates a phantom selection for a healthy zero-tool server", () => {
		const emptyInventory: McpInventory = {
			revision: "empty",
			servers: [{
				server: "empty",
				revision: "empty-1",
				health: "healthy",
				tools: [],
			}],
		};
		const phantom = new Map([["empty", new Set<string>()]]);
		expect(toggleServer(emptyInventory, new Map(), "empty")).toEqual(new Map());
		expect(toggleServer(emptyInventory, phantom, "empty")).toEqual(new Map());
		expect(mcpSelectedToolCount(phantom)).toBe(0);
		expect(
			mcpSelectionPayloadForProvider(
				"openai",
				"openai",
				"openai",
				emptyInventory,
				false,
				phantom,
				[],
				false,
			)
		).toBeUndefined();
	});
});
