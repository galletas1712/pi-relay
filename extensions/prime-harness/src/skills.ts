// Minimal skill loader for prime-harness (M2).
//
// PROVENANCE: detection rules + prompt format ported from prime-agent's
//   packages/coding-agent/src/core/skills.ts
// (loadSkillsFromDir / detectPythonSkill / formatSkillsForPrompt).
// M2 simplifications: only directory-form skills with SKILL.md; python skills
// detected by pyproject.toml + src/<underscored-name>/__init__.py (PA rule);
// python packages are sys.path-inserted into the kernel at bootstrap (no venv
// install). Skill sources: the extension's bundled skills/ dir plus
// <agentDir>/skills/ (user-global, PA-compatible location).

import { existsSync, readdirSync, readFileSync, statSync } from "node:fs";
import { join } from "node:path";
import { parseFrontmatter } from "@earendil-works/pi-coding-agent";

export interface LoadedSkill {
	name: string;
	description: string;
	/** Absolute path of the SKILL.md. */
	filePath: string;
	/** Skill directory (parent of SKILL.md). */
	baseDir: string;
	/** Python import name when this is a python-backed skill. */
	pythonImport?: string;
	/** Directory to put on sys.path for the python import (skillDir/src). */
	pythonPath?: string;
}

function pythonImportNameForSkill(name: string): string {
	return name.replaceAll("-", "_");
}

function isValidPythonImportName(name: string): boolean {
	return /^[A-Za-z_][A-Za-z0-9_]*$/.test(name);
}

/** PA detectPythonSkill: pyproject.toml + src/<importName>/__init__.py. */
function detectPython(skillDir: string, name: string): { importName: string; path: string } | undefined {
	if (!existsSync(join(skillDir, "pyproject.toml"))) return undefined;
	const importName = pythonImportNameForSkill(name);
	if (!isValidPythonImportName(importName)) return undefined;
	const packageInit = join(skillDir, "src", importName, "__init__.py");
	try {
		if (!statSync(packageInit).isFile()) return undefined;
	} catch {
		return undefined;
	}
	return { importName, path: join(skillDir, "src") };
}

function loadSkillsFromDir(dir: string, out: Map<string, LoadedSkill>): void {
	let entries: string[];
	try {
		entries = readdirSync(dir);
	} catch {
		return;
	}
	for (const entry of entries.sort()) {
		const skillDir = join(dir, entry);
		const skillFile = join(skillDir, "SKILL.md");
		try {
			if (!statSync(skillDir).isDirectory() || !statSync(skillFile).isFile()) continue;
		} catch {
			continue;
		}
		let content: string;
		try {
			content = readFileSync(skillFile, "utf8");
		} catch {
			continue;
		}
		const { frontmatter } = parseFrontmatter(content);
		const name = typeof frontmatter.name === "string" && frontmatter.name ? frontmatter.name : entry;
		const description = typeof frontmatter.description === "string" ? frontmatter.description : "";
		const python = detectPython(skillDir, name);
		// First source wins (bundled skills/ before agentDir/skills/).
		if (!out.has(name)) {
			out.set(name, {
				name,
				description,
				filePath: skillFile,
				baseDir: skillDir,
				pythonImport: python?.importName,
				pythonPath: python?.path,
			});
		}
	}
}

/** M8: PRIME_HARNESS_EXTRA_SKILLS_DIRS (comma-separated) adds lowest-precedence
 * skill dirs. The pi-relay bridge points this at the session's generated MCP
 * skill dir (<stateRoot>/sessions/<id>/mcp/skills). */
export function extraSkillsDirsFromEnv(env: NodeJS.ProcessEnv = process.env): string[] {
	return (env.PRIME_HARNESS_EXTRA_SKILLS_DIRS ?? "")
		.split(",")
		.map((d) => d.trim())
		.filter((d) => d.length > 0);
}

export function loadHarnessSkills(bundledSkillsDir: string, agentDir: string): LoadedSkill[] {
	const skills = new Map<string, LoadedSkill>();
	loadSkillsFromDir(bundledSkillsDir, skills);
	loadSkillsFromDir(join(agentDir, "skills"), skills);
	for (const dir of extraSkillsDirsFromEnv()) loadSkillsFromDir(dir, skills);
	return [...skills.values()];
}

function escapeXml(str: string): string {
	return str
		.replace(/&/g, "&amp;")
		.replace(/</g, "&lt;")
		.replace(/>/g, "&gt;")
		.replace(/"/g, "&quot;")
		.replace(/'/g, "&apos;");
}

const EDIT_SKILL_HINT =
	"For targeted existing-file edits, prefer the pre-imported async `edit` skill from IPython: `old = '''...'''; new = '''...'''; await edit(path=\"pkg/file.py\", old_str=old, new_str=new)`. Use exact old/new strings; if the text contains triple double quotes, use triple single-quoted variables or build `old`/`new` from inspected file slices.";

/** formatSkillsForPrompt: ported from prime-agent skills.ts (agentskills.io XML). */
export function formatSkillsForPrompt(skills: LoadedSkill[]): string {
	if (skills.length === 0) return "";
	const lines = [
		"\n\nThe following skills provide specialized instructions for specific tasks.",
		"Use ipython to inspect a skill's file when the task matches its description.",
		"Skills with a python_import are prepared in the persistent IPython kernel when available and can be called directly by that import name.",
		"When a skill file references a relative path, resolve it against the skill directory (parent of SKILL.md / dirname of the path) and use that absolute path in tool commands.",
	];
	const preImported = skills.filter((s) => s.pythonImport).map((s) => "`" + s.pythonImport + "`");
	if (preImported.length > 0) {
		// PA rlm.ts prompt-parity lines (restored 2026-08-09):
		lines.push("Installed Python skill modules (pre-imported): " + preImported.join(", ") + ".");
		lines.push(
			"Read each skill's SKILL.md for its API. Inspect a module with `help(<skill>)` or `dir(<skill>)`, then inspect a documented callable with `inspect.signature(<skill>.<function>)`.",
		);
		lines.push("Each skill is also available as a shell command by the same name: `<skill> ...`. Discover its CLI usage with `<skill> --help`.");
		if (skills.some((s) => s.pythonImport === "edit")) {
			lines.push(EDIT_SKILL_HINT);
		}
	}
	lines.push("", "<available_skills>");
	for (const skill of skills) {
		lines.push("  <skill>");
		lines.push(`    <name>${escapeXml(skill.name)}</name>`);
		lines.push(`    <type>${skill.pythonImport ? "python" : "markdown"}</type>`);
		if (skill.pythonImport) {
			lines.push(`    <python_import>${escapeXml(skill.pythonImport)}</python_import>`);
		}
		lines.push(`    <description>${escapeXml(skill.description)}</description>`);
		lines.push(`    <location>${escapeXml(skill.filePath)}</location>`);
		lines.push("  </skill>");
	}
	lines.push("</available_skills>");
	return lines.join("\n");
}
