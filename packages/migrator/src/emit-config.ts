// Config-plane emitters: roles, skills, prompt scopes, harness state, mcp.toml.
// Sources are READ-ONLY copies from the live pi-relay layout:
//   $runtimeConfigRoot/subagent-roles/<n>/SKILL.md   -> agent/roles/<n>/SKILL.md   (M7 global-dir origin)
//   $runtimeConfigRoot/skills/<n>/SKILL.md           -> agent/skills/<n>/...       (RuntimeWorkflow -> user-global)
//   $homeAgents/skills/<n>/                          -> agent/skills/<n>/...       (HomeGlobal, PA-compatible)
//   $homeAgents/projects/<k>/skills/<n>/             -> agent/projects/<k>/skills/<n>/  (staging; new-stack home = <ws>/.pi/skills)
//   $homeAgents/AGENTS.md                            -> agent/AGENTS.md            (InstructionScope::Global)
//   $homeAgents/projects/<k>/AGENTS.md               -> agent/projects/<k>/AGENTS.md    (scope Project)
//   $runtimeConfigRoot/mcp.toml                      -> mcp.toml (normalized, bridge parseMcpConfig-compatible)
// harness_state.json: schema:1, one prompt entry documenting the migration (provenance).
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { parse as parseToml, stringify as stringifyToml } from "smol-toml";
import type { MigratorConfig } from "./config.ts";
import { writeIfChanged } from "./sessions.ts";

export interface EmitResult {
	changed: number;
	unchanged: number;
	files: string[];
	notes: string[];
}

function newRes(): EmitResult {
	return { changed: 0, unchanged: 0, files: [], notes: [] };
}

function put(res: EmitResult, path: string, content: string): void {
	res.files.push(path);
	if (writeIfChanged(path, content)) res.changed += 1;
	else res.unchanged += 1;
}

function copyTree(src: string, dst: string, res: EmitResult): void {
	if (!existsSync(src)) return;
	for (const name of readdirSync(src).sort()) {
		const s = join(src, name);
		const d = join(dst, name);
		if (statSync(s).isDirectory()) copyTree(s, d, res);
		else put(res, d, readFileSync(s, "utf8"));
	}
}

export function emitRolesSkillsScopes(cfg: MigratorConfig): EmitResult {
	const res = newRes();
	const agentDir = join(cfg.outRoot, "agent");
	mkdirSync(agentDir, { recursive: true });

	// roles (RuntimeRole)
	copyTree(join(cfg.runtimeConfigRoot, "subagent-roles"), join(agentDir, "roles"), res);
	// workflow skills (RuntimeWorkflow) + home-global skills (HomeGlobal)
	copyTree(join(cfg.runtimeConfigRoot, "skills"), join(agentDir, "skills"), res);
	copyTree(join(cfg.homeAgentsRoot, "skills"), join(agentDir, "skills"), res);
	// global prompt scope
	const globalAgents = join(cfg.homeAgentsRoot, "AGENTS.md");
	if (existsSync(globalAgents)) put(res, join(agentDir, "AGENTS.md"), readFileSync(globalAgents, "utf8"));
	// project scopes (AGENTS.md + skills), staging layout
	const projectsDir = join(cfg.homeAgentsRoot, "projects");
	if (existsSync(projectsDir)) {
		for (const key of readdirSync(projectsDir).sort()) {
			const pdir = join(projectsDir, key);
			if (!statSync(pdir).isDirectory()) continue;
			const agents = join(pdir, "AGENTS.md");
			if (existsSync(agents)) put(res, join(agentDir, "projects", key, "AGENTS.md"), readFileSync(agents, "utf8"));
			copyTree(join(pdir, "skills"), join(agentDir, "projects", key, "skills"), res);
		}
	}
	return res;
}

export function emitHarnessState(cfg: MigratorConfig, dumpLabel: string, sessionCount: number): EmitResult {
	const res = newRes();
	const harnessDir = join(cfg.outRoot, "agent", "harness");
	mkdirSync(harnessDir, { recursive: true });
	const now = `2026-08-09T11:27:41.000Z`; // dump capture time (backup label), deterministic
	const state = {
		schema: 1,
		entries: {
			prompt: {
				m10a_migration_provenance: {
					id: "m10a_migration_provenance",
					kind: "prompt",
					title: "M10a migration provenance",
					content:
						`This agent directory was produced by packages/migrator (M10a) from the pi-relay dump ` +
						`${dumpLabel} (${sessionCount} sessions). Roles/skills/AGENTS.md are byte-copies of the ` +
						`pi-relay runtime config; project-scoped skills are staged under agent/projects/<key>/ ` +
						`for placement into workspace .pi/skills at M11.`,
					path: "policy",
					scope: "global",
					reference: {},
					arguments: {},
					metadata: { milestone: "M10a", dump: dumpLabel },
					source: "agent",
					created_at: now,
					updated_at: now,
					version: 1,
				},
			},
			memory: {},
			skill: {},
			subagent: {},
			role: {},
		},
		refinements: [],
	};
	put(res, join(harnessDir, "harness_state.json"), JSON.stringify(state, null, 1) + "\n");
	return res;
}

/** Normalize the live pi-relay mcp.toml into bridge-compatible canonical form.
 * The bridge parser (packages/bridge/src/mcp/config.ts) is a port of pi-relay's
 * config.rs and accepts the same wire shapes; we re-emit multi-line tables. */
export function emitMcpToml(cfg: MigratorConfig): EmitResult & { servers: string[]; fingerprintInput: unknown } {
	const res = newRes() as EmitResult & { servers: string[]; fingerprintInput: unknown };
	res.servers = [];
	const src = join(cfg.runtimeConfigRoot, "mcp.toml");
	if (!existsSync(src)) {
		res.notes.push("no mcp.toml at runtime config root; skipped");
		res.fingerprintInput = null;
		return res;
	}
	const parsed = parseToml(readFileSync(src, "utf8")) as Record<string, unknown>;
	const servers = (parsed.servers ?? {}) as Record<string, unknown>;
	const out: Record<string, unknown> = { servers: {} };
	for (const [id, def] of Object.entries(servers).sort(([a], [b]) => (a < b ? -1 : 1))) {
		(out.servers as Record<string, unknown>)[id] = def;
		res.servers.push(id);
	}
	res.fingerprintInput = out;
	put(res, join(cfg.outRoot, "mcp.toml"), stringifyToml(out) + "\n");
	return res;
}
