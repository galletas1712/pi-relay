// Bridge entrypoint: migrate PG control plane, reconcile sessions from PG
// (respawn hosts for everything not closed), then serve the WSS contract.
import { migrate, closeDb } from "./db.ts";
import { initMcp, reconcileOnBoot, shutdownSupervisor } from "./supervisor.ts";
import { startServer } from "./server.ts";
import { config } from "./config.ts";
import { validateWorkspaceRoot } from "./workspaces.ts";
import { writeFileSync, mkdirSync } from "node:fs";

async function main(): Promise<void> {
	mkdirSync(config.dataDir, { recursive: true });
	const applied = await migrate();
	if (applied.length) console.error(`[bridge] applied migrations: ${applied.join(", ")}`);
	// M8: probe the workspace state root (port of pi-relay validate_root) before
	// any session is materialized or reconciled.
	await validateWorkspaceRoot();
	// M8 phase 2: load mcp.toml (absent = empty MCP; invalid = mcp.* errors but
	// the bridge still boots).
	const mcpBoot = initMcp();
	if (mcpBoot.ok) console.error(`[bridge] mcp config loaded (revision ${mcpBoot.revision.slice(0, 12) || "empty"})`);
	else console.error(`[bridge] mcp config INVALID: ${mcpBoot.error}`);
	await reconcileOnBoot();
	startServer();
	if (process.env.BRIDGE_PID_FILE) writeFileSync(process.env.BRIDGE_PID_FILE, String(process.pid));
	console.error(`[bridge] ready (pid ${process.pid})`);

	const shutdown = async (signal: string) => {
		console.error(`[bridge] ${signal} received, shutting down`);
		try {
			await shutdownSupervisor();
		} finally {
			await closeDb();
			process.exit(0);
		}
	};
	process.on("SIGINT", () => void shutdown("SIGINT"));
	process.on("SIGTERM", () => void shutdown("SIGTERM"));
}

main().catch((err) => {
	console.error("[bridge] fatal:", err);
	process.exit(1);
});
