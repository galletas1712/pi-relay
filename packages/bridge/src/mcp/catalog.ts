// Session MCP artifacts — written by the bridge before host spawn so the
// KERNEL can build its own MCP connections without ever talking to the
// bridge. Layout under <workspaceStateRoot>/sessions/<sessionId>/mcp/:
//
//   catalog.json  — the session's selected servers + tools + auth kinds and
//                   per-server config fingerprints (McpSessionManifest seed)
//   skills/       — generated prime-harness Python skills (one per server),
//                   exposed via PRIME_HARNESS_EXTRA_SKILLS_DIRS
//
// Tokens are NOT here: OAuth credentials live in the shared 0600 store
// (PI_RELAY_MCP_CREDENTIALS) and bearer tokens stay in host env vars —
// never in PG, never in a session workspace.
import { mkdirSync, readdirSync, writeFileSync, rmSync } from "node:fs";
import { join } from "node:path";
import type { McpManager, McpSessionSelection } from "./manager.ts";
import type { McpServerConfig } from "./config.ts";

export interface McpCatalogServer {
	server: string;
	revision: string;
	transport:
		| { type: "stdio"; command: string; args: string[]; cwd?: string; env: Record<string, string>; inherit_env: string[] }
		| { type: "streamable_http"; url: string; auth: { kind: "none" } | { kind: "bearer_env"; env: string } | { kind: "oauth" } };
	call_timeout_ms: number;
	tools: string[]; // selected raw tool names
}

export interface McpSessionCatalog {
	version: 1;
	inventory_revision: string;
	servers: McpCatalogServer[];
}

export function buildSessionCatalog(manager: McpManager, selection: McpSessionSelection): McpSessionCatalog {
	const servers: McpCatalogServer[] = [];
	for (const sel of selection.servers) {
		const cfg = manager.serverConfig(sel.server);
		const revision = manager.serverRevision(sel.server);
		if (!cfg || !revision) throw new Error(`unknown mcp server in selection: ${sel.server}`);
		let transport: McpCatalogServer["transport"];
		if (cfg.transport.type === "stdio") {
			const t = cfg.transport;
			transport = { type: "stdio", command: t.command, args: t.args, cwd: t.cwd, env: t.env, inherit_env: t.inheritEnv };
		} else {
			const t = cfg.transport;
			transport =
				t.auth?.type === "bearer_env"
					? { type: "streamable_http", url: t.url, auth: { kind: "bearer_env", env: t.auth.env } }
					: t.auth?.type === "oauth"
						? { type: "streamable_http", url: t.url, auth: { kind: "oauth" } }
						: { type: "streamable_http", url: t.url, auth: { kind: "none" } };
		}
		servers.push({ server: sel.server, revision, transport, call_timeout_ms: cfg.callTimeoutMs, tools: [...sel.tools].sort() });
	}
	return { version: 1, inventory_revision: selection.inventory_revision, servers };
}

export interface SessionMcpPaths {
	dir: string;
	catalogPath: string;
	skillsDir: string;
}

export function sessionMcpPaths(stateRoot: string, sessionId: string): SessionMcpPaths {
	const dir = join(stateRoot, "sessions", sessionId, "mcp");
	return { dir, catalogPath: join(dir, "catalog.json"), skillsDir: join(dir, "skills") };
}

/** Write catalog.json for a session (0600 dir tree under state root). */
export function writeSessionMcpCatalog(stateRoot: string, sessionId: string, catalog: McpSessionCatalog): SessionMcpPaths {
	const paths = sessionMcpPaths(stateRoot, sessionId);
	mkdirSync(paths.skillsDir, { recursive: true });
	writeFileSync(paths.catalogPath, JSON.stringify(catalog, null, 2), { mode: 0o600 });
	writeSessionMcpSkills(paths, catalog);
	return paths;
}

/** Python skill module source for one selected server. The module subclasses
 * CatalogMcpIntegration (prime_rlm_runtime.mcp_base) which reads the session
 * catalog + shared credential store from the kernel environment; tools are
 * exposed as async module attributes (PEP 562). */
function skillModuleSource(serverId: string): string {
	const mod = `mcp_${serverId.replace(/[^A-Za-z0-9_]/g, "_")}`;
	return `"""Generated pi-relay MCP skill for the "${serverId}" server (do not edit).

Async tool calls mirror the server's selected tools:

    import ${mod}
    result = await ${mod}.call_tool("<tool>", {...})
    # or, with the selected tool names bound as attributes:
    result = await ${mod}.<tool_name>(**args)

Credentials come from the bridge-managed credential store via the kernel
environment (PI_RELAY_MCP_CATALOG / PI_RELAY_MCP_CREDENTIALS); OAuth tokens
refresh in-place under the store's cross-process lock. If the server is not
logged in, calls raise NotEnabled — tell the user to run mcp.login.
"""

from __future__ import annotations

from typing import Any

from prime_rlm_runtime.mcp_base import CatalogMcpIntegration

_integration = CatalogMcpIntegration.for_server(${JSON.stringify(serverId)})


async def list_tools() -> list[dict[str, Any]]:
    """Return this server's selected tools as [{name, description, inputSchema}]."""
    return await _integration.list_tools()


async def call_tool(tool: str, arguments: dict[str, Any] | None = None) -> Any:
    return await _integration.call_tool(tool, arguments)


def __getattr__(name: str) -> Any:  # PEP 562: bind selected tools as attributes
    if name.startswith("_"):
        raise AttributeError(name)
    return _integration.__getattr__(name)
`;
}

function skillMdSource(serverId: string, tools: string[], descriptionHint: string): string {
	const mod = `mcp_${serverId.replace(/[^A-Za-z0-9_]/g, "_")}`;
	const toolList = tools.length > 0 ? tools.map((t) => `\`${t}\``).join(", ") : "(none selected)";
	return `---
name: mcp-${serverId}
description: "MCP tools from the ${serverId} server${descriptionHint}. Selected tools: ${toolList}. Use when a task needs ${serverId} data or actions."
---

# MCP skill: ${serverId}

Pre-imported in the kernel as \`${mod}\`. Call the selected tools as async
module attributes:

\`\`\`python
result = await ${mod}.${tools[0]?.replace(/[^A-Za-z0-9_]/g, "_") ?? "call_tool"}(${tools.length > 0 ? "..." : ""})
# explicit escape hatch:
result = await ${mod}.call_tool("<tool>", {...})
\`\`\`

Selected tools: ${toolList}

If a call raises \`NotEnabled\`, the server needs login — tell the user to
connect it from the client (mcp.login); do NOT ask for tokens or env vars.
`;
}

/** Regenerate the per-server skills for a session selection. Old generated
 * skills for servers no longer selected are removed. */
export function writeSessionMcpSkills(paths: SessionMcpPaths, catalog: McpSessionCatalog): void {
	mkdirSync(paths.skillsDir, { recursive: true });
	const wanted = new Set(catalog.servers.map((s) => `mcp-${s.server}`));
	for (const entry of readdirSync(paths.skillsDir)) {
		if (!wanted.has(entry)) rmSync(join(paths.skillsDir, entry), { recursive: true, force: true });
	}
	for (const server of catalog.servers) {
		// harness import-name rule: skill dir mcp-<server> → import mcp_<server>
		const mod = `mcp_${server.server.replace(/[^A-Za-z0-9_]/g, "_")}`;
		const dir = join(paths.skillsDir, `mcp-${server.server}`);
		mkdirSync(join(dir, "src", mod), { recursive: true });
		writeFileSync(join(dir, "pyproject.toml"), `[project]\nname = "mcp-${server.server}-skill"\nversion = "0.1.0"\ndescription = "Generated pi-relay MCP skill for ${server.server}"\nrequires-python = ">=3.10"\n`, { mode: 0o600 });
		writeFileSync(join(dir, "SKILL.md"), skillMdSource(server.server, server.tools, ""), { mode: 0o600 });
		writeFileSync(join(dir, "src", mod, "__init__.py"), skillModuleSource(server.server), { mode: 0o600 });
	}
}

export function removeSessionMcpArtifacts(stateRoot: string, sessionId: string): void {
	rmSync(sessionMcpPaths(stateRoot, sessionId).dir, { recursive: true, force: true });
}
