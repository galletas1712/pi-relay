// Migrator configuration — env-driven, test-container defaults (M10a).
// SAFETY: the ONLY PG this tool will open is the scratch restore on the TEST
// container (default 127.0.0.1:56432). Live pi-relay (55432) is never touched;
// the config REFUSES any other port unless MIGRATE_ALLOW_NONTEST=1 is set.
import { homedir } from "node:os";
import { join, resolve } from "node:path";

export interface MigratorConfig {
	pgHost: string;
	pgPort: number;
	pgDb: string;
	pgUser: string;
	pgPassword: string;
	outRoot: string;
	/** read-only pi-relay runtime config root (roles/skills/mcp.toml) */
	runtimeConfigRoot: string;
	/** read-only $HOME/.agents (prompt scopes + home-global/project skills) */
	homeAgentsRoot: string;
	/** state for migrated bridge rows (default host_down) */
	emitState: string;
	/** absolute path prefix recorded as session_file in control-plane.sql (default: outRoot/sessions) */
	sessionFilePrefix: string;
}

export function loadConfig(env: NodeJS.ProcessEnv = process.env): MigratorConfig {
	const pgPort = Number(env.MIGRATE_PG_PORT ?? "56432");
	if (pgPort !== 56432 && env.MIGRATE_ALLOW_NONTEST !== "1") {
		throw new Error(
			`refusing MIGRATE_PG_PORT=${pgPort}: the migrator only talks to the TEST container ` +
				`(pi-relay-bridge-test-pg @56432). Set MIGRATE_ALLOW_NONTEST=1 to override (never for live).`,
		);
	}
	const pgPassword = env.MIGRATE_PG_PASSWORD;
	if (!pgPassword) throw new Error("MIGRATE_PG_PASSWORD is required (test container password)");
	const outRoot = resolve(env.MIGRATE_OUT ?? join(process.cwd(), "out"));
	return {
		pgHost: env.MIGRATE_PG_HOST ?? "127.0.0.1",
		pgPort,
		pgDb: env.MIGRATE_PG_DB ?? "pi_relay_migtest",
		pgUser: env.MIGRATE_PG_USER ?? "postgres",
		pgPassword,
		outRoot,
		runtimeConfigRoot: env.MIGRATE_RUNTIME_CONFIG ?? join(homedir(), ".config", "pi-relay", "runtime"),
		homeAgentsRoot: env.MIGRATE_HOME_AGENTS ?? join(homedir(), ".agents"),
		emitState: env.MIGRATE_EMIT_STATE ?? "host_down",
		sessionFilePrefix: env.MIGRATE_SESSION_PREFIX ?? join(outRoot, "sessions"),
	};
}
