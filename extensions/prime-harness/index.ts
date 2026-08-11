// prime-harness — M2 milestone extension for unpatched upstream pi.
//
// Continual harness for pi sessions powered by prime-rlm kernels:
//   (a) in-kernel `rlm.harness` CRUD API (memories, prompt notes, skills,
//       subagent specs; local vs global scope) — python port of prime-agent's
//       rlm/harness.py, loaded via the prime-rlm bootstrap seam
//   (b) markdown + python_import skills, rendered into the system prompt and
//       pre-imported into the kernel
//   (c) system-prompt assembly: harness section appended to the incoming prompt
//       in before_agent_start (chains after prime-rlm's RLM prompt); state files
//       are re-read on EVERY build so post-compaction reinjection is automatic
//   (d) full /refine (PA parity): two-phase pipeline — background LLM planning
//       (never blocks the conversation) + fast apply at the turn boundary
//       (agent_settled / immediate when idle); kernel `refine.run/status`;
//       auto-refine (turn_interval + compact triggers behind a review gate);
//       rollback by id; global refinements.jsonl evidence log.
//       See src/refine-engine.ts for the seam adaptation notes.
//
// Requires prime-rlm for the kernel side; without it, the prompt section and
// /refine still work (pure TS) but rlm.harness is absent from kernels.
// Load order: prime-rlm BEFORE prime-harness (its system prompt is the base).

import { appendFileSync, mkdirSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import { _bindHarnessApi } from "./src/host-api.ts";
import { globalHarnessDir, localHarnessDir, sessionStateDir } from "./src/paths.ts";
import { discoverRoles, formatRoleCatalogPromptSection } from "./src/roles.ts";
import { loadHarnessSkills } from "./src/skills.ts";
import {
	formatHarnessStateForPrompt,
	loadHarnessState,
	mergeHarnessStates,
	type RefineOptions,
} from "./src/store.ts";
import { formatSkillsForPrompt } from "./src/skills.ts";
import { formatContextFilesForPrompt, loadContextFiles } from "./src/context-files.ts";
import {
	engineStatus,
	getEngine,
	onAgentSettled,
	onBeforeCompact,
	onTurnEnd,
	refineNow,
	registerEngine,
	requestRefine,
	waitForRefineApplyIdle,
} from "./src/refine-engine.ts";

const moduleDir = dirname(fileURLToPath(import.meta.url));
const BUNDLED_SKILLS_DIR = join(moduleDir, "skills");
const RUNTIME_SRC_DIR = join(moduleDir, "python", "prime-harness-runtime", "src");

function agentDir(): string {
	return process.env.PI_CODING_AGENT_DIR ?? join(homedir(), ".pi", "agent");
}

// ---- prime-rlm seam (structural type; resolved lazily via globalThis) -------

interface PrimeRlmSessionInfo {
	sessionId: string;
	sessionDir: string;
	cwd: string;
	agentDir: string;
	depth: number;
}
type HostRequestHandler = (payload: unknown) => Promise<unknown>;
interface PrimeRlmHostApi {
	version: number;
	registerKernelBootstrapContributor(
		fn: (info: PrimeRlmSessionInfo) => { python?: string; env?: Record<string, string> } | undefined,
	): void;
	registerHostHandlerProvider(
		fn: (info: PrimeRlmSessionInfo) => Record<string, HostRequestHandler> | undefined,
	): void;
	sessionInfo(sessionId: string): PrimeRlmSessionInfo | undefined;
}

/** M7: a session may delegate (spawn children) when its depth is below the rlm
 * recursion cap. The role catalog is only shown then — pi-relay gates the
 * catalog on the parent prompt profile; depth < maxDepth is the new-stack
 * equivalent (documented in M7-ROLES.md). */
function canDelegate(sessionId: string): boolean {
	const maxDepth = Number(process.env.RLM_MAX_DEPTH ?? "1");
	const api = getPrimeRlmApi();
	if (!api) return false; // no rlm(): roles are unusable, hide the catalog
	const depth = api.sessionInfo(sessionId)?.depth ?? 0;
	return depth < maxDepth;
}
function getPrimeRlmApi(): PrimeRlmHostApi | undefined {
	const candidate = (globalThis as Record<symbol, unknown>)[Symbol.for("prime-rlm.host-api")];
	if (candidate && (candidate as { version?: unknown }).version === 1) {
		 return candidate as PrimeRlmHostApi;
	}
	return undefined;
}

// ---- kernel bootstrap contribution ------------------------------------------

function pyStr(value: string): string {
	// JSON.stringify output is a valid Python string literal for our paths.
	return JSON.stringify(value);
}

let contributorRegistered = false;

function registerKernelContribution(): void {
	if (contributorRegistered) return;
	const api = getPrimeRlmApi();
	if (!api) return; // prime-rlm not loaded: prompt section + /refine still work
	contributorRegistered = true;

	// Kernel host handlers for the refine skill (refine.run / refine.status).
	// Resolved per-session at call time from the module-scope engine map.
	api.registerHostHandlerProvider((info) => ({
		"refine.run": async (payload) => {
			const engine = getEngine(info.sessionId);
			if (!engine) throw new Error(`prime-harness: no refine engine bound for session ${info.sessionId}`);
			const record = (payload ?? {}) as Record<string, unknown>;
			const options: RefineOptions = {};
			if (typeof record.instructions === "string") options.instructions = record.instructions;
			if (record.global === true) options.global = true;
			return requestRefine(engine, options);
		},
		"refine.status": async () => {
			const engine = getEngine(info.sessionId);
			if (!engine) return { pending: false, in_flight: false, error: "no engine bound" };
			return engineStatus(engine);
		},
	}));
	api.registerKernelBootstrapContributor((info) => {
		const perSession = join(info.sessionDir, "prime", info.sessionId);
		const localDir = join(perSession, "harness");
		const globalDir = globalHarnessDir(info.agentDir || agentDir());
		const skills = loadHarnessSkills(BUNDLED_SKILLS_DIR, info.agentDir || agentDir());
		const pythonSkills = skills.filter((s) => s.pythonImport && s.pythonPath);
		const skillImports = pythonSkills
			.map(
				(s) =>
					`    (${pyStr(s.pythonPath!)}, ${pyStr(s.pythonImport!)}),`,
			)
			.join("\n");
		const python = `
import sys as _ph_sys

_PH_RUNTIME_SRC = ${pyStr(RUNTIME_SRC_DIR)}
if _PH_RUNTIME_SRC not in _ph_sys.path:
    _ph_sys.path.insert(0, _PH_RUNTIME_SRC)

import prime_harness_runtime as _phr

rlm.harness = _phr.harness
rlm.get_harness_state = _phr.get_harness_state

_PH_SKILL_ERRORS = {}
for _ph_src, _ph_mod in [
${skillImports}
]:
    if _ph_src not in _ph_sys.path:
        _ph_sys.path.insert(0, _ph_src)
    try:
        globals()[_ph_mod] = __import__(_ph_mod)
    except Exception as _ph_e:
        _PH_SKILL_ERRORS[_ph_mod] = str(_ph_e)

if _PH_SKILL_ERRORS:
    print("[prime-harness] python skill import errors:", _PH_SKILL_ERRORS)
`.trim();
		return {
			python,
			env: {
				RLM_SESSION_DIR: perSession,
				RLM_HARNESS_STATE_DIR: localDir,
				RLM_GLOBAL_HARNESS_STATE_DIR: globalDir,
				PRIME_AGENT_CODING_AGENT_DIR: info.agentDir || agentDir(),
			},
		};
	});
}

// ---- prompt assembly ----------------------------------------------------------

function logPromptBuild(ctx: ExtensionContext, record: Record<string, unknown>): void {
	try {
		const dir = sessionStateDir(ctx);
		mkdirSync(dir, { recursive: true });
		appendFileSync(
			join(dir, "prompt-builds.jsonl"),
			`${JSON.stringify({ ts: new Date().toISOString(), ...record })}\n`,
			"utf8",
		);
	} catch {
		// debug logging must never break prompt assembly
	}
}

export default function primeHarness(pi: ExtensionAPI): void {
	registerKernelContribution();
	// M7: publish the prime-harness.host-api seam (role resolution for
	// prime-rlm's rlm.run(role=...)).
	_bindHarnessApi(BUNDLED_SKILLS_DIR, agentDir);

	pi.on("session_start", (_event, ctx) => {
		const engine = registerEngine(ctx.sessionManager.getSessionId(), pi, agentDir());
		engine.ctx = ctx;
	});

	pi.on("turn_end", async (_event, ctx) => {
		const engine = getEngine(ctx.sessionManager.getSessionId());
		if (!engine) return;
		// Auto-refine review runs in the background; never block turn dispatch.
		void onTurnEnd(engine, ctx);
	});

	pi.on("session_before_compact", (_event, ctx) => {
		const engine = getEngine(ctx.sessionManager.getSessionId());
		if (engine) onBeforeCompact(engine);
		return undefined;
	});

	pi.on("agent_settled", async (_event, ctx) => {
		const engine = getEngine(ctx.sessionManager.getSessionId());
		if (!engine) return;
		// Apply boundary: pending plans apply at post-run quiescence (fast).
		await onAgentSettled(engine, ctx);
	});

	pi.on("before_agent_start", async (event, ctx) => {
		// PA's _waitForRefineIdle: turn entry waits only on the (fast) apply phase.
		const engine = getEngine(ctx.sessionManager.getSessionId());
		if (engine) await waitForRefineApplyIdle(engine);

		const gDir = globalHarnessDir(agentDir());
		const lDir = localHarnessDir(ctx);
		const globalState = loadHarnessState(gDir, "global");
		const localState = loadHarnessState(lDir, "local");
		const merged = mergeHarnessStates(globalState, localState);
		const skills = loadHarnessSkills(BUNDLED_SKILLS_DIR, agentDir());

		const harnessSection = formatHarnessStateForPrompt(merged);
		const skillsSection = formatSkillsForPrompt(skills);
		// AGENTS.md parity (was dropped by prime-rlm's wholesale base-prompt replace).
		const contextSection = formatContextFilesForPrompt(loadContextFiles(agentDir(), ctx.cwd));
		// M7: pi-relay PI.md "### Packaged subagent roles" section (port). Shown
		// only when the session can delegate (depth < maxDepth) and valid roles
		// exist; invalid roles stay out of the catalog (pi-relay doctrine) and
		// land in prompt-builds.jsonl diagnostics instead.
		const sessionId = ctx.sessionManager.getSessionId();
		const roleDiscovery = discoverRoles({
			agentDir: agentDir(),
			cwd: ctx.cwd,
			bundledSkillsDir: BUNDLED_SKILLS_DIR,
			sessionDir: ctx.sessionManager.getSessionDir(),
			sessionId,
		});
		const rolesSection = canDelegate(sessionId)
			? formatRoleCatalogPromptSection(roleDiscovery.roles)
			: "";
		const counts = Object.fromEntries(
			Object.entries(merged.entries).map(([kind, records]) => [kind, Object.keys(records).length]),
		);
		logPromptBuild(ctx, {
			sessionId,
			localDir: lDir,
			globalDir: gDir,
			counts,
			skills: skills.map((s) => s.name),
			roles: roleDiscovery.roles.map((r) => r.name),
			invalidRoles: roleDiscovery.invalid,
			sectionChars: harnessSection.length + skillsSection.length + rolesSection.length,
		});

		return {
			systemPrompt: `${event.systemPrompt}\n\n${harnessSection}${rolesSection ? `${rolesSection}\n\n` : ""}${skillsSection}${contextSection}`,
		};
	});

	pi.registerCommand("refine", {
		description:
			"Refine the continual harness (prompt notes, memories, skills, subagent specs) from the recent trajectory. Usage: /refine [--global] [--rollback <id>] [instructions]",
		handler: async (args, ctx) => {
			const tokens = args.trim().split(/\s+/).filter(Boolean);
			const options: RefineOptions = {};
			const instructionParts: string[] = [];
			for (let i = 0; i < tokens.length; i++) {
				const token = tokens[i];
				if (token === "--global") options.global = true;
				else if (token === "--rollback") {
					const id = tokens[++i];
					if (!id) throw new Error("/refine --rollback requires a refinement id");
					options.rollbackId = id;
				} else instructionParts.push(token);
			}
			if (instructionParts.length > 0) options.instructions = instructionParts.join(" ");

			const engine = registerEngine(ctx.sessionManager.getSessionId(), pi, agentDir());
			engine.ctx = ctx;

			if (!ctx.isIdle()) {
				// Mid-turn /refine: queue like PA's session-input pump — planning runs
				// in the background and the apply lands at the next settle boundary.
				const scheduled = requestRefine(engine, options);
				ctx.ui.notify(
					scheduled.scheduled
						? "Refine scheduled: planning in the background; applies at the next turn boundary."
						: `Refine not scheduled: ${scheduled.reason}`,
					scheduled.scheduled ? "info" : "warning",
				);
				return;
			}

			ctx.ui.notify("Refine: planning (background LLM pass)…", "info");
			const result = await refineNow(engine, ctx, options);
			const applied = result.appliedEdits.filter((e) => e.applied).length;
			const failed = result.appliedEdits.length - applied;
			ctx.ui.notify(
				`Refinement ${result.id} (${result.scope ?? "local"}): ${result.summary} — ${applied} edit(s) applied${failed > 0 ? `, ${failed} failed` : ""}`,
				failed > 0 ? "warning" : "info",
			);
		},
	});
}
