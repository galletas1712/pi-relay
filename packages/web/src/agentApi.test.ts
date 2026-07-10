import { describe, expect, it } from "vitest";
import { createAgentApi } from "./agentApi.ts";
import {
	SESSION_START_REQUEST_TIMEOUT_MS,
	type RpcClient,
	type RpcRequestOptions,
} from "./rpc.ts";

type RpcCall = {
	method: string;
	params?: Record<string, unknown>;
	options?: RpcRequestOptions;
};

describe("AgentApi MCP wire format", () => {
	it("requests inventory and serializes sorted raw session selection", async () => {
		const calls: RpcCall[] = [];
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
			options: { timeoutMs: SESSION_START_REQUEST_TIMEOUT_MS },
		});
	});

	it("maps the sanitized MCP OAuth lifecycle without browser storage", async () => {
		const calls: RpcCall[] = [];
		const storageSet = globalThis.localStorage?.setItem;
		const api = createAgentApi(fakeClient(calls));
		await api.getMcpStatus();
		await api.loginMcp("remote");
		await api.completeMcpLogin(
			"remote",
			"0000000000000001",
			"http://127.0.0.1:43123/oauth/callback/0000000000000001?code=code&state=state",
		);
		await api.cancelMcpLogin("remote", "0000000000000001");
		await api.logoutMcp("remote");
		expect(calls).toEqual([
			{ method: "mcp.status", params: undefined },
			{ method: "mcp.login", params: { server: "remote" } },
			{
				method: "mcp.complete",
				params: {
					server: "remote",
					login_id: "0000000000000001",
					callback_url:
						"http://127.0.0.1:43123/oauth/callback/0000000000000001?code=code&state=state",
				},
			},
			{
				method: "mcp.cancel",
				params: { server: "remote", login_id: "0000000000000001" },
			},
			{ method: "mcp.logout", params: { server: "remote" } },
		]);
		expect(globalThis.localStorage?.setItem).toBe(storageSet);
	});

	it("omits MCP for an MCP-free session", async () => {
		const calls: RpcCall[] = [];
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
		const calls: RpcCall[] = [];
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

function fakeClient(calls: RpcCall[]): RpcClient {
	return {
		connect: async () => {},
		reconnect: async () => {},
		close: () => {},
		isOpen: () => true,
		onEvent: () => () => {},
		onStatus: () => () => {},
		request: async <T>(
			method: string,
			params?: Record<string, unknown>,
			options?: RpcRequestOptions,
		) => {
			calls.push({ method, params, ...(options ? { options } : {}) });
			if (method === "mcp.inventory") return { revision: "inventory-1", servers: [] } as T;
			if (method === "mcp.status") return { servers: [] } as T;
			if (method === "mcp.login") {
				return {
					login_id: "0000000000000001",
					authorization_url: "https://auth.example.test/authorize",
					expires_at_unix_seconds: 1,
				} as T;
			}
			if (method === "mcp.logout") return { result: "removed" } as T;
			return { session_id: "session-1", activity: "running" } as T;
		},
	};
}
