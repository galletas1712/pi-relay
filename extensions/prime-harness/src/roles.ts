// prime-harness subagent roles (M7) — pi-relay role parity.
//
// PROVENANCE: ported from pi-relay
//   rust/agent-runtime/src/skills.rs (resolve_role_file / resolve_role_skills /
//   resolved_role_catalog validation doctrine) and
//   rust/agent-prompt/src/lib.rs (SubagentRole + subagent_role_catalog_json).
// pi-relay discovers roles ONLY from `$runtime_config_root/subagent-roles`
// (SkillOrigin::RuntimeRole). New-stack origin mapping (documented in
// M7-ROLES.md):
//   pi-relay RuntimeRole ($runtime_config_root/subagent-roles)
//     -> "global-dir": <agentDir>/roles/<name>/SKILL.md
//        (agentDir is PI_CODING_AGENT_DIR; on prime-agent-layout installs that
//        is ~/.prime/agent, giving the requested ~/.prime/agent/roles path)
//   (new) "project-dir": <cwd>/.pi/roles/<name>/SKILL.md
//        (pi-relay had no project role origin; mirrors pi's .pi/ project config)
//   (new) "harness-global" / "harness-local": roles stored as continual-harness
//        entries of kind "role" (PA manageability surface; rlm.harness CRUD).
// Precedence (first match wins): harness-local > project-dir > harness-global
// > global-dir. pi-relay has a single origin so it ERRORS on duplicates; the
// new stack treats same-named entries across origins as intentional shadowing
// (same doctrine as harness merge: local beats global).
//
// Frontmatter (pi-relay SkillFrontmatter): name, description, model,
// reasoning_effort, max_tokens, skills. Aliases accepted (documented):
// effort -> reasoning_effort, preload -> skills. Model selector canonical form
// is the new-stack "provider/model-id"; pi-relay's legacy "provider:model" is
// also accepted (parsed by prime-rlm at spawn).

import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { parseFrontmatter } from "@earendil-works/pi-coding-agent";
import { HARNESS_STATE_DIR_NAME } from "./paths.ts";
import { loadHarnessState } from "./store.ts";
import { loadHarnessSkills } from "./skills.ts";

export type RoleOrigin = "harness-local" | "project-dir" | "harness-global" | "global-dir";

export interface ResolvedPreloadedSkill {
	name: string;
	filePath: string;
	/** SKILL.md body (frontmatter stripped) — pi-relay inlines parsed.body. */
	content: string;
	pythonImport?: string;
	pythonPath?: string;
}

export interface ResolvedRole {
	name: string;
	description: string;
	/** Display path: absolute SKILL.md path, or `harness:<scope>:<entry-id>`. */
	filePath: string;
	/** SKILL.md body (frontmatter stripped). */
	body: string;
	model?: string;
	reasoningEffort?: string;
	maxTokens?: number;
	preload: ResolvedPreloadedSkill[];
	origin: RoleOrigin;
}

export interface InvalidRole {
	name: string;
	origin: RoleOrigin;
	reason: string;
}

export interface RoleDiscovery {
	/** Valid roles, deduped by precedence, sorted by name. */
	roles: ResolvedRole[];
	/** Unusable roles (pi-relay omits these from the catalog; resolve-by-name
	 * errors). Kept for diagnostics (prompt-builds.jsonl). */
	invalid: InvalidRole[];
}

export interface RoleSearchContext {
	agentDir: string;
	cwd: string;
	bundledSkillsDir: string;
	/** Session storage dir + id, for harness-local roles. */
	sessionDir?: string;
	sessionId?: string;
}

// ---- frontmatter parsing -----------------------------------------------------

interface ParsedRoleFrontmatter {
	name: string;
	description: string;
	model?: string;
	reasoningEffort?: string;
	maxTokens?: number;
	preload: string[];
}

/** Validate a model selector's SHAPE (registry availability is a spawn-time
 * concern — pi-relay checks availability against live provider config and
 * falls back to its stable default; prime-rlm falls back to the parent model).
 * pi-relay marks a role with a malformed selector unusable; we do the same. */
function isWellFormedModelSelector(raw: string): boolean {
	const trimmed = raw.trim();
	if (!trimmed || trimmed !== raw) return false;
	if (trimmed.includes("/")) {
		const [provider, ...rest] = trimmed.split("/");
		return !!provider && !!rest.join("/");
	}
	const i = trimmed.indexOf(":");
	return i > 0 && i < trimmed.length - 1 && !trimmed.slice(i + 1).includes(":");
}

/** Parse + validate role SKILL.md content. Returns the reason string when
 * invalid (pi-relay resolve_role_file error doctrine). */
function parseRoleContent(
	content: string,
	expectedName: string,
	origin: RoleOrigin,
): { parsed: ParsedRoleFrontmatter; body: string } | { reason: string } {
	const { frontmatter, body } = parseFrontmatter(content);
	const name = typeof frontmatter.name === "string" ? frontmatter.name : "";
	if (!name) return { reason: "missing frontmatter name" };
	// pi-relay: role skill directory must match SKILL.md name (for harness-stored
	// roles the entry id plays the directory role).
	if (name !== expectedName) {
		return { reason: `role name "${name}" does not match ${origin === "harness-local" || origin === "harness-global" ? "harness entry id" : "directory"} "${expectedName}"` };
	}
	const description = typeof frontmatter.description === "string" ? frontmatter.description : "";

	let model: string | undefined;
	if (frontmatter.model !== undefined) {
		if (typeof frontmatter.model !== "string" || !isWellFormedModelSelector(frontmatter.model)) {
			return { reason: `malformed model '${String(frontmatter.model)}'; expected "provider/model-id" (or legacy "provider:model")` };
		}
		model = frontmatter.model;
	}

	// reasoning_effort (canonical, pi-relay) with `effort` alias.
	const effortRaw = frontmatter.reasoning_effort ?? frontmatter.effort;
	let reasoningEffort: string | undefined;
	if (effortRaw !== undefined) {
		if (typeof effortRaw !== "string" || !effortRaw.trim()) {
			return { reason: "reasoning_effort must be a non-empty string" };
		}
		reasoningEffort = effortRaw.trim();
	}

	let maxTokens: number | undefined;
	if (frontmatter.max_tokens !== undefined) {
		const raw = frontmatter.max_tokens;
		if (typeof raw !== "number" || !Number.isInteger(raw) || raw <= 0) {
			return { reason: "max_tokens must be a positive integer" };
		}
		maxTokens = raw;
	}

	// skills (canonical, pi-relay) with `preload` alias.
	const preloadRaw = frontmatter.skills ?? frontmatter.preload;
	const preload: string[] = [];
	if (preloadRaw !== undefined) {
		if (!Array.isArray(preloadRaw)) return { reason: "skills/preload must be a list of skill names" };
		const seen = new Set<string>();
		for (const item of preloadRaw) {
			const requested = typeof item === "string" ? item.trim() : "";
			if (!requested || requested.includes("/")) {
				return { reason: `invalid skill dependency: ${String(item)}` };
			}
			if (seen.has(requested)) return { reason: `repeats skill dependency: ${requested}` };
			seen.add(requested);
			preload.push(requested);
		}
	}

	return { parsed: { name, description, model, reasoningEffort, maxTokens, preload }, body };
}

// ---- discovery ----------------------------------------------------------------

function loadRolesFromDir(dir: string, origin: RoleOrigin, out: Map<string, ResolvedRole>, invalid: InvalidRole[], ctx: RoleSearchContext): void {
	let entries: string[];
	try {
		entries = readdirSync(dir);
	} catch {
		return;
	}
	for (const entry of entries.sort()) {
		const roleDir = join(dir, entry);
		const roleFile = join(roleDir, "SKILL.md");
		try {
			if (!statSync(roleDir).isDirectory() || !statSync(roleFile).isFile()) continue;
		} catch {
			continue;
		}
		let content: string;
		try {
			content = readFileSync(roleFile, "utf8");
		} catch {
			continue;
		}
		const parsed = parseRoleContent(content, entry, origin);
		if ("reason" in parsed) {
			invalid.push({ name: entry, origin, reason: parsed.reason });
			continue;
		}
		// First origin in precedence order wins (intentional shadowing).
		if (out.has(parsed.parsed.name)) continue;
		const resolved = finishRole(parsed.parsed, parsed.body, roleFile, origin, ctx, invalid);
		if (resolved) out.set(parsed.parsed.name, resolved);
	}
}

function loadRolesFromHarness(
	harnessDir: string,
	scope: "local" | "global",
	origin: RoleOrigin,
	out: Map<string, ResolvedRole>,
	invalid: InvalidRole[],
	ctx: RoleSearchContext,
): void {
	const state = loadHarnessState(harnessDir, scope);
	for (const entry of Object.values(state.entries.role)) {
		const parsed = parseRoleContent(entry.content, entry.id, origin);
		if ("reason" in parsed) {
			invalid.push({ name: entry.id, origin, reason: parsed.reason });
			continue;
		}
		if (out.has(parsed.parsed.name)) continue;
		const resolved = finishRole(parsed.parsed, parsed.body, `harness:${scope}:${entry.id}`, origin, ctx, invalid);
		if (resolved) out.set(parsed.parsed.name, resolved);
	}
}

/** Resolve preload skills against the harness skill set (pi-relay requires
 * HomeGlobal origin; the new-stack global skill set is bundled skills/ +
 * <agentDir>/skills/ — documented in M7-ROLES.md). Missing/unresolvable
 * preloads make the role unusable (pi-relay hard-error doctrine). */
function finishRole(
	parsed: ParsedRoleFrontmatter,
	body: string,
	filePath: string,
	origin: RoleOrigin,
	ctx: RoleSearchContext,
	invalid: InvalidRole[],
): ResolvedRole | undefined {
	const preload: ResolvedPreloadedSkill[] = [];
	if (parsed.preload.length > 0) {
		const skills = loadHarnessSkills(ctx.bundledSkillsDir, ctx.agentDir);
		const byName = new Map(skills.map((s) => [s.name, s]));
		for (const requested of parsed.preload) {
			const skill = byName.get(requested);
			if (!skill) {
				invalid.push({ name: parsed.name, origin, reason: `requires unavailable skill: ${requested}` });
				return undefined;
			}
			let content: string;
			try {
				content = parseFrontmatter(readFileSync(skill.filePath, "utf8")).body;
			} catch {
				invalid.push({ name: parsed.name, origin, reason: `skill ${requested} unreadable` });
				return undefined;
			}
			preload.push({
				name: skill.name,
				filePath: skill.filePath,
				content,
				pythonImport: skill.pythonImport,
				pythonPath: skill.pythonPath,
			});
		}
	}
	return {
		name: parsed.name,
		description: parsed.description,
		filePath,
		body,
		model: parsed.model,
		reasoningEffort: parsed.reasoningEffort,
		maxTokens: parsed.maxTokens,
		preload,
		origin,
	};
}

export function discoverRoles(ctx: RoleSearchContext): RoleDiscovery {
	const out = new Map<string, ResolvedRole>();
	const invalid: InvalidRole[] = [];
	// Precedence order: harness-local > project-dir > harness-global > global-dir.
	if (ctx.sessionDir && ctx.sessionId) {
		loadRolesFromHarness(join(ctx.sessionDir, "prime", ctx.sessionId, HARNESS_STATE_DIR_NAME), "local", "harness-local", out, invalid, ctx);
	}
	loadRolesFromDir(join(ctx.cwd, ".pi", "roles"), "project-dir", out, invalid, ctx);
	loadRolesFromHarness(join(ctx.agentDir, HARNESS_STATE_DIR_NAME), "global", "harness-global", out, invalid, ctx);
	loadRolesFromDir(join(ctx.agentDir, "roles"), "global-dir", out, invalid, ctx);
	const roles = [...out.values()].sort((a, b) => a.name.localeCompare(b.name));
	return { roles, invalid };
}

/** pi-relay resolve_skill_role: unknown name errors with the known list. */
export function resolveRoleByName(name: string, ctx: RoleSearchContext): ResolvedRole {
	const discovery = discoverRoles(ctx);
	const role = discovery.roles.find((r) => r.name === name);
	if (role) return role;
	const invalidMatch = discovery.invalid.find((r) => r.name === name);
	if (invalidMatch) {
		throw new Error(`role "${name}" is unusable (${invalidMatch.origin}): ${invalidMatch.reason}`);
	}
	const known = discovery.roles.map((r) => r.name).join(", ") || "(none)";
	throw new Error(`role not found: "${name}". Known roles: ${known}`);
}

// ---- prompt rendering ----------------------------------------------------------

/** Port of pi-relay subagent_role_catalog_json (agent-prompt/src/lib.rs):
 * {"subagent_roles": [{name, description}]} pretty-printed, sorted by name. */
export function roleCatalogJson(roles: ResolvedRole[]): string {
	if (roles.length === 0) return "";
	const catalog = roles.map((role) => ({ name: role.name, description: role.description }));
	return JSON.stringify({ subagent_roles: catalog }, null, 2);
}

/** PI.md "### Packaged subagent roles" section (port), gated by the caller on
 * non-empty catalog + delegation capability (pi-relay gates on the parent
 * profile; the new stack gates on depth < maxDepth — documented in M7-ROLES.md). */
export function formatRoleCatalogPromptSection(roles: ResolvedRole[]): string {
	const catalog = roleCatalogJson(roles);
	if (!catalog) return "";
	return [
		"### Packaged subagent roles",
		"",
		'Available subagent roles you can pass as `role="..."` to `rlm(...)` when spawning sub-agents:',
		"",
		"```json",
		catalog,
		"```",
	].join("\n");
}
