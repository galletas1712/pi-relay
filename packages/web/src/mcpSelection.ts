import type {
	McpAuthServerStatus,
	McpInventory,
	McpSelection,
	McpServerSelection,
	ProviderConfig,
} from "./types.ts";

export type McpSelectionState = ReadonlyMap<string, ReadonlySet<string>>;
export type McpTriState = "none" | "some" | "all";

export function serverSelectionState(
	inventory: McpInventory,
	selection: McpSelectionState,
	serverId: string,
): McpTriState {
	const server = inventory.servers.find((candidate) => candidate.server === serverId);
	const selected = selection.get(serverId)?.size ?? 0;
	if (!server || selected === 0) return "none";
	return selected === server.tools.length ? "all" : "some";
}

export function clearMcpServerSelection(
	selection: McpSelectionState,
	serverId: string,
	locked: McpSelectionState = new Map(),
): McpSelectionState {
	if (!selection.has(serverId)) return selection;
	const next = new Map(selection);
	const retained = locked.get(serverId);
	if (retained?.size) next.set(serverId, retained);
	else next.delete(serverId);
	return next;
}

export function toggleServer(
	inventory: McpInventory,
	selection: McpSelectionState,
	serverId: string,
	locked: McpSelectionState = new Map(),
): McpSelectionState {
	const server = inventory.servers.find((candidate) => candidate.server === serverId);
	if (!server) return selection;
	if (server.tools.length === 0 && !selection.has(serverId)) return selection;
	const next = new Map(selection);
	if (server.health !== "healthy" || server.tools.length === 0) {
		const retained = locked.get(serverId);
		if (retained?.size) next.set(serverId, retained);
		else next.delete(serverId);
		return next;
	}
	if (serverSelectionState(inventory, selection, serverId) === "all") {
		const retained = locked.get(serverId);
		if (retained?.size) next.set(serverId, retained);
		else next.delete(serverId);
	} else {
		next.set(serverId, new Set(server.tools.map((tool) => tool.raw_name)));
	}
	return next;
}

export function toggleTool(
	selection: McpSelectionState,
	serverId: string,
	rawName: string,
	locked: McpSelectionState = new Map(),
): McpSelectionState {
	if (locked.get(serverId)?.has(rawName)) return selection;
	const next = new Map(selection);
	const tools = new Set(next.get(serverId) ?? []);
	if (tools.has(rawName)) tools.delete(rawName);
	else tools.add(rawName);
	if (tools.size) next.set(serverId, tools);
	else next.delete(serverId);
	return next;
}

export function mcpSelectionForProviderChange(
	currentProvider: ProviderConfig["kind"],
	nextProvider: ProviderConfig["kind"],
	selection: McpSelectionState,
): { selection: McpSelectionState; reset: boolean } {
	if (currentProvider === nextProvider) return { selection, reset: false };
	return { selection: new Map(), reset: mcpSelectedToolCount(selection) > 0 };
}

export function mcpSelectionPayloadForProvider(
	provider: ProviderConfig["kind"],
	selectionProvider: ProviderConfig["kind"],
	inventoryProvider: ProviderConfig["kind"] | null,
	inventory: McpInventory | null,
	inventoryReady: boolean,
	selection: McpSelectionState,
	authStatus: McpAuthServerStatus[],
	authStatusReady: boolean,
): McpSelection | undefined {
	if (mcpSelectedToolCount(selection) === 0) return undefined;
	if (
		!inventoryReady ||
		selectionProvider !== provider ||
		inventoryProvider !== provider ||
		!inventory
	) {
		throw new Error("MCP inventory for the selected provider is still loading; review the MCP selection before sending");
	}
	if (!authStatusReady) {
		throw new Error("MCP authorization status is still loading; review the MCP selection before sending");
	}
	const authByServer = new Map(authStatus.map((status) => [status.server, status]));
	for (const server of selection.keys()) {
		const auth = authByServer.get(server);
		if (!auth || (auth.auth_kind === "oauth" && auth.auth_state !== "ready")) {
			throw new Error(`MCP server ${server} is not authorized; review the MCP selection before sending`);
		}
	}
	return mcpSelectionPayload(inventory, selection);
}

export function mcpSelectedToolCount(selection: McpSelectionState): number {
	let count = 0;
	for (const tools of selection.values()) count += tools.size;
	return count;
}

export function reconcileMcpSelection(
	previousInventory: McpInventory | null,
	nextInventory: McpInventory,
	selection: McpSelectionState,
): McpSelectionState {
	if (!previousInventory) return selection;
	const priorRevisions = new Map(previousInventory.servers.map((server) => [server.server, server.revision]));
	const next = new Map<string, ReadonlySet<string>>();
	for (const server of nextInventory.servers) {
		if (priorRevisions.get(server.server) !== server.revision) continue;
		const allowed = new Set(server.tools.map((tool) => tool.raw_name));
		const retained = new Set([...(selection.get(server.server) ?? [])].filter((tool) => allowed.has(tool)));
		if (retained.size) next.set(server.server, retained);
	}
	return next;
}

export function mcpSelectionTotals(
	inventory: McpInventory,
	selection: McpSelectionState,
): { tools: number; contextTokens: number } {
	let tools = 0;
	let contextTokens = 0;
	for (const server of inventory.servers) {
		const selected = selection.get(server.server);
		if (!selected) continue;
		for (const tool of server.tools) {
			if (!selected.has(tool.raw_name)) continue;
			tools += 1;
			contextTokens += tool.context_token_estimate;
		}
	}
	return { tools, contextTokens };
}

export function mcpSelectionPayload(
	inventory: McpInventory,
	selection: McpSelectionState,
	exclude: McpSelectionState = new Map(),
): McpSelection | undefined {
	const servers = [...selection]
		.map(([server, tools]) => ({
			server,
			tools: [...tools]
				.filter((tool) => !exclude.get(server)?.has(tool))
				.sort(),
		}))
		.filter((server) => server.tools.length)
		.sort((left, right) => (left.server < right.server ? -1 : left.server > right.server ? 1 : 0));
	return servers.length ? { inventoryRevision: inventory.revision, servers } : undefined;
}

export function mcpSelectionFromServers(
	servers: readonly McpServerSelection[] = [],
): McpSelectionState {
	return new Map(
		servers
			.filter((server) => server.tools.length)
			.map((server) => [server.server, new Set(server.tools)]),
	);
}

export function inventoryWithSelection(
	inventory: McpInventory,
	selection: McpSelectionState,
): McpInventory {
	const byServer = new Map(inventory.servers.map((server) => [server.server, {
		...server,
		tools: [...server.tools],
	}]));
	for (const [serverId, selectedTools] of selection) {
		const server = byServer.get(serverId) ?? {
			server: serverId,
			revision: "",
			health: "unavailable" as const,
			tools: [],
		};
		const known = new Set(server.tools.map((tool) => tool.raw_name));
		for (const rawName of selectedTools) {
			if (known.has(rawName)) continue;
			server.tools.push({
				raw_name: rawName,
				description: "Selected for this session",
				context_token_estimate: 0,
			});
		}
		server.tools.sort((left, right) => left.raw_name.localeCompare(right.raw_name));
		byServer.set(serverId, server);
	}
	return { ...inventory, servers: [...byServer.values()].sort((left, right) => left.server.localeCompare(right.server)) };
}
