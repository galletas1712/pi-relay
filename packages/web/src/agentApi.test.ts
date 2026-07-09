import { describe, expect, it } from "vitest";
import { createAgentApi } from "./agentApi.ts";
import type { RpcClient } from "./rpc.ts";

describe("AgentApi MCP wire format", () => {
	it("requests inventory and serializes sorted raw session selection", async () => {
		const calls: { method: string; params?: Record<string, unknown> }[] = [];
		const client = fakeClient(calls);
		const api = createAgentApi(client);
		await api.getMcpInventory("openai");
		await api.startSession({
			sessionId: "session-1",
			provider: { kind: "openai", model: "gpt-test" },
			metadata: {},
			clientInputId: "input-1",
			priority: "follow_up",
			content: [{ type: "text", text: "hello" }],
			projectId: "project-1",
			workspaces: [{ workspaceDir: "repo-a", branch: " feature/login " }],
			mcp: {
				inventoryRevision: "inventory-1",
				servers: [
					{ server: "workspace", tools: ["search", "read"] },
					{ server: "archive", tools: ["write"] },
				],
			},
		});
		expect(calls[0]).toEqual({ method: "mcp.inventory", params: { provider: "openai" } });
		expect(calls[1]).toEqual({
			method: "session.start",
			params: {
				session_id: "session-1",
				project_id: "project-1",
				provider: { kind: "openai", model: "gpt-test" },
				metadata: {},
				client_input_id: "input-1",
				priority: "follow_up",
				content: [{ type: "text", text: "hello" }],
				workspaces: [{ workspace_dir: "repo-a", branch: "feature/login" }],
				mcp: {
					inventory_revision: "inventory-1",
					servers: [
						{ server: "archive", tools: ["write"] },
						{ server: "workspace", tools: ["read", "search"] },
					],
				},
			},
		});
	});

	it("omits MCP for an MCP-free session", async () => {
		const calls: { method: string; params?: Record<string, unknown> }[] = [];
		await createAgentApi(fakeClient(calls)).startSession({
			sessionId: "session-1",
			provider: { kind: "openai", model: "gpt-test" },
			metadata: {},
			clientInputId: "input-1",
			priority: "follow_up",
			content: [{ type: "text", text: "hello" }],
		});
		expect(calls[0]?.params).not.toHaveProperty("mcp");
	});

	it("preserves an explicit empty workspace subset for daemon validation", async () => {
		const calls: { method: string; params?: Record<string, unknown> }[] = [];
		await createAgentApi(fakeClient(calls)).startSession({
			sessionId: "session-1",
			projectId: "project-1",
			provider: { kind: "openai", model: "gpt-test" },
			metadata: {},
			clientInputId: "input-1",
			priority: "follow_up",
			content: [{ type: "text", text: "hello" }],
			workspaces: [],
		});
		expect(calls[0]?.params?.workspaces).toEqual([]);
	});
});

function fakeClient(calls: { method: string; params?: Record<string, unknown> }[]): RpcClient {
	return {
		connect: async () => {},
		reconnect: async () => {},
		close: () => {},
		isOpen: () => true,
		onEvent: () => () => {},
		onStatus: () => () => {},
		request: async <T>(method: string, params?: Record<string, unknown>) => {
			calls.push({ method, params });
			if (method === "mcp.inventory") return { revision: "inventory-1", servers: [] } as T;
			return { session_id: "session-1", activity: "running" } as T;
		},
	};
}
