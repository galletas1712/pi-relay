// mcp.toml parsing + validation — port of pi-relay rust/crates/agent-mcp/src/config.rs
// (and oauth_config.rs). Wire-compatible shapes (transport is a TAGGED SUB-TABLE):
//
//   [servers.slack]
//   transport = { type = "streamable_http", url = "https://mcp.slack.com/mcp",
//                 auth = { type = "oauth", client_id = "..." } }
//   allow_all_tools = true
//   # or: [servers.slack.transport] with url/auth as keys — same table shape.
//
//   [servers.local]           # legacy bare form (no transport table) = stdio
//   command = "npx"
//   args = ["-y", "some-mcp"]
//
// Secrets never enter config-derived data: literal stdio env rejects
// secret-like names (use inherit_env), bearer tokens come from env vars,
// OAuth tokens live in the credentials file — never in PG or the workspace.
import { createHash } from "node:crypto";
import { readFileSync, statSync } from "node:fs";
import { parse as parseToml } from "smol-toml";

export const DEFAULT_STARTUP_TIMEOUT_MS = 10_000;
export const DEFAULT_CALL_TIMEOUT_MS = 30_000;
const MAX_TIMEOUT_MS = 10 * 60 * 1000;
const MAX_SERVERS = 64;
const MAX_PARALLEL_CALLS = 32;
const MAX_CONFIG_BYTES = 1024 * 1024;
const MAX_COMMAND_BYTES = 4 * 1024;
const MAX_CWD_BYTES = 16 * 1024;
const MAX_URL_BYTES = 16 * 1024;
const MAX_ARGS = 256;
const MAX_ARG_BYTES = 16 * 1024;
const MAX_TOTAL_ARG_BYTES = 128 * 1024;
const MAX_ENV_ENTRIES = 128;
const MAX_ENV_VALUE_BYTES = 16 * 1024;
const MAX_INHERITED_ENV = 128;
const MAX_ENABLED_TOOLS = 512;
const MAX_TOOL_NAME_BYTES = 256;
const DEFAULT_CALLBACK_TIMEOUT_MS = 5 * 60 * 1000;
const MAX_CALLBACK_TIMEOUT_MS = 10 * 60 * 1000;

export class McpConfigError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "McpConfigError";
	}
}

export interface McpOAuthConfig {
	type: "oauth";
	clientId?: string;
	scopes?: string[];
	resource?: string;
	callbackPort?: number; // 1..65535; undefined = ephemeral
	callbackTimeoutMs: number;
}

export interface McpBearerEnvConfig {
	type: "bearer_env";
	env: string;
}

export type McpHttpAuthConfig = McpOAuthConfig | McpBearerEnvConfig;

export interface McpStdioTransport {
	type: "stdio";
	command: string;
	args: string[];
	cwd?: string;
	env: Record<string, string>;
	inheritEnv: string[];
}

export interface McpStreamableHttpTransport {
	type: "streamable_http";
	url: string;
	auth?: McpHttpAuthConfig;
}

export type McpTransport = McpStdioTransport | McpStreamableHttpTransport;

export interface McpServerConfig {
	transport: McpTransport;
	startupTimeoutMs: number;
	callTimeoutMs: number;
	parallelCalls: number;
	allowAllTools: boolean;
	enabledTools: string[];
}

export interface McpConfig {
	servers: Map<string, McpServerConfig>;
}

// ---- fingerprints (agent-mcp-types catalog.rs: canonical JSON + sha256 hex) ---------

type Json = null | boolean | number | string | Json[] | { [k: string]: Json };

function canonicalJson(value: unknown): Json {
	if (Array.isArray(value)) return value.map(canonicalJson);
	if (value !== null && typeof value === "object") {
		const out: Record<string, Json> = {};
		for (const k of Object.keys(value as Record<string, unknown>).sort()) {
			out[k] = canonicalJson((value as Record<string, unknown>)[k]);
		}
		return out;
	}
	return value as Json;
}

export function fingerprintJson(value: unknown): string {
	return createHash("sha256").update(JSON.stringify(canonicalJson(value)), "utf8").digest("hex");
}

function sha256Hex(s: string): string {
	return createHash("sha256").update(s, "utf8").digest("hex");
}

/** Semantic fingerprint of ONE server config (config.rs semantic_fingerprint_input).
 * Secret stdio env values enter only as sha256 hashes. */
export function serverFingerprint(server: McpServerConfig): string {
	let body: Record<string, unknown>;
	if (server.transport.type === "stdio") {
		const t = server.transport;
		const envHashes: Record<string, string> = {};
		for (const [k, v] of Object.entries(t.env).sort(([a], [b]) => (a < b ? -1 : 1))) envHashes[k] = sha256Hex(v);
		body = {
			command: t.command,
			args: t.args,
			cwd: t.cwd ?? null,
			env_hashes: envHashes,
			inherit_env: [...t.inheritEnv].sort(),
			parallel_calls: server.parallelCalls,
			allow_all_tools: server.allowAllTools,
			enabled_tools: [...server.enabledTools].sort(),
		};
	} else {
		const t = server.transport;
		let transport: Record<string, unknown>;
		if (t.auth?.type === "oauth") {
			const clientId = t.auth.clientId?.trim() || null;
			transport = {
				type: "streamable_http",
				url: canonicalUrl(t.url),
				auth: { type: "oauth", client_id: clientId, scopes: t.auth.scopes ?? null, resource: t.auth.resource ?? null },
			};
		} else {
			transport = {
				type: "streamable_http",
				url: t.url,
				auth: t.auth ? (t.auth.type === "bearer_env" ? { type: "bearer_env", env: t.auth.env } : null) : null,
			};
		}
		body = {
			transport,
			parallel_calls: server.parallelCalls,
			allow_all_tools: server.allowAllTools,
			enabled_tools: [...server.enabledTools].sort(),
		};
	}
	return fingerprintJson(body);
}

/** Whole-config fingerprint (McpConfig::semantic_fingerprint_input). */
export function configFingerprint(config: McpConfig): string {
	const servers: Record<string, unknown> = {};
	for (const [id, server] of [...config.servers.entries()].sort(([a], [b]) => (a < b ? -1 : 1))) {
		// per-server semantic fingerprint input keyed by id
		servers[id] = JSON.parse(JSON.stringify(serverSemanticInput(server)));
	}
	return fingerprintJson({ servers });
}

function serverSemanticInput(server: McpServerConfig): unknown {
	// recompute the same structure serverFingerprint hashes
	if (server.transport.type === "stdio") {
		const t = server.transport;
		const envHashes: Record<string, string> = {};
		for (const [k, v] of Object.entries(t.env)) envHashes[k] = sha256Hex(v);
		return {
			command: t.command,
			args: t.args,
			cwd: t.cwd ?? null,
			env_hashes: envHashes,
			inherit_env: [...t.inheritEnv].sort(),
			parallel_calls: server.parallelCalls,
			allow_all_tools: server.allowAllTools,
			enabled_tools: [...server.enabledTools].sort(),
		};
	}
	const t = server.transport;
	const transport =
		t.auth?.type === "oauth"
			? {
					type: "streamable_http",
					url: canonicalUrl(t.url),
					auth: {
						type: "oauth",
						client_id: t.auth.clientId?.trim() || null,
						scopes: t.auth.scopes ?? null,
						resource: t.auth.resource ?? null,
					},
				}
			: { type: "streamable_http", url: t.url, auth: t.auth ?? null };
	return {
		transport,
		parallel_calls: server.parallelCalls,
		allow_all_tools: server.allowAllTools,
		enabled_tools: [...server.enabledTools].sort(),
	};
}

// ---- validation helpers --------------------------------------------------------------

function fail(message: string): never {
	throw new McpConfigError(message);
}

function validateServerId(id: string): void {
	if (id.length === 0 || new TextEncoder().encode(id).length > 128) fail("server id must contain between 1 and 128 bytes");
	// eslint-disable-next-line no-control-regex
	if (/[\x00-\x1f\x7f]/.test(id)) fail("server id must not contain control characters");
}

function validateEnvName(name: string): void {
	if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(name)) fail(`invalid environment variable name ${JSON.stringify(name)}`);
}

const SECRET_FRAGMENTS = ["TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "API_KEY", "ACCESS_KEY", "PRIVATE_KEY", "AUTH", "COOKIE", "SESSION"];
const SECRET_COMPONENTS = ["PAT", "BEARER", "SSH"];
const SECRET_EXACT = ["DATABASE_URL", "DB_URL", "POSTGRES_URL", "POSTGRESQL_URL", "MYSQL_URL", "MARIADB_URL", "MONGODB_URI", "REDIS_URL"];

function secretLikeEnvName(name: string): boolean {
	const upper = name.toUpperCase();
	const components = upper.split("_");
	return (
		SECRET_FRAGMENTS.some((f) => upper.includes(f)) ||
		SECRET_COMPONENTS.some((c) => components.includes(c)) ||
		SECRET_EXACT.includes(upper)
	);
}

export function canonicalUrl(raw: string): string {
	const url = new URL(raw);
	url.hash = "";
	// URL normalizes case/trailing dots differently than reqwest; keep the
	// serializer's output as the canonical form.
	return url.toString();
}

function validateHttpUrl(raw: string): void {
	if (new TextEncoder().encode(raw).length > MAX_URL_BYTES) fail(`Streamable HTTP URL exceeds ${MAX_URL_BYTES} bytes`);
	let url: URL;
	try {
		url = new URL(raw);
	} catch {
		fail("parse Streamable HTTP URL");
	}
	if (url.username !== "" || url.password !== "") fail("Streamable HTTP URL must not contain credentials");
	const host = url.hostname.toLowerCase().replace(/^\[|\]$/g, "");
	const v4 = host.split(".");
	const loopback =
		host === "localhost" ||
		host === "::1" ||
		(v4.length === 4 && v4[0] === "127" && v4.every((p) => /^\d{1,3}$/.test(p)));
	if (url.protocol !== "https:" && !(url.protocol === "http:" && loopback)) {
		fail("Streamable HTTP URL must use HTTPS (HTTP is limited to loopback hosts)");
	}
	if (raw.includes("#")) fail("Streamable HTTP URL must not contain a fragment");
}

function validateAuth(auth: McpHttpAuthConfig): void {
	if (auth.type === "bearer_env") {
		validateEnvName(auth.env);
		return;
	}
	if (auth.callbackPort !== undefined && (auth.callbackPort < 1 || auth.callbackPort > 65535)) {
		fail("callback_port must be between 1 and 65535");
	}
	if (auth.callbackTimeoutMs === 0 || auth.callbackTimeoutMs > MAX_CALLBACK_TIMEOUT_MS) {
		fail(`callback_timeout_ms must be between 1 and ${MAX_CALLBACK_TIMEOUT_MS}`);
	}
}

function validateStdio(t: McpStdioTransport): void {
	const cmdBytes = new TextEncoder().encode(t.command).length;
	if (t.command.trim() === "" || cmdBytes > MAX_COMMAND_BYTES) fail(`command must contain between 1 and ${MAX_COMMAND_BYTES} bytes`);
	if (t.cwd !== undefined && new TextEncoder().encode(t.cwd).length > MAX_CWD_BYTES) fail(`cwd exceeds ${MAX_CWD_BYTES} bytes`);
	if (t.args.length > MAX_ARGS || t.args.some((a) => new TextEncoder().encode(a).length > MAX_ARG_BYTES)) {
		fail("MCP command arguments exceed configured bounds");
	}
	const totalArgBytes = t.args.reduce((n, a) => n + new TextEncoder().encode(a).length, 0);
	if (totalArgBytes > MAX_TOTAL_ARG_BYTES) fail("MCP command arguments exceed configured bounds");
	if (Object.keys(t.env).length > MAX_ENV_ENTRIES || Object.values(t.env).some((v) => new TextEncoder().encode(v).length > MAX_ENV_VALUE_BYTES)) {
		fail("literal environment exceeds configured bounds");
	}
	if (t.inheritEnv.length > MAX_INHERITED_ENV) fail(`inherit_env has more than ${MAX_INHERITED_ENV} entries`);
	for (const name of [...Object.keys(t.env), ...t.inheritEnv]) validateEnvName(name);
	const secretName = Object.keys(t.env).find((n) => secretLikeEnvName(n));
	if (secretName) fail(`secret-like environment variable ${secretName} must use inherit_env instead of literal env`);
	if (Object.keys(t.env).some((n) => t.inheritEnv.includes(n))) fail("an environment name cannot appear in both env and inherit_env");
}

function validateServer(server: McpServerConfig): void {
	if (server.transport.type === "stdio") validateStdio(server.transport);
	else {
		validateHttpUrl(server.transport.url);
		if (server.transport.auth) validateAuth(server.transport.auth);
	}
	if (!server.allowAllTools && server.enabledTools.length === 0) fail("enabled_tools is required unless allow_all_tools is true");
	if (server.parallelCalls === 0 || server.parallelCalls > MAX_PARALLEL_CALLS) fail(`parallel_calls must be between 1 and ${MAX_PARALLEL_CALLS}`);
	for (const [name, value] of [
		["startup_timeout_ms", server.startupTimeoutMs],
		["call_timeout_ms", server.callTimeoutMs],
	] as const) {
		if (value === 0 || value > MAX_TIMEOUT_MS) fail(`${name} must be between 1 and ${MAX_TIMEOUT_MS}`);
	}
	if (server.enabledTools.length > MAX_ENABLED_TOOLS || server.enabledTools.some((n) => n === "" || new TextEncoder().encode(n).length > MAX_TOOL_NAME_BYTES)) {
		fail("enabled_tools exceeds configured bounds");
	}
}

// ---- parsing -------------------------------------------------------------------------

function asRecord(v: unknown, what: string): Record<string, unknown> {
	if (typeof v !== "object" || v === null || Array.isArray(v)) fail(`${what} must be a table`);
	return v as Record<string, unknown>;
}

function asString(v: unknown, what: string): string {
	if (typeof v !== "string") fail(`${what} must be a string`);
	return v;
}

function asOptString(v: unknown, what: string): string | undefined {
	return v === undefined ? undefined : asString(v, what);
}

function asStringList(v: unknown, what: string): string[] {
	if (v === undefined) return [];
	if (!Array.isArray(v) || v.some((x) => typeof x !== "string")) fail(`${what} must be an array of strings`);
	return [...(v as string[])];
}

function asTimeout(v: unknown, what: string, dflt: number): number {
	if (v === undefined) return dflt;
	if (typeof v !== "number" || !Number.isInteger(v)) fail(`${what} must be an integer`);
	return v;
}

function parseAuth(raw: unknown, what: string): McpHttpAuthConfig | undefined {
	if (raw === undefined) return undefined;
	const o = asRecord(raw, what);
	const type = asString(o.type, `${what}.type`);
	if (type === "bearer_env") {
		return { type: "bearer_env", env: asString(o.env, `${what}.env`) };
	}
	if (type === "oauth") {
		return {
			type: "oauth",
			clientId: asOptString(o.client_id, `${what}.client_id`),
			scopes: o.scopes === undefined ? undefined : asStringList(o.scopes, `${what}.scopes`),
			resource: asOptString(o.resource, `${what}.resource`),
			callbackPort: o.callback_port === undefined ? undefined : asTimeout(o.callback_port, `${what}.callback_port`, 0),
			callbackTimeoutMs: asTimeout(o.callback_timeout_ms, `${what}.callback_timeout_ms`, DEFAULT_CALLBACK_TIMEOUT_MS),
		};
	}
	fail(`${what}.type must be "bearer_env" or "oauth"`);
}

function parseTransport(id: string, raw: unknown): McpTransport {
	const o = asRecord(raw, `servers.${id}.transport`);
	const type = asString(o.type, `servers.${id}.transport.type`);
	if (type === "stdio") {
		return {
			type: "stdio",
			command: asString(o.command, `servers.${id}.transport.command`),
			args: asStringList(o.args, `servers.${id}.transport.args`),
			cwd: asOptString(o.cwd, `servers.${id}.transport.cwd`),
			env: parseEnv(o.env, `servers.${id}.transport.env`),
			inheritEnv: asStringList(o.inherit_env, `servers.${id}.transport.inherit_env`),
		};
	}
	if (type === "streamable_http") {
		return {
			type: "streamable_http",
			url: asString(o.url, `servers.${id}.transport.url`),
			auth: parseAuth(o.auth, `servers.${id}.transport.auth`),
		};
	}
	fail(`servers.${id}.transport.type must be "stdio" or "streamable_http"`);
}

function parseServer(id: string, raw: unknown): McpServerConfig {
	const o = asRecord(raw, `servers.${id}`);
	let transport: McpTransport;
	if (o.transport !== undefined) {
		transport = parseTransport(id, o.transport);
	} else if (o.command !== undefined) {
		// legacy bare form: stdio (LegacyMcpServerConfig)
		transport = {
			type: "stdio",
			command: asString(o.command, `servers.${id}.command`),
			args: asStringList(o.args, `servers.${id}.args`),
			cwd: asOptString(o.cwd, `servers.${id}.cwd`),
			env: parseEnv(o.env, `servers.${id}.env`),
			inheritEnv: asStringList(o.inherit_env, `servers.${id}.inherit_env`),
		};
	} else {
		fail(`servers.${id} needs a transport table (type = "stdio"|"streamable_http") or a legacy command`);
	}
	return {
		transport,
		startupTimeoutMs: asTimeout(o.startup_timeout_ms, `servers.${id}.startup_timeout_ms`, DEFAULT_STARTUP_TIMEOUT_MS),
		callTimeoutMs: asTimeout(o.call_timeout_ms, `servers.${id}.call_timeout_ms`, DEFAULT_CALL_TIMEOUT_MS),
		parallelCalls: asTimeout(o.parallel_calls, `servers.${id}.parallel_calls`, 1),
		allowAllTools: o.allow_all_tools === true,
		enabledTools: asStringList(o.enabled_tools, `servers.${id}.enabled_tools`),
	};
}

function parseEnv(raw: unknown, what: string): Record<string, string> {
	if (raw === undefined) return {};
	const o = asRecord(raw, what);
	const out: Record<string, string> = {};
	for (const [k, v] of Object.entries(o)) out[k] = asString(v, `${what}.${k}`);
	return out;
}

export function parseMcpConfig(contents: string): McpConfig {
	let doc: Record<string, unknown>;
	try {
		doc = parseToml(contents) as Record<string, unknown>;
	} catch (err) {
		throw new McpConfigError(`parse MCP config: ${err instanceof Error ? err.message : err}`);
	}
	const serversRaw = asRecord(doc.servers ?? {}, "servers");
	const servers = new Map<string, McpServerConfig>();
	for (const [id, raw] of Object.entries(serversRaw)) {
		validateServerId(id);
		const server = parseServer(id, raw);
		try {
			validateServer(server);
		} catch (err) {
			throw new McpConfigError(`invalid MCP server ${id}: ${err instanceof Error ? err.message : err}`);
		}
		servers.set(id, server);
	}
	if (servers.size > MAX_SERVERS) fail(`MCP config has more than ${MAX_SERVERS} servers`);
	return { servers };
}

export function loadMcpConfig(path: string): McpConfig {
	let st;
	try {
		st = statSync(path);
	} catch {
		throw new McpConfigError(`open MCP config ${path}`);
	}
	if (st.size > MAX_CONFIG_BYTES) throw new McpConfigError(`MCP config exceeds ${MAX_CONFIG_BYTES} bytes`);
	return parseMcpConfig(readFileSync(path, "utf8"));
}

/** What tool names is this server allowed to expose? (tool_enabled) */
export function toolEnabled(server: McpServerConfig, name: string): boolean {
	return server.allowAllTools || server.enabledTools.includes(name);
}
