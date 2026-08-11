// Project/global context files (AGENTS.md) for the system prompt.
//
// Closes the gap where prime-rlm's wholesale base-prompt replace discards
// upstream pi's contextFiles (and PA never re-added them). Semantics: the
// agent-dir AGENTS.md is global (pi-relay InstructionScope::Global); project
// context is the AGENTS.md chain walking cwd upward, BOUNDED at the first git
// root (monorepo-safe) or $HOME (strays in /tmp etc. are never pulled).
// pi-relay's managed per-project files (~/.agents/projects/<name>/AGENTS.md)
// and workspace-scope instruction files arrive with the bridge's projects +
// multi-dir workspaces in M8.
// Caps per-file and total bytes to keep context cost bounded.

import { existsSync, readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join, resolve } from "node:path";

const MAX_FILE_BYTES = 32 * 1024;
const MAX_TOTAL_BYTES = 96 * 1024;

export interface ContextFile {
	path: string;
	content: string;
}

function readCapped(path: string, budget: number): string | undefined {
	try {
		if (!statSync(path).isFile()) return undefined;
		const raw = readFileSync(path, "utf8");
		if (raw.trim().length === 0) return undefined;
		return raw.length > budget ? `${raw.slice(0, budget)}\n\n[...truncated: file exceeds ${MAX_FILE_BYTES} bytes]` : raw;
	} catch {
		return undefined;
	}
}

export function loadContextFiles(agentDir: string, cwd: string): ContextFile[] {
	const out: ContextFile[] = [];
	const seen = new Set<string>();
	let total = 0;
	const push = (path: string): void => {
		const real = resolve(path);
		if (seen.has(real) || total >= MAX_TOTAL_BYTES) return;
		const content = readCapped(real, Math.min(MAX_FILE_BYTES, MAX_TOTAL_BYTES - total));
		if (content === undefined) return;
		seen.add(real);
		total += content.length;
		out.push({ path: real, content });
	};
	// Global first (pi-relay InstructionScope::Global → HomeGlobal).
	push(join(agentDir, "AGENTS.md"));
	// Project chain: walk cwd upward, but STOP at the first boundary —
	// git repo root (monorepo chains end there), $HOME (user-global is the
	// agent-dir file's job), or filesystem root. This avoids pulling stray
	// AGENTS.md files from unrelated ancestor dirs (e.g. /tmp). Root-most first.
	const chain: string[] = [];
	const home = homedir();
	let dir = resolve(cwd);
	for (;;) {
		const candidate = join(dir, "AGENTS.md");
		if (existsSync(candidate)) chain.push(candidate);
		if (dir === home || existsSync(join(dir, ".git"))) break;
		const parent = dirname(dir);
		if (parent === dir) break;
		dir = parent;
	}
	for (const p of chain.reverse()) push(p);
	// M8: pi-relay multi-dir workspaces. The bridge sets PI_RELAY_WORKSPACE_DIRS
	// (comma-separated, declared order) at host spawn; each dir's AGENTS.md is a
	// workspace-scope instruction file under the session cwd. The upward walk
	// above never sees these (they are BELOW cwd), so load them explicitly,
	// most-specific last.
	const wsDirs = (process.env.PI_RELAY_WORKSPACE_DIRS ?? "")
		.split(",")
		.map((d) => d.trim())
		.filter((d) => d !== "" && !d.includes("/") && !d.includes("\\") && d !== "." && d !== "..");
	for (const d of wsDirs) push(join(resolve(cwd), d, "AGENTS.md"));
	return out;
}

/** Renders like upstream's contextFiles: path header + fenced content. */
export function formatContextFilesForPrompt(files: ContextFile[]): string {
	if (files.length === 0) return "";
	const lines = ["", "", "# Project context", "", "Instructions from AGENTS.md files (global agent dir first, then project files root-most to most-specific). These are operator-authored standing instructions; follow them."];
	for (const f of files) {
		lines.push("", `## ${f.path}`, "", f.content.trimEnd());
	}
	return lines.join("\n");
}
