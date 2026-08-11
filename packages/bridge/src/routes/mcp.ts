// mcp.* contract methods (M8 phase 2). Kernel-mediated MCP: the bridge owns
// config, connections, OAuth state, and the 0600 credential store; the kernel
// consumes the per-session catalog + generated skills. Session-less auth
// lifecycle events fan out through the injected broadcast (server.ts owns the
// connection set).
import * as supervisor from "../supervisor.ts";
import { BridgeError } from "../supervisor.ts";
import { requireString, type MethodTable, type McpAuthBroadcast } from "./common.ts";

export function mcpMethods(broadcastMcpAuthChanged: McpAuthBroadcast): MethodTable {
	return {
		async "mcp.inventory"() {
				return supervisor.getMcpManager().inventory();
			},
			async "mcp.status"() {
				return { servers: await supervisor.getMcpManager().authStatus() };
			},
			async "mcp.reload"() {
				const result = supervisor.getMcpManager().reload();
				if (!result.ok) throw new BridgeError("mcp_config_invalid", result.error ?? "invalid mcp config");
				return result;
			},
			async "mcp.select"(_conn, params) {
				const sessionId = requireString(params, "sessionId");
				if (params.selection === undefined) throw new BridgeError("bad_request", "params.selection is required");
				return supervisor.selectMcp(sessionId, params.selection);
			},
			async "mcp.call"(_conn, params) {
				const server = requireString(params, "server");
				const tool = requireString(params, "tool");
				const args = (params.arguments ?? {}) as Record<string, unknown>;
				if (typeof args !== "object" || args === null || Array.isArray(args)) throw new BridgeError("bad_request", "params.arguments must be an object");
				// Control-plane call. When a sessionId is given and has a selection, the
				// tool must be in it (pi-relay: model surface is the selected set).
				if (params.sessionId !== undefined) {
					const sessionId = requireString(params, "sessionId");
					const state = await supervisor.getSessionState(sessionId);
					const sel = state?.mcpSelection as { servers?: { server: string; tools: string[] }[] } | null;
					if (sel?.servers) {
						const entry = sel.servers.find((s) => s.server === server);
						if (!entry) throw new BridgeError("mcp_tool_not_enabled", `mcp server ${server} is not selected for session ${sessionId}`);
						if (!entry.tools.includes(tool)) throw new BridgeError("mcp_tool_not_enabled", `mcp tool ${tool} is not selected for session ${sessionId}`);
					}
				}
				return supervisor.getMcpManager().callTool(server, tool, args);
			},
			async "mcp.login"(_conn, params) {
				const server = requireString(params, "server");
				const cfg = supervisor.getMcpManager().serverConfig(server);
				if (!cfg) throw new BridgeError("mcp_unknown_server", `unknown mcp server: ${server}`);
				if (cfg.transport.type !== "streamable_http" || cfg.transport.auth?.type !== "oauth") {
					throw new BridgeError("mcp_not_oauth", `mcp server ${server} does not use oauth`);
				}
				const pending = await supervisor.getOAuthManager().beginLogin(server, cfg.transport.url, cfg.transport.auth);
				// Completion (or failure) surfaces as mcp.authChanged on every attached
				// control connection.
				void supervisor
					.getOAuthManager()
					.waitLogin(server)
					.then((cred) => broadcastMcpAuthChanged(server, "oauth_ready", cred.granted_scopes))
					.catch((err) => broadcastMcpAuthChanged(server, "login_required", undefined, String(err instanceof Error ? err.message : err)));
				return { authorizationUrl: pending.authorizationUrl, state: pending.state, callbackPort: pending.callbackPort, expiresAtMs: pending.expiresAtMs };
			},
			async "mcp.complete"(_conn, params) {
				const server = requireString(params, "server");
				const code = requireString(params, "code");
				const state = params.state !== undefined ? requireString(params, "state") : undefined;
				const cred = await supervisor.getOAuthManager().completeLogin(server, code, state);
				broadcastMcpAuthChanged(server, "oauth_ready", cred.granted_scopes);
				return { ok: true, scopes: cred.granted_scopes };
			},
			async "mcp.cancel"(_conn, params) {
				const server = requireString(params, "server");
				return { cancelled: supervisor.getOAuthManager().cancelLogin(server) };
			},
			async "mcp.logout"(_conn, params) {
				const server = requireString(params, "server");
				const cfg = supervisor.getMcpManager().serverConfig(server);
				if (!cfg || cfg.transport.type !== "streamable_http") throw new BridgeError("mcp_unknown_server", `unknown mcp server: ${server}`);
				const removed = await supervisor.getOAuthManager().logout(server, cfg.transport.url);
				broadcastMcpAuthChanged(server, "login_required");
				return { removed };
			},
	};
}
