import { describe, expect, it } from "vitest";
import { createAgentApi } from "./agentApi.ts";
import {
	WORKSPACE_OPERATION_REQUEST_TIMEOUT_MS,
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
		await api.getMcpInventory("openai", "runtime-local");
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

		expect(calls[0]).toEqual({
			method: "mcp.inventory",
			params: { provider: "openai", runtime_id: "runtime-local" },
		});
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
			options: {
				timeoutMs: WORKSPACE_OPERATION_REQUEST_TIMEOUT_MS,
			},
		});
	});

	it("returns session-aware inventory and sends additions through the shared selection shape", async () => {
		const calls: RpcCall[] = [];
		const api = createAgentApi(fakeClient(calls));

		await api.getMcpInventory("openai", "runtime-local", "session-1");
		await api.addMcpTools({
			sessionId: "session-1",
			sessionRevision: 7,
			selection: {
				inventoryRevision: "inventory-2",
				servers: [
					{ server: "workspace", tools: ["write", "search"] },
					{ server: "archive", tools: ["zeta", "alpha"] },
				],
			},
		});

		expect(calls).toEqual([
			{
				method: "mcp.inventory",
				params: {
					provider: "openai",
					runtime_id: "runtime-local",
					session_id: "session-1",
				},
			},
			{
				method: "mcp.add",
				params: {
					session_id: "session-1",
					session_revision: 7,
					inventory_revision: "inventory-2",
					servers: [
						{ server: "archive", tools: ["alpha", "zeta"] },
						{ server: "workspace", tools: ["search", "write"] },
					],
				},
				options: { timeoutMs: WORKSPACE_OPERATION_REQUEST_TIMEOUT_MS },
			},
		]);
	});

	it("maps the sanitized MCP OAuth lifecycle without browser storage", async () => {
		const calls: RpcCall[] = [];
		const storageSet = globalThis.localStorage?.setItem;
		const api = createAgentApi(fakeClient(calls));
		await api.getMcpStatus("runtime-local");
		await api.loginMcp("remote", "runtime-local");
		await api.completeMcpLogin(
			"remote",
			"0000000000000001",
			"http://127.0.0.1:43123/oauth/callback/0000000000000001?code=code&state=state",
			"runtime-local",
		);
		await api.cancelMcpLogin("remote", "0000000000000001", "runtime-local");
		await api.logoutMcp("remote", "runtime-local");
		expect(calls).toEqual([
			{ method: "mcp.status", params: { runtime_id: "runtime-local" } },
			{ method: "mcp.login", params: { server: "remote", runtime_id: "runtime-local" } },
			{
				method: "mcp.complete",
				params: {
					server: "remote",
					login_id: "0000000000000001",
					callback_url:
						"http://127.0.0.1:43123/oauth/callback/0000000000000001?code=code&state=state",
					runtime_id: "runtime-local",
				},
			},
			{
				method: "mcp.cancel",
				params: { server: "remote", login_id: "0000000000000001", runtime_id: "runtime-local" },
			},
			{ method: "mcp.logout", params: { server: "remote", runtime_id: "runtime-local" } },
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

	it("omits an unspecified provider so the daemon can choose its configured default", async () => {
		const calls: { method: string; params?: Record<string, unknown> }[] = [];
		await createAgentApi(fakeClient(calls)).startSession({
			sessionId: "session-default-provider",
			metadata: {},
			clientInputId: "input-1",
			priority: "follow_up",
			content: [{ type: "text", text: "hello" }],
		});
		expect(calls[0]?.params).not.toHaveProperty("provider");
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

	it("pages server-projected targets and fences a selected source entry", async () => {
		const calls: RpcCall[] = [];
		const api = createAgentApi(fakeClient(calls));
		await api.getHistoryTargets("source", { beforeSequence: 400, limit: 25 });
		await api.switchHistory({
			sessionId: "source",
			leafId: "finish",
			sourceEntryId: "user-401",
			expectedActiveLeafId: "active",
			expectedTranscriptRevision: 7,
		});
		expect(calls).toEqual([
			{
				method: "history.targets",
				params: { session_id: "source", before_sequence: 400, limit: 25 },
			},
			{
				method: "history.switch",
				params: {
					session_id: "source",
					leaf_id: "finish",
					source_entry_id: "user-401",
					expected_active_leaf_id: "active",
					expected_transcript_revision: 7,
					active_branch_entry_ids: undefined,
					return_active_branch: undefined,
					missing_body_ids: undefined,
				},
			},
		]);
	});

});

describe("AgentApi history fork wire format", () => {
	it("duplicates the session with only its id", async () => {
		const calls: RpcCall[] = [];
		await createAgentApi(fakeClient(calls)).forkHistory({ sessionId: "source" });
		expect(calls).toEqual([{
			method: "history.fork",
			params: { session_id: "source" },
			options: { timeoutMs: WORKSPACE_OPERATION_REQUEST_TIMEOUT_MS },
		}]);
	});
});

describe("AgentApi delegation launch identity", () => {
	it("retains each caller launch ID across a parallel launch batch", async () => {
		const calls: RpcCall[] = [];
		const api = createAgentApi(fakeClient(calls));

		await Promise.all([
			api.startFullDelegation({
				parentSessionId: "parent",
				clientLaunchId: "full-launch",
				role: "implementer",
				prompt: "Implement it",
			}),
			api.startReadonlyDelegationFanout({
				parentSessionId: "parent",
				clientLaunchId: "readonly-launch",
				tasks: [{ role: "reviewer", prompt: "Review it" }],
			}),
		]);

		expect(calls).toEqual([
			{
				method: "delegation.start_full",
				params: {
					parent_session_id: "parent",
					client_launch_id: "full-launch",
					role: "implementer",
					prompt: "Implement it",
					workflow: undefined,
					label: undefined,
				},
			},
			{
				method: "delegation.start_readonly_fanout",
				params: {
					parent_session_id: "parent",
					client_launch_id: "readonly-launch",
					tasks: [{ role: "reviewer", prompt: "Review it" }],
					workflow: undefined,
					label: undefined,
				},
			},
		]);
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
