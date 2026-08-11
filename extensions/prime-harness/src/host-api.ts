// prime-harness public host API (M7) — documented seam for sibling extensions.
//
// Published at module import as globalThis[Symbol.for("prime-harness.host-api")]
// (same pattern as prime-rlm's src/api.ts). Consumers MUST look it up lazily
// (prime-harness loads AFTER prime-rlm in the demo settings.json).
//
// v1 surface: subagent role resolution for prime-rlm's rlm.run(role=...).
// Roles are harness content, so discovery/validation/storage live here;
// prime-rlm owns only the spawn mechanics (model/effort application, pending
// registration, child prompt sections).

import { discoverRoles, resolveRoleByName, type ResolvedRole, type RoleSearchContext } from "./roles.ts";

export const PRIME_HARNESS_API_VERSION = 1;

export interface HarnessRoleContext {
	sessionId: string;
	sessionDir: string;
	cwd: string;
	agentDir: string;
}

export interface PrimeHarnessHostApi {
	version: typeof PRIME_HARNESS_API_VERSION;
	/** Valid roles visible to the session (name+description, sorted by name). */
	roleCatalog(ctx: HarnessRoleContext): Array<{ name: string; description: string }>;
	/** Fully resolve a role for spawn; throws on unknown or unusable roles. */
	resolveRole(name: string, ctx: HarnessRoleContext): ResolvedRole;
}

/** Wired by index.ts at extension load (bundled skills dir + agentDir are
 * module-locals there). */
let deps: { bundledSkillsDir: string; agentDir: () => string } | undefined;

export function _bindHarnessApi(bundledSkillsDir: string, agentDir: () => string): void {
	deps = { bundledSkillsDir, agentDir };
}

function searchContext(ctx: HarnessRoleContext): RoleSearchContext {
	if (!deps) throw new Error("prime-harness host api not bound (extension not loaded)");
	return {
		agentDir: ctx.agentDir || deps.agentDir(),
		cwd: ctx.cwd,
		bundledSkillsDir: deps.bundledSkillsDir,
		sessionDir: ctx.sessionDir,
		sessionId: ctx.sessionId,
	};
}

const api: PrimeHarnessHostApi = {
	version: PRIME_HARNESS_API_VERSION,
	roleCatalog(ctx) {
		const discovery = discoverRoles(searchContext(ctx));
		return discovery.roles.map((role) => ({ name: role.name, description: role.description }));
	},
	resolveRole(name, ctx) {
		return resolveRoleByName(name, searchContext(ctx));
	},
};

(globalThis as Record<symbol, unknown>)[Symbol.for("prime-harness.host-api")] = api;
