// Bridge-hosted MCP client manager — port of pi-relay rust/crates/agent-mcp/src/manager.rs
// semantics onto the @modelcontextprotocol/sdk. The bridge owns the CONTROL
// PLANE (inventory, health, oauth, selection validation); session kernels own
// their own connections via mcp_base.py using the catalog + credential file
// handed to them through the environment (kernel-mediated model exposure).
//
// Fingerprint discipline: every session inventory entry carries the config
// fingerprint so stale selection/catalog is detectable (mcp-client.md).
import { Client } from "@modelcontextprotocol/sdk/client/index.js";
import { StdioClientTransport } from "@modelcontextprotocol/sdk/client/stdio.js";
import { StreamableHTTPClientTransport } from "@modelcontextprotocol/sdk/client/streamableHttp.js";
import { configFingerprint, loadMcpConfig, serverFingerprint, toolEnabled, type McpConfig, type McpServerConfig } from "./config.ts";
import { credentialCompatible, credentialExpired, OAuthCredentialRepository } from "./credentials.ts";
import type { OAuthManager } from "./oauth.ts";

export type McpHealth = "healthy" | "unavailable" | "revoked";
export type McpAuthKind = "none" | "bearer" | "oauth";

export interface McpInventoryTool {
	raw_name: string;
	description: string;
	context_token_estimate: number;
}

export interface McpInventoryServer {
	server: string;
	revision: string;
	health: McpHealth;
	tools: McpInventoryTool[];
}

export interface McpInventory {
	revision: string;
	servers: McpInventoryServer[];
}

export interface McpServerSelection {
	server: string;
	tools: string[];
}

export interface McpSessionSelection {
	inventory_revision: string;
	servers: McpServerSelection[];
}

export interface McpAuthServerStatus {
	server: string;
	auth_kind: McpAuthKind;
	status: string;
	detail?: string;
}

export class McpManagerError extends Error {
	readonly code: string;
	constructor(code: string, message: string) {
		super(message);
		this.name = "McpManagerError";
		this.code = code;
	}
}

const MAX_ERROR_BYTES = 16 * 1024;
const MAX_TOOL_RESULT_BYTES = 1024 * 1024;
const MAX_MCP_PROMPT_SUMMARY_BYTES = 16 * 1024;
const CLIENT_NAME = "pi-relay-bridge";

function truncateError(message: string): string {
	if (message.length <= MAX_ERROR_BYTES) return message;
	return `${message.slice(0, MAX_ERROR_BYTES)} [truncated]`;
}

interface Connection {
	client: Client;
	transport: StdioClientTransport | StreamableHTTPClientTransport;
	fingerprint: string;
}

export interface McpManagerOptions {
	configPath: string;
	credentialsPath: string;
	registrationPath?: string;
	defaultCallbackPort?: number;
	oauth?: OAuthManager; // wired after construction to break the import cycle
}

function canonicalJson(value: unknown): unknown {
	if (Array.isArray(value)) return value.map(canonicalJson);
	if (value !== null && typeof value === "object") {
		const out: Record<string, unknown> = {};
		for (const k of Object.keys(value as Record<string, unknown>).sort()) out[k] = canonicalJson((value as Record<string, unknown>)[k]);
		return out;
	}
	return value;
}

/** declaration_token_estimate: canonical JSON bytes / 4, rounded up. */
export function declarationTokenEstimate(declaration: unknown): number {
	const bytes = new TextEncoder().encode(JSON.stringify(canonicalJson(declaration))).length;
	return Math.ceil(bytes / 4);
}

export class McpManager {
	private config: McpConfig | null = null;
	private configError: string | null = null;
	private readonly connections = new Map<string, Connection>();
	readonly credentials: OAuthCredentialRepository;
	private oauth: OAuthManager | null = null;

	private readonly opts: McpManagerOptions;
	constructor(opts: McpManagerOptions) {
		this.opts = opts;
		this.credentials = new OAuthCredentialRepository(opts.credentialsPath);
		this.oauth = opts.oauth ?? null;
	}

	setOAuth(oauth: OAuthManager): void {
		this.oauth = oauth;
	}

	get revision(): string {
		return this.config ? configFingerprint(this.config) : "";
	}

	serverRevision(serverId: string): string | null {
		const cfg = this.config?.servers.get(serverId);
		return cfg ? serverFingerprint(cfg) : null;
	}

	serverIds(): string[] {
		return [...(this.config?.servers.keys() ?? [])].sort();
	}

	serverConfig(serverId: string): McpServerConfig | null {
		return this.config?.servers.get(serverId) ?? null;
	}

	/** (Re)load mcp.toml from disk. Parse/validation failures degrade the whole
	 * MCP surface (pi-relay: startup fails) — here we surface the error in
	 * inventory so the bridge stays bootable. */
	reload(): { ok: boolean; error?: string; revision: string } {
		try {
			this.config = loadMcpConfig(this.opts.configPath);
			this.configError = null;
		} catch (err) {
			const message = err instanceof Error ? err.message : String(err);
			if (message.startsWith("open MCP config")) {
				// no mcp.toml at all → MCP is empty, not broken
				this.config = { servers: new Map() };
				this.configError = null;
			} else {
				this.config = null;
				this.configError = message;
			}
		}
		void this.disconnectStale();
		return this.configError === null ? { ok: true, revision: this.revision } : { ok: false, error: this.configError, revision: "" };
	}

	private async disconnectStale(): Promise<void> {
		for (const [id, conn] of this.connections) {
			const cfg = this.config?.servers.get(id);
			if (!cfg || serverFingerprint(cfg) !== conn.fingerprint) {
				this.connections.delete(id);
				await conn.client.close().catch(() => {});
			}
		}
	}

	private bearerHeader(serverId: string, cfg: McpServerConfig): Record<string, string> {
		if (cfg.transport.type !== "streamable_http") return {};
		const auth = cfg.transport.auth;
		if (!auth) return {};
		if (auth.type === "bearer_env") {
			const token = process.env[auth.env];
			if (!token) throw new McpManagerError("mcp_auth_missing", `mcp bearer env var ${auth.env} is not set for server ${serverId}`);
			return { Authorization: `Bearer ${token}` };
		}
		return {};
	}

	private async oauthHeader(serverId: string, cfg: McpServerConfig): Promise<Record<string, string>> {
		if (cfg.transport.type !== "streamable_http" || cfg.transport.auth?.type !== "oauth") return {};
		const url = cfg.transport.url;
		const cred = await this.credentials.get(serverId, url);
		if (!cred || !credentialCompatible(cred, serverId, url, cfg.transport.auth.clientId, cfg.transport.auth.scopes, cfg.transport.auth.resource)) {
			throw new McpManagerError("mcp_login_required", `mcp oauth login required for ${serverId}`);
		}
		let token = cred.access_token;
		if (credentialExpired(cred)) {
			if (!this.oauth) throw new McpManagerError("mcp_auth_expired", `mcp oauth credential expired for ${serverId}`);
			try {
				token = (await this.oauth.refresh(serverId, url, cfg.transport.auth)).access_token;
			} catch (err) {
				throw new McpManagerError("mcp_auth_expired", `mcp oauth refresh failed for ${serverId}: ${truncateError(err instanceof Error ? err.message : String(err))}`);
			}
		}
		return { Authorization: `Bearer ${token}` };
	}

	private async connect(serverId: string): Promise<Connection> {
		const cfg = this.config?.servers.get(serverId);
		if (!cfg) throw new McpManagerError("mcp_unknown_server", `unknown mcp server: ${serverId}`);
		const fingerprint = serverFingerprint(cfg);
		const existing = this.connections.get(serverId);
		if (existing && existing.fingerprint === fingerprint) return existing;

		let transport: StdioClientTransport | StreamableHTTPClientTransport;
		if (cfg.transport.type === "stdio") {
			const t = cfg.transport;
			const env: Record<string, string> = { ...t.env };
			for (const name of t.inheritEnv) {
				const v = process.env[name];
				if (v !== undefined) env[name] = v;
			}
			transport = new StdioClientTransport({ command: t.command, args: t.args, cwd: t.cwd, env, stderr: "ignore" });
		} else {
			const headers = { ...this.bearerHeader(serverId, cfg), ...(await this.oauthHeader(serverId, cfg)) };
			transport = new StreamableHTTPClientTransport(new URL(cfg.transport.url), {
				requestInit: Object.keys(headers).length > 0 ? { headers } : undefined,
			});
		}
		const client = new Client({ name: CLIENT_NAME, version: "m8" }, { capabilities: {} });
		try {
			await withTimeout(client.connect(transport), cfg.startupTimeoutMs, `mcp connect to ${serverId}`);
		} catch (err) {
			await client.close().catch(() => {});
			throw new McpManagerError("mcp_unavailable", `mcp connect failed for ${serverId}: ${truncateError(err instanceof Error ? err.message : String(err))}`);
		}
		const conn: Connection = { client, transport, fingerprint };
		this.connections.set(serverId, conn);
		return conn;
	}

	/** Raw tool list (post enabled_tools filter), sorted by raw name. */
	async listTools(serverId: string): Promise<McpInventoryTool[]> {
		const cfg = this.config?.servers.get(serverId);
		if (!cfg) throw new McpManagerError("mcp_unknown_server", `unknown mcp server: ${serverId}`);
		const conn = await this.connect(serverId);
		const out: McpInventoryTool[] = [];
		let cursor: string | undefined;
		for (;;) {
			const page = await withTimeout(conn.client.listTools(cursor ? { cursor } : {}), cfg.callTimeoutMs, `mcp tools/list on ${serverId}`);
			for (const tool of page.tools) {
				if (!toolEnabled(cfg, tool.name)) continue;
				out.push({
					raw_name: tool.name,
					description: tool.description ?? "",
					context_token_estimate: declarationTokenEstimate(tool),
				});
			}
			cursor = page.nextCursor;
			if (!cursor) break;
		}
		out.sort((a, b) => (a.raw_name < b.raw_name ? -1 : a.raw_name > b.raw_name ? 1 : 0));
		return out;
	}

	/** Full inventory across every configured server. Unreachable servers
	 * appear with health=unavailable and empty tools (never block the rest). */
	async inventory(): Promise<McpInventory> {
		if (!this.config) {
			this.reload();
		}
		if (!this.config) {
			throw new McpManagerError("mcp_config_invalid", this.configError ?? "mcp config unavailable");
		}
		const servers: McpInventoryServer[] = [];
		for (const [serverId, cfg] of [...this.config.servers.entries()].sort(([a], [b]) => (a < b ? -1 : 1))) {
			const revision = serverFingerprint(cfg);
			try {
				const tools = await this.listTools(serverId);
				servers.push({ server: serverId, revision, health: "healthy", tools });
			} catch (err) {
				const health: McpHealth = err instanceof McpManagerError && (err.code === "mcp_login_required" || err.code === "mcp_auth_expired") ? "revoked" : "unavailable";
				servers.push({ server: serverId, revision, health, tools: [] });
			}
		}
		return { revision: this.revision, servers };
	}

	/** Validate a session selection against the current inventory. Unknown
	 * servers/tools fail; empty server list = no MCP for the session. */
	async validateSelection(selection: McpSessionSelection): Promise<{ ok: true; inventory: McpInventory } | { ok: false; error: string }> {
		if (
			typeof selection !== "object" ||
			selection === null ||
			!Array.isArray(selection.servers) ||
			typeof selection.inventory_revision !== "string" ||
			selection.inventory_revision === ""
		) {
			// pi-relay shape: { inventory_revision, servers: [{ server, tools }] } (deny_unknown_fields).
			return { ok: false, error: "selection must have inventory_revision and a servers array" };
		}
		if (selection.servers.length === 0) return { ok: true, inventory: { revision: this.revision, servers: [] } };
		const inventory = await this.inventory();
		if (inventory.revision !== selection.inventory_revision) {
			return { ok: false, error: `mcp inventory revision mismatch (stale selection): have ${inventory.revision.slice(0, 12)}, selection pins ${selection.inventory_revision.slice(0, 12)}` };
		}
		const promptSummaryBytes = selection.servers.reduce((n, s) => n + s.server.length + s.tools.reduce((m, t) => m + t.length + 1, 0), 0);
		if (promptSummaryBytes > MAX_MCP_PROMPT_SUMMARY_BYTES) {
			return { ok: false, error: `selected MCP names exceed the ${MAX_MCP_PROMPT_SUMMARY_BYTES}-byte prompt summary limit` };
		}
		for (const sel of selection.servers) {
			const inv = inventory.servers.find((s) => s.server === sel.server);
			if (!inv) return { ok: false, error: `unknown mcp server in selection: ${sel.server}` };
			if (inv.health !== "healthy") return { ok: false, error: `mcp server ${sel.server} is ${inv.health}` };
			for (const tool of sel.tools) {
				if (!inv.tools.some((t) => t.raw_name === tool)) return { ok: false, error: `unknown tool ${tool} on mcp server ${sel.server}` };
			}
		}
		return { ok: true, inventory };
	}

	/** Control-plane tool call (used by mcp.call; kernel calls go through
	 * mcp_base.py with the same catalog). */
	async callTool(serverId: string, tool: string, args: Record<string, unknown>): Promise<unknown> {
		const cfg = this.config?.servers.get(serverId);
		if (!cfg) throw new McpManagerError("mcp_unknown_server", `unknown mcp server: ${serverId}`);
		if (!toolEnabled(cfg, tool)) throw new McpManagerError("mcp_tool_not_enabled", `mcp tool ${tool} is not enabled on ${serverId}`);
		const conn = await this.connect(serverId);
		let result: Awaited<ReturnType<Client["callTool"]>>;
		try {
			result = await withTimeout(conn.client.callTool({ name: tool, arguments: args }), cfg.callTimeoutMs, `mcp tools/call ${tool} on ${serverId}`);
		} catch (err) {
			throw new McpManagerError("mcp_call_failed", `mcp call ${tool} on ${serverId} failed: ${truncateError(err instanceof Error ? err.message : String(err))}`);
		}
		const serialized = JSON.stringify(result);
		if (new TextEncoder().encode(serialized).length > MAX_TOOL_RESULT_BYTES) {
			throw new McpManagerError("mcp_result_oversized", `mcp result from ${tool} exceeds ${MAX_TOOL_RESULT_BYTES} bytes`);
		}
		return result;
	}

	/** Per-server auth status for mcp.status (McpAuthServerStatus). */
	async authStatus(): Promise<McpAuthServerStatus[]> {
		if (!this.config) this.reload();
		if (!this.config) return [];
		const out: McpAuthServerStatus[] = [];
		for (const [serverId, cfg] of [...this.config.servers.entries()].sort(([a], [b]) => (a < b ? -1 : 1))) {
			let kind: McpAuthKind = "none";
			let status = "non_oauth";
			if (cfg.transport.type === "streamable_http" && cfg.transport.auth) {
				if (cfg.transport.auth.type === "bearer_env") {
					kind = "bearer";
					status = process.env[cfg.transport.auth.env] ? "bearer" : "login_required";
				} else {
					kind = "oauth";
					if (this.oauth) {
						status = await this.oauth.status(serverId, cfg.transport.url, cfg.transport.auth, false);
					} else {
						status = "unknown";
					}
				}
			}
			out.push({ server: serverId, auth_kind: kind, status });
		}
		return out;
	}

	async disconnectAll(): Promise<void> {
		for (const [, conn] of this.connections) await conn.client.close().catch(() => {});
		this.connections.clear();
	}
}

async function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
	let timer: NodeJS.Timeout;
	try {
		return await Promise.race([
			p,
			new Promise<never>((_, reject) => {
				timer = setTimeout(() => reject(new McpManagerError("mcp_timeout", `${what} timed out after ${ms}ms`)), ms);
			}),
		]);
	} finally {
		clearTimeout(timer!);
	}
}
