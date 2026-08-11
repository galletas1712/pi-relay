// TS port of prime-agent's continual harness state layer (M2).
//
// PROVENANCE: ported from prime-agent's
//   packages/coding-agent/src/core/refinement/refinement.ts
// (upstream repo: repos/prime-agent). Ported pieces: HarnessState schema,
// load/save/merge, formatHarnessStateForPrompt, proposal parse/validate/apply,
// REFINEMENT_SYSTEM_PROMPT, planRefinement. The harness_state.json file schema
// is identical to prime-agent's so state is interchangeable.
//
// Full /refine parity port (M2, upgraded from minimal): includes
// planRefinement / applyRefinementProposal (with baselineState conflict
// checks), rollbackProposal, reviewAutoRefine + AUTO_REFINE review prompt,
// global refinements.jsonl history (appendGlobalRefinement /
// loadGlobalRefinementHistory / mergeRefinementHistory) — session-local
// history comes from session custom entries ("prime.harness.refinement").
// Remaining M2 simplifications (documented):
//   - no usage attribution / fork patches
//   - the LLM call uses pi-ai's completeSimple with auth resolved by the caller
//   - thinkingLevel param omitted (PA threads it but ignores it for refine)

import { randomUUID } from "node:crypto";
import {
	appendFileSync,
	existsSync,
	mkdirSync,
	readFileSync,
	renameSync,
	statSync,
	unlinkSync,
	writeFileSync,
} from "node:fs";
import { join } from "node:path";
import { convertToLlm, serializeConversation } from "@earendil-works/pi-coding-agent";
import { completeSimple, type Model } from "@earendil-works/pi-ai/compat";

export const REFINEMENT_CUSTOM_TYPE = "prime.harness.refinement";

const DEFAULT_OVERVIEW_ENTRY_LIMIT = 6;
const DEFAULT_OVERVIEW_REFINEMENT_LIMIT = 5;
const DEFAULT_OVERVIEW_CONTENT_LIMIT = 180;

// M7: "role" is a deliberate ADDITIVE schema extension over PA's four kinds
// (pi-relay SkillKind::SubagentRole as harness content). Both loaders (this TS
// port and PA's rlm/harness.py) tolerate a missing/unknown kind key, so state
// files stay interchangeable in both directions. Role entry content is the full
// SKILL.md role document; the entry id must equal the frontmatter name
// (pi-relay's directory-name doctrine).
export type RefinementKind = "prompt" | "memory" | "skill" | "subagent" | "role";
export type RefinementAction = "create" | "update" | "delete";
export type HarnessScope = "local" | "global";

export interface HarnessEntry {
	id: string;
	kind: RefinementKind;
	title: string;
	content: string;
	path: string;
	scope?: HarnessScope;
	reference: Record<string, unknown>;
	arguments: Record<string, unknown>;
	metadata: Record<string, unknown>;
	source: string;
	created_at: string;
	updated_at: string;
	version: number;
}

export interface HarnessRefinementEvent {
	id: string;
	trigger: string;
	changes: string[];
	evidence: string;
	outcome: string;
	created_at: string;
}

export interface HarnessState {
	schema: number;
	entries: Record<RefinementKind, Record<string, HarnessEntry>>;
	refinements: HarnessRefinementEvent[];
}

export interface RefinementEdit {
	action: RefinementAction;
	kind: RefinementKind;
	id?: string;
	title?: string;
	content?: string;
	path?: string;
	reference?: Record<string, unknown>;
	arguments?: Record<string, unknown>;
	metadata?: Record<string, unknown>;
	reason?: string;
}

export interface RefinementProposal {
	summary: string;
	rationale: string;
	edits: RefinementEdit[];
	expectedOutcome: string;
}

export interface AppliedRefinementEdit extends RefinementEdit {
	id: string;
	before?: HarnessEntry;
	after?: HarnessEntry;
	applied: boolean;
	error?: string;
}

export interface RefinementResult {
	id: string;
	summary: string;
	rationale: string;
	expectedOutcome: string;
	appliedEdits: AppliedRefinementEdit[];
	harnessStatePath: string;
	rollbackOf?: string;
	scope?: HarnessScope;
}

export interface RefineOptions {
	instructions?: string;
	rollbackId?: string;
	global?: boolean;
}

// REFINEMENT_SYSTEM_PROMPT: verbatim port from prime-agent refinement.ts.
const REFINEMENT_SYSTEM_PROMPT = `You are the /refine continual harness subsystem.

Your job is to improve the editable continual harness state from the current trajectory.
This is similar in spirit to context compaction, but instead of summarizing the
conversation you emit precise Create, Update, or Delete edits to reusable state.
The continual harness is the persistent, editable set of prompt notes, memories,
skills, and subagent specs that lets the agent improve reusable behavior
outside the token history.
Use "continual harness" for that persistent artifact layer; keep "RLM" for the
runtime, IPython kernel, and native call interface that executes those artifacts.

Continual harness components:
- prompt: supplemental prompt notes only. The base system prompt is immutable and MUST NOT be rewritten.
- memory: durable facts, decisions, failures, preferences, and outcomes.
- skill: installed Python REPL skill. Skill create/update edits MUST include a \`reference\` object with \`{"type":"python"}\`, a Python import, and a callable or call pattern; they also MUST include an \`arguments\` object describing accepted inputs, required fields, defaults, and constraints. Use \`{}\` for \`arguments\` only when the Python callable truly needs no external inputs. Include the RLM-native call form \`await <skill_import>(...)\`.
- subagent: reusable delegation specs, including purpose, instructions, and when to invoke. Include the RLM-native call form: compose a concise task prompt and spawn with \`handle = await rlm("sub-task")\`; admission returns immediately with \`rlm_child_id\`, \`name\`, \`session_dir\`, and \`model\`, never the child's answer. Results arrive only through explicit \`agent_message\` replies or files; children reply with \`await agent_message.send(message, receiver_role="parent")\`. Use \`await rlm.list_subagents()\` to recover direct child handles and \`await agent_message.send(..., receiver_role="child", receiver_name=handle.name)\` for follow-ups. Do not invent wrappers like \`run_subagent(...)\`.
- role: packaged subagent role documents (SKILL.md format: frontmatter \`name\`, \`description\`, optional \`model\`, \`reasoning_effort\`, \`max_tokens\`, \`skills\` preload list, then the body). Spawn with \`handle = await rlm("sub-task", role="name")\`. The entry id MUST equal the frontmatter \`name\`.

Scope and persistence policy:
- The default editable continual harness store is local to the current session. Use it for session-specific progress, active task state, current-run coordination notes, temporary blockers, and project facts that should not affect other sessions.
- A caller may explicitly request global refinement. Global edits must be stable cross-session lessons, durable user preferences, reusable skills/subagents, or tool/environment facts that should affect future sessions.
- Entry ids in the harness overview may carry a display-only \`local:\` or \`global:\` prefix. Always use the bare id (no prefix) in edits.
- All edits in one refinement apply only to the requested scope's store. During a local refinement, global entries are read-only context: never propose update or delete edits for them; create a local entry instead if an override is needed.
- Project/workspace-specific lessons may be persisted globally only when the title, path, or content explicitly names the project/workspace and the lesson is likely to be reused in future sessions for that project. Prefer local edits when the lesson only belongs in the current conversation.
- Use memory for declarative facts and preferences, skill for repeatable procedures exposed as Python calls, prompt for narrow behavioral policy addendums, and subagent for reusable delegation roles.
- Create or update the smallest relevant component: repeated delegation roles should become subagent specs, repeated procedures should become skills, durable facts/preferences should become memories, and narrow behavioral policies should become prompt addendums.
- When an edit is persisted, include metadata such as \`{"scope":"local"}\` or \`{"scope":"global"}\` when that helps future review understand the intended blast radius.

Use the trajectory, current continual harness state, and prior refinement history. Prefer
small evidence-backed edits. If prior refinements caused issues, rollback or
replace the faulty editable entries. Never edit source files directly. Output
JSON only with this exact shape:

{
  "summary": "one sentence",
  "rationale": "why these edits are justified by trajectory evidence",
  "expectedOutcome": "what should improve and how to validate it",
  "edits": [
    {
      "action": "create|update|delete",
      "kind": "prompt|memory|skill|subagent",
      "id": "stable id for update/delete, optional for create",
      "title": "required for create/update except delete",
      "content": "required for create/update except delete",
      "path": "optional grouping path",
      "reference": {"type": "python", "import": "package.module", "callable": "function_name", "call_pattern": "await function_name(...)"},
      "arguments": {"name": {"type": "string", "required": true, "description": "accepted input"}},
      "metadata": {},
      "reason": "why this edit is useful"
    }
  ]
}`;

const REFINEMENT_MAX_OUTPUT_TOKENS = 32_000;

const TRUNCATED_JSON_ERROR =
	"the model stopped before completing its JSON object. This usually means the output budget was exhausted; retry with a smaller request.";

function refinementMaxOutputTokens(model: Model<any>): number {
	return Math.min(model.maxTokens, REFINEMENT_MAX_OUTPUT_TOKENS);
}

function now(): string {
	return new Date().toISOString();
}

export function emptyHarnessState(): HarnessState {
	return {
		schema: 1,
		entries: {
			prompt: {},
			memory: {},
			skill: {},
			subagent: {},
			role: {},
		},
		refinements: [],
	};
}

function slug(raw: string, fallback: string): string {
	const normalized = raw
		.trim()
		.toLowerCase()
		.replace(/[^a-z0-9]+/g, "_")
		.replace(/^_+|_+$/g, "")
		.slice(0, 80);
	return normalized || fallback;
}

function cloneEntry(entry: HarnessEntry | undefined): HarnessEntry | undefined {
	return entry ? JSON.parse(JSON.stringify(entry)) : undefined;
}

function objectRecord(value: unknown): Record<string, unknown> | undefined {
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		return undefined;
	}
	return value as Record<string, unknown>;
}

function normalizeHarnessScope(value: unknown, fallback: HarnessScope): HarnessScope {
	return value === "global" || value === "local" ? value : fallback;
}

export function getHarnessStatePath(harnessStateDir: string): string {
	return join(harnessStateDir, "harness_state.json");
}

// loadHarnessState: ported from prime-agent. A corrupt/unreadable file degrades
// to empty rather than throwing — this runs on every system-prompt build.
export function loadHarnessState(harnessStateDir: string, scope: HarnessScope): HarnessState {
	const statePath = getHarnessStatePath(harnessStateDir);
	if (!existsSync(statePath)) {
		return emptyHarnessState();
	}
	let parsed: Partial<HarnessState>;
	try {
		const raw = JSON.parse(readFileSync(statePath, "utf8"));
		if (typeof raw !== "object" || raw === null || Array.isArray(raw)) {
			return emptyHarnessState();
		}
		parsed = raw as Partial<HarnessState>;
	} catch {
		return emptyHarnessState();
	}
	const state = emptyHarnessState();
	if (typeof parsed.schema === "number") {
		state.schema = parsed.schema;
	}
	const rawEntries = parsed.entries;
	if (typeof rawEntries === "object" && rawEntries !== null) {
		for (const kind of Object.keys(state.entries) as RefinementKind[]) {
			const rawKind = (rawEntries as Record<string, unknown>)[kind];
			if (typeof rawKind !== "object" || rawKind === null) continue;
			for (const [id, rawEntry] of Object.entries(rawKind as Record<string, unknown>)) {
				if (typeof rawEntry !== "object" || rawEntry === null) continue;
				const record = rawEntry as Record<string, unknown>;
				if (typeof record.title !== "string" || typeof record.content !== "string") continue;
				const version = typeof record.version === "number" ? record.version : 1;
				state.entries[kind][id] = {
					id,
					kind,
					title: record.title,
					content: record.content,
					path: typeof record.path === "string" ? record.path : "general",
					scope: normalizeHarnessScope(record.scope, scope),
					reference: objectRecord(record.reference) ?? {},
					arguments: objectRecord(record.arguments) ?? {},
					metadata: objectRecord(record.metadata) ?? {},
					source: typeof record.source === "string" ? record.source : "agent",
					created_at: typeof record.created_at === "string" ? record.created_at : now(),
					updated_at: typeof record.updated_at === "string" ? record.updated_at : now(),
					version,
				};
			}
		}
	}
	if (Array.isArray(parsed.refinements)) {
		for (const rawEvent of parsed.refinements) {
			if (typeof rawEvent !== "object" || rawEvent === null) continue;
			const record = rawEvent as unknown as Record<string, unknown>;
			if (typeof record.id !== "string" || typeof record.trigger !== "string") continue;
			state.refinements.push({
				id: record.id,
				trigger: record.trigger,
				changes: Array.isArray(record.changes) ? record.changes.map(String) : [],
				evidence: typeof record.evidence === "string" ? record.evidence : "",
				outcome: typeof record.outcome === "string" ? record.outcome : "",
				created_at: typeof record.created_at === "string" ? record.created_at : now(),
			});
		}
	}
	return state;
}

// mergeHarnessStates: ported from prime-agent (global first, local overrides win
// on id collision with a local: prefix).
export function mergeHarnessStates(globalState: HarnessState, localState?: HarnessState): HarnessState {
	const merged = emptyHarnessState();
	merged.schema = Math.max(globalState.schema, localState?.schema ?? 1);
	for (const kind of Object.keys(merged.entries) as RefinementKind[]) {
		for (const [id, entry] of Object.entries(globalState.entries[kind])) {
			const cloned = cloneEntry(entry)!;
			merged.entries[kind][id] = { ...cloned, scope: normalizeHarnessScope(cloned.scope, "global") };
		}
		for (const [id, entry] of Object.entries(localState?.entries[kind] ?? {})) {
			const cloned = cloneEntry(entry)!;
			const scopedEntry = { ...cloned, scope: normalizeHarnessScope(cloned.scope, "local") };
			const mergedId = merged.entries[kind][id] ? `${scopedEntry.scope}:${id}` : id;
			merged.entries[kind][mergedId] = scopedEntry;
		}
	}
	merged.refinements = [...globalState.refinements, ...(localState?.refinements ?? [])];
	return merged;
}

// saveHarnessState: ported from prime-agent (atomic temp-file rename, 0o600 default).
export function saveHarnessState(harnessStateDir: string, state: HarnessState): string {
	const statePath = getHarnessStatePath(harnessStateDir);
	const tempPath = `${statePath}.${process.pid}.${randomUUID()}.tmp`;
	mkdirSync(harnessStateDir, { recursive: true });
	try {
		const mode = existsSync(statePath) ? statSync(statePath).mode & 0o777 : 0o600;
		writeFileSync(tempPath, `${JSON.stringify(state, null, 2)}\n`, { encoding: "utf8", mode });
		renameSync(tempPath, statePath);
	} finally {
		if (existsSync(tempPath)) {
			unlinkSync(tempPath);
		}
	}
	return statePath;
}

function compactText(text: string, maxLength: number): string {
	const normalized = text.replace(/\s+/g, " ").trim();
	if (normalized.length <= maxLength) {
		return normalized;
	}
	return `${normalized.slice(0, Math.max(0, maxLength - 3))}...`;
}

// formatHarnessStateForPrompt: ported from prime-agent (refine.run variant of the
// "when to refine" line, plus a pointer to rlm.harness CRUD and /refine).
export function formatHarnessStateForPrompt(
	state: HarnessState,
	options: {
		maxEntriesPerKind?: number;
		maxRefinements?: number;
		maxContentLength?: number;
	} = {},
): string {
	const maxEntriesPerKind = options.maxEntriesPerKind ?? DEFAULT_OVERVIEW_ENTRY_LIMIT;
	const maxRefinements = options.maxRefinements ?? DEFAULT_OVERVIEW_REFINEMENT_LIMIT;
	const maxContentLength = options.maxContentLength ?? DEFAULT_OVERVIEW_CONTENT_LIMIT;
	const lines = [
		"# Continual Harness State",
		"",
		"Local continual harness entries belong to this session. Global continual harness entries persist across sessions.",
		"The continual harness entries below are compact summaries, not full descriptions. Use them as routing/context hints; inspect or refine the underlying continual harness entry only when detail matters.",
		"Default to local continual harness refinement for current task progress, temporary blockers, and session coordination. Use global continual harness refinement only for stable cross-session lessons, durable user preferences, reusable skills/subagents, or explicitly project-qualified facts.",
		"Use these continual harness prompt notes, memories, skills, and subagent specs when they are relevant. The base system prompt is immutable; prompt entries below are supplemental notes only.",
		"",
		"Continual harness state is available as `rlm.harness` and `rlm.get_harness_state()`. CRUD calls are local to this session by default: `rlm.harness.create_memory(...)`, `rlm.harness.update_memory(...)`, `rlm.harness.delete_memory(...)`, `rlm.harness.create_skill(...)`, `rlm.harness.update_skill(...)`, `rlm.harness.delete_skill(...)`, `rlm.harness.create_subagent(...)`, `rlm.harness.update_subagent(...)`, `rlm.harness.delete_subagent(...)`, `rlm.harness.create_prompt_note(...)`, `rlm.harness.update_prompt_note(...)`, `rlm.harness.delete_prompt_note(...)`, plus `rlm.harness.record_refinement(...)` and `rlm.harness.overview()`. Use `global_=True` only for stable cross-session lessons; Python reserves `global`, so literal `global=True` is invalid syntax.",
		"",
		"Terminology: continual harness names the persisted prompt, memory, skill, and subagent layer; RLM names the runtime, IPython kernel, and native call interface exposed to the model.",
		"",
		"When to call `await refine.run()`: after a repeated failure, a reusable tactic emerges, a repeated delegation role should become a subagent spec, a repeated procedure should become a skill, a durable fact/preference should become a memory, a narrow behavioral policy should become a prompt addendum, a user corrects behavior that should persist locally or globally, validation shows a continual harness entry is wrong, or a skill/subagent/memory/prompt note should be created, updated, deleted, or rolled back. Keep `await refine.run()` continual harness edits small and evidence-backed. Direct CRUD via `rlm.harness` and the /refine command are also available.",
		"",
		"Call contract: read each installed Python skill's SKILL.md and call its documented module function in IPython; do not assume a `.run` entrypoint. Use `<skill_import> ...` in shell when a CLI exists. Continual harness skill entries are Python REPL skills with an explicit Python `reference` and `arguments` contract. Spawn a continual harness subagent spec by composing a concise task prompt and calling `handle = await rlm('sub-task')`; admission returns immediately with `rlm_child_id`, `name`, `session_dir`, and `model`, never the child's answer. Results arrive only through explicit `agent_message` replies or files; children reply with `await agent_message.send(message, receiver_role='parent')`. Use `await rlm.list_subagents()` to recover direct child handles and `await agent_message.send(..., receiver_role='child', receiver_name=handle.name)` for follow-ups. Do not invent wrappers such as `call_skill(...)`, `run_subagent(...)`, or named subagent registries.",
		"",
	];

	let totalEntries = 0;
	for (const kind of Object.keys(state.entries) as RefinementKind[]) {
		const entries = Object.values(state.entries[kind]).sort((a, b) =>
			[a.path, a.title, a.id].join("\0").localeCompare([b.path, b.title, b.id].join("\0")),
		);
		totalEntries += entries.length;
		// M7: role entries render as a count only — the actionable listing is the
		// "### Packaged subagent roles" catalog section (pi-relay port).
		if (kind === "role") {
			lines.push(`role: ${entries.length} (pass as \`role="name"\` to rlm(); see the role catalog section)`);
			continue;
		}
		if (kind === "subagent" && entries.length > 0) {
			lines.push(
				`${kind}: ${entries.length} (invoke a spec by turning it into a concise task prompt and spawning with \`await rlm('<task>')\`; admission returns a child handle, never the answer)`,
			);
		} else {
			lines.push(`${kind}: ${entries.length}`);
		}
		for (const entry of entries.slice(0, maxEntriesPerKind)) {
			const argumentsText =
				entry.kind === "skill" && Object.keys(entry.arguments).length > 0
					? ` args=${compactText(JSON.stringify(entry.arguments), maxContentLength)}`
					: "";
			const referenceText =
				entry.kind === "skill" && Object.keys(entry.reference).length > 0
					? ` ref=${compactText(JSON.stringify(entry.reference), maxContentLength)}`
					: "";
			lines.push(
				`- [${entry.scope ?? "global"}:${entry.id}] ${entry.title} (${entry.path}, v${entry.version})${referenceText}${argumentsText}: ${compactText(
					entry.content,
					maxContentLength,
				)}`,
			);
		}
		const overflow = entries.length - Math.min(entries.length, maxEntriesPerKind);
		if (overflow > 0) {
			lines.push(`- +${overflow} more ${kind} entries`);
		}
		lines.push("");
	}

	if (totalEntries === 0) {
		lines.push("No saved harness entries yet.", "");
	}

	lines.push(`recent refinements: ${state.refinements.length}`);
	for (const event of state.refinements.slice(-maxRefinements)) {
		const changes = event.changes.length > 0 ? event.changes.join(", ") : "no applied edits";
		const outcome = event.outcome ? `; outcome: ${compactText(event.outcome, maxContentLength)}` : "";
		lines.push(`- [${event.id}] ${compactText(event.trigger, maxContentLength)}: ${changes}${outcome}`);
	}
	const refinementOverflow = state.refinements.length - Math.min(state.refinements.length, maxRefinements);
	if (refinementOverflow > 0) {
		lines.push(`- +${refinementOverflow} older refinement events`);
	}

	return lines.join("\n").trim();
}

function overviewForPrompt(state: HarnessState): string {
	const lines: string[] = [];
	for (const kind of Object.keys(state.entries) as RefinementKind[]) {
		const entries = Object.values(state.entries[kind]);
		lines.push(`${kind}: ${entries.length}`);
		for (const entry of entries.slice(0, 40)) {
			const content = entry.content.replace(/\s+/g, " ").slice(0, 240);
			const argumentsText =
				entry.kind === "skill" && Object.keys(entry.arguments).length > 0
					? ` args=${JSON.stringify(entry.arguments).slice(0, 240)}`
					: "";
			const referenceText =
				entry.kind === "skill" && Object.keys(entry.reference).length > 0
					? ` ref=${JSON.stringify(entry.reference).slice(0, 240)}`
					: "";
			lines.push(
				`- [${entry.scope ?? "global"}:${entry.id}] ${entry.title} (${entry.path}, v${entry.version})${referenceText}${argumentsText}: ${content}`,
			);
		}
		if (entries.length > 40) {
			lines.push(`- +${entries.length - 40} more ${kind} entries`);
		}
	}
	return lines.join("\n");
}

function historyForPrompt(history: RefinementResult[]): string {
	if (history.length === 0) {
		return "No prior refinement history.";
	}
	return history
		.slice(-20)
		.map((item) => {
			const edits = item.appliedEdits
				.map((edit) => `${edit.applied ? "applied" : "failed"} ${edit.action} ${edit.kind}:${edit.id}`)
				.join(", ");
			const rollback = item.rollbackOf ? ` rollbackOf=${item.rollbackOf}` : "";
			return `[${item.id}]${rollback} ${item.summary}\n${edits}\nExpected outcome: ${item.expectedOutcome}`;
		})
		.join("\n\n");
}

// ---- proposal parsing (ported from prime-agent) -----------------------------

function isIncompleteJson(candidate: string): boolean {
	let depth = 0;
	let inString = false;
	let escaped = false;
	for (const char of candidate) {
		if (escaped) {
			escaped = false;
			continue;
		}
		if (inString) {
			if (char === "\\") escaped = true;
			else if (char === '"') inString = false;
			continue;
		}
		if (char === '"') inString = true;
		else if (char === "{" || char === "[") depth++;
		else if (char === "}" || char === "]") depth--;
	}
	return inString || depth > 0;
}

function parseJsonCandidate(candidate: string): unknown {
	try {
		return JSON.parse(candidate);
	} catch (error) {
		if (isIncompleteJson(candidate)) {
			throw new Error(TRUNCATED_JSON_ERROR);
		}
		throw new Error(`the model did not return valid JSON: ${error instanceof Error ? error.message : String(error)}`);
	}
}

function extractJsonObject(text: string): unknown {
	const trimmed = text.trim();
	if (trimmed.startsWith("{") && trimmed.endsWith("}")) {
		return parseJsonCandidate(trimmed);
	}
	const fenced = trimmed.match(/```(?:json)?\s*([\s\S]*?)```/);
	if (fenced) {
		return parseJsonCandidate(fenced[1].trim());
	}
	const start = trimmed.indexOf("{");
	const end = trimmed.lastIndexOf("}");
	if (start !== -1 && end > start) {
		try {
			return JSON.parse(trimmed.slice(start, end + 1));
		} catch {
			return parseJsonCandidate(trimmed.slice(start));
		}
	}
	if (isIncompleteJson(trimmed)) {
		throw new Error(TRUNCATED_JSON_ERROR);
	}
	throw new Error("Refiner did not return a JSON object");
}

function parseProposal(text: string): RefinementProposal {
	const value = extractJsonObject(text);
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new Error("Refiner JSON must be an object");
	}
	const record = value as Record<string, unknown>;
	const edits = Array.isArray(record.edits) ? record.edits : [];
	return {
		summary: typeof record.summary === "string" ? record.summary : "Refined continual harness state",
		rationale: typeof record.rationale === "string" ? record.rationale : "",
		expectedOutcome: typeof record.expectedOutcome === "string" ? record.expectedOutcome : "",
		edits: edits
			.filter((edit): edit is Record<string, unknown> => typeof edit === "object" && edit !== null)
			.map((edit) => ({
				action: edit.action as RefinementAction,
				kind: edit.kind as RefinementKind,
				id: typeof edit.id === "string" ? edit.id : undefined,
				title: typeof edit.title === "string" ? edit.title : undefined,
				content: typeof edit.content === "string" ? edit.content : undefined,
				path: typeof edit.path === "string" ? edit.path : undefined,
				reference: objectRecord(edit.reference),
				arguments: objectRecord(edit.arguments),
				metadata: objectRecord(edit.metadata),
				reason: typeof edit.reason === "string" ? edit.reason : undefined,
			})),
	};
}

function validateEdit(edit: RefinementEdit, computedId?: string): string | undefined {
	if (!["create", "update", "delete"].includes(edit.action)) {
		return `unsupported action ${String(edit.action)}`;
	}
	if (!["prompt", "memory", "skill", "subagent", "role"].includes(edit.kind)) {
		return `unsupported kind ${String(edit.kind)}`;
	}
	if (edit.kind === "prompt" && (edit.id === "base_system_prompt" || computedId === "base_system_prompt")) {
		return "base system prompt is not editable";
	}
	if (edit.action !== "create" && !edit.id) {
		return `${edit.action} requires id`;
	}
	if (edit.action !== "delete" && (!edit.title || !edit.content)) {
		return `${edit.action} requires title and content`;
	}
	if (edit.action !== "delete" && edit.kind === "skill" && edit.arguments === undefined) {
		return `${edit.action} skill requires arguments`;
	}
	if (edit.action !== "delete" && edit.kind === "skill") {
		const reference = edit.reference;
		if (!reference) {
			return `${edit.action} skill requires python reference`;
		}
		if (reference.type !== "python") {
			return `${edit.action} skill reference.type must be python`;
		}
		const hasImport =
			(typeof reference.import === "string" && reference.import.length > 0) ||
			(typeof reference.python_import === "string" && reference.python_import.length > 0);
		const hasCallable =
			(typeof reference.callable === "string" && reference.callable.length > 0) ||
			(typeof reference.call_pattern === "string" && reference.call_pattern.length > 0);
		if (!hasImport) {
			return `${edit.action} skill requires python import`;
		}
		if (!hasCallable) {
			return `${edit.action} skill requires callable or call_pattern`;
		}
	}
	return undefined;
}

// applyRefinementProposal: ported from prime-agent (full parity, including the
// baselineState conflict check that rejects edits whose target entry changed
// during the planning LLM call).
export function applyRefinementProposal(
	state: HarnessState,
	proposal: RefinementProposal,
	options: { id: string; rollbackOf?: string; scope?: HarnessScope; baselineState?: HarnessState },
): RefinementResult {
	const appliedEdits: AppliedRefinementEdit[] = [];
	const proposalModifiedKeys = new Set<string>();
	for (const edit of proposal.edits) {
		const computedId = edit.id ?? (edit.action === "create" ? slug(edit.title ?? edit.kind, edit.kind) : undefined);
		const id = computedId ?? "";
		const validationError = validateEdit(edit, id);
		if (validationError) {
			appliedEdits.push({ ...edit, id, applied: false, error: validationError });
			continue;
		}

		const records = state.entries[edit.kind];
		const before = cloneEntry(records[id]);
		// PA parity: when a baseline was captured before the (slow) planning call,
		// reject edits whose target changed underneath us (e.g. kernel rlm.harness
		// writes) instead of clobbering them.
		const entryKey = `${edit.kind}:${id}`;
		const baseline = cloneEntry(options.baselineState?.entries[edit.kind][id]);
		if (
			options.baselineState &&
			!proposalModifiedKeys.has(entryKey) &&
			JSON.stringify(before) !== JSON.stringify(baseline)
		) {
			appliedEdits.push({
				...edit,
				id,
				before,
				applied: false,
				error: "entry changed during refinement planning",
			});
			continue;
		}
		if (edit.action === "delete") {
			if (!before) {
				appliedEdits.push({ ...edit, id, applied: false, error: "entry not found" });
				continue;
			}
			delete records[id];
			proposalModifiedKeys.add(entryKey);
			appliedEdits.push({ ...edit, id, before, applied: true });
			continue;
		}
		if (edit.action === "create" && before) {
			appliedEdits.push({ ...edit, id, before, applied: false, error: "entry already exists" });
			continue;
		}
		if (edit.action === "update" && !before) {
			appliedEdits.push({ ...edit, id, applied: false, error: "entry not found" });
			continue;
		}

		const createdAt = before?.created_at ?? now();
		const version = before ? before.version + 1 : 1;
		const after: HarnessEntry = {
			id,
			kind: edit.kind,
			title: edit.title ?? before?.title ?? id,
			content: edit.content ?? before?.content ?? "",
			path: edit.path ?? before?.path ?? "general",
			scope: before?.scope ?? options.scope ?? "local",
			reference: edit.reference ?? before?.reference ?? {},
			arguments: edit.arguments ?? before?.arguments ?? {},
			metadata: edit.metadata ?? before?.metadata ?? {},
			source: "refine",
			created_at: createdAt,
			updated_at: now(),
			version,
		};
		records[id] = after;
		proposalModifiedKeys.add(entryKey);
		appliedEdits.push({ ...edit, id, before, after: cloneEntry(after), applied: true });
	}

	const changes = appliedEdits.filter((edit) => edit.applied).map((edit) => `${edit.action} ${edit.kind}:${edit.id}`);
	state.refinements.push({
		id: options.id,
		trigger: proposal.summary,
		changes,
		evidence: proposal.rationale,
		outcome: proposal.expectedOutcome,
		created_at: now(),
	});

	return {
		id: options.id,
		summary: proposal.summary,
		rationale: proposal.rationale,
		expectedOutcome: proposal.expectedOutcome,
		appliedEdits,
		harnessStatePath: "",
		rollbackOf: options.rollbackOf,
		scope: options.scope,
	};
}

function rollbackProposal(target: RefinementResult): RefinementProposal {
	const edits: RefinementEdit[] = [];
	for (const edit of [...target.appliedEdits].reverse()) {
		if (!edit.applied) continue;
		if (edit.before) {
			edits.push({
				action: edit.after ? "update" : "create",
				kind: edit.kind,
				id: edit.id,
				title: edit.before.title,
				content: edit.before.content,
				path: edit.before.path,
				reference: edit.before.reference,
				arguments: edit.before.arguments,
				metadata: edit.before.metadata,
				reason: `Rollback ${target.id}`,
			});
		} else if (edit.after) {
			edits.push({
				action: "delete",
				kind: edit.kind,
				id: edit.id,
				reason: `Rollback ${target.id}`,
			});
		}
	}
	return {
		summary: `Rollback refinement ${target.id}`,
		rationale: `Restores continual harness state snapshots from refinement ${target.id}.`,
		expectedOutcome: "Faulty refinement edits are reverted.",
		edits,
	};
}

/** Refinement history from session custom entries (M2 replaces PA's global
 * refinements.jsonl with session-scoped history; custom entries survive
 * compaction because they are not message entries). */
export function getRefinementHistory(entries: readonly unknown[]): RefinementResult[] {
	return entries
		.filter((entry): entry is { customType: string; data: unknown } => {
			return (
				typeof entry === "object" &&
				entry !== null &&
				(entry as { customType?: unknown }).customType === REFINEMENT_CUSTOM_TYPE
			);
		})
		.map((entry) => entry.data)
		.filter((data): data is RefinementResult => {
			return typeof data === "object" && data !== null && "id" in data && "appliedEdits" in data;
		});
}

export interface RefinementPlan {
	proposal: RefinementProposal;
	id: string;
	rollbackOf?: string;
	rollbackScope?: HarnessScope;
	/** Target-scope state captured before planning, used to reject conflicting edits at apply time. */
	baselineState?: HarnessState;
}

export function inferRefinementResultScope(result: RefinementResult): HarnessScope | undefined {
	if (result.scope) return result.scope;
	const scopes = new Set<HarnessScope>();
	for (const edit of result.appliedEdits) {
		const scope = edit.after?.scope ?? edit.before?.scope;
		if (scope) scopes.add(scope);
	}
	return scopes.size === 1 ? [...scopes][0] : undefined;
}

// planRefinement: ported from prime-agent. M2: messages arrive as LLM-ready
// AgentMessages; auth is resolved by the caller (apiKey/headers params).
export async function planRefinement(
	messages: Parameters<typeof convertToLlm>[0],
	state: HarnessState,
	history: RefinementResult[],
	model: Model<any>,
	auth: { apiKey?: string; headers?: Record<string, string> },
	options: RefineOptions = {},
	signal?: AbortSignal,
): Promise<RefinementPlan> {
	const id = `refine_${new Date()
		.toISOString()
		.replace(/[^0-9]/g, "")
		.slice(0, 17)}`;
	if (options.rollbackId) {
		const target = history.find((item) => item.id === options.rollbackId);
		if (!target) {
			throw new Error(`Refinement ${options.rollbackId} not found`);
		}
		const fallbackScope: HarnessScope = options.global ? "global" : "local";
		return {
			proposal: rollbackProposal(target),
			id,
			rollbackOf: target.id,
			rollbackScope: inferRefinementResultScope(target) ?? fallbackScope,
		};
	}

	const conversationText = serializeConversation(convertToLlm(messages)).slice(-80_000);
	const scopeInstruction = options.global
		? "Requested refinement scope: global. Only propose stable cross-session continual harness edits, durable user preferences, reusable skills/subagents, or explicitly project-qualified facts that should affect future sessions. Do not persist session-only progress, temporary blockers, or current-run coordination globally."
		: "Requested refinement scope: local. Prefer local continual harness edits for current task progress, temporary blockers, current-run coordination, and project facts that are not clearly reusable across sessions. Global entries in the overview are read-only context: do not propose update or delete edits for them; create a local entry instead if an override is needed.";
	const userPrompt = [
		`<current_harness_state>\n${overviewForPrompt(state)}\n</current_harness_state>`,
		`<refinement_history>\n${historyForPrompt(history)}\n</refinement_history>`,
		`<conversation>\n${conversationText}\n</conversation>`,
		`<scope_policy>\n${scopeInstruction}\n</scope_policy>`,
		options.instructions ? `<user_refine_instructions>\n${options.instructions}\n</user_refine_instructions>` : "",
		"Return only JSON edits. If no useful edit is justified, return an empty edits array with a rationale.",
	]
		.filter(Boolean)
		.join("\n\n");

	const response = await completeSimple(
		model,
		{
			systemPrompt: REFINEMENT_SYSTEM_PROMPT,
			messages: [{ role: "user", content: [{ type: "text", text: userPrompt }], timestamp: Date.now() }],
		},
		{ maxTokens: refinementMaxOutputTokens(model), signal, apiKey: auth.apiKey, headers: auth.headers },
	);

	if (response.stopReason === "error") {
		throw new Error(`Refinement failed: ${response.errorMessage || "Unknown error"}`);
	}
	if (response.stopReason === "length") {
		throw new Error(`Refinement failed: ${TRUNCATED_JSON_ERROR}`);
	}

	const text = response.content
		.filter((content): content is { type: "text"; text: string } => content.type === "text")
		.map((content) => content.text)
		.join("\n");
	return { proposal: parseProposal(text), id };
}

/** plan + apply + save in one step (the /refine command's main path). */
export async function refineHarness(
	messages: Parameters<typeof convertToLlm>[0],
	targetStateDir: string,
	mergedState: HarnessState,
	history: RefinementResult[],
	model: Model<any>,
	auth: { apiKey?: string; headers?: Record<string, string> },
	options: RefineOptions = {},
	signal?: AbortSignal,
): Promise<RefinementResult> {
	const scope: HarnessScope = options.global ? "global" : "local";
	const plan = await planRefinement(messages, mergedState, history, model, auth, options, signal);
	// Apply against a FRESH read of the target store (the LLM call can take many
	// seconds; the kernel may have written the file in between).
	const targetState = loadHarnessState(targetStateDir, scope);
	const result = applyRefinementProposal(targetState, plan.proposal, {
		id: plan.id,
		rollbackOf: plan.rollbackOf,
		scope: plan.rollbackScope ?? scope,
	});
	result.harnessStatePath = saveHarnessState(targetStateDir, targetState);
	return result;
}

// ============================================================================
// Refinement history files + auto-refine review (ported from prime-agent
// core/refinement/refinement.ts for M2 full /refine parity; REFINEMENT_CUSTOM_TYPE
// is prime-harness's own, otherwise verbatim semantics).
// ============================================================================

const REFINEMENT_HISTORY_FILE_NAME = "refinements.jsonl";

export type AutoRefineReason = "turn_interval" | "compact";

export interface AutoRefineReviewContext {
	reason: AutoRefineReason;
	turnsSinceLastReview: number;
}

export interface AutoRefineReview {
	shouldRefine: boolean;
	rationale: string;
	instructions?: string;
}

const AUTO_REFINE_REVIEW_SYSTEM_PROMPT = `You are Prime Agent's automatic /refine review gate.

Decide whether this checkpoint should run /refine. Auto /refine writes local continual harness state by default, so approve when the trajectory contains evidence useful to this session's future turns.
Reject one-off noise, unsupported hypotheses, and transient tool outputs. Ask for global refinement only for durable cross-session lessons or explicitly project-qualified lessons likely to be reused in future sessions.

Return JSON only:
{
  "shouldRefine": true|false,
  "rationale": "short reason",
  "instructions": "optional concise instructions for /refine if shouldRefine is true"
}`;

const AUTO_REFINE_REVIEW_MAX_OUTPUT_TOKENS = 4_096;

function autoRefineReviewMaxOutputTokens(model: Model<any>): number {
	return Math.min(model.maxTokens, AUTO_REFINE_REVIEW_MAX_OUTPUT_TOKENS);
}

function withDefaultRefinementScope(result: RefinementResult, scope: HarnessScope): RefinementResult {
	const inferred = inferRefinementResultScope(result);
	return { ...result, scope: inferred ?? scope };
}

export function getRefinementHistoryPath(harnessStateDir: string): string {
	return join(harnessStateDir, REFINEMENT_HISTORY_FILE_NAME);
}

function isRefinementResult(data: unknown): data is RefinementResult {
	return typeof data === "object" && data !== null && "id" in data && "appliedEdits" in data;
}

/**
 * Append a global-scope refinement to the cross-session history log so it can be
 * rolled back from any session. Local-scope refinements are recorded only in the
 * session JSONL and roll back via their recorded harnessStatePath.
 */
export function appendGlobalRefinement(harnessStateDir: string, result: RefinementResult): string {
	const historyPath = getRefinementHistoryPath(harnessStateDir);
	mkdirSync(harnessStateDir, { recursive: true });
	appendFileSync(historyPath, `${JSON.stringify(result)}\n`, "utf8");
	return historyPath;
}

export function loadGlobalRefinementHistory(harnessStateDir: string): RefinementResult[] {
	const historyPath = getRefinementHistoryPath(harnessStateDir);
	if (!existsSync(historyPath)) {
		return [];
	}
	const results: RefinementResult[] = [];
	for (const line of readFileSync(historyPath, "utf8").split("\n")) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		try {
			const parsed = JSON.parse(trimmed);
			if (isRefinementResult(parsed)) {
				results.push(withDefaultRefinementScope(parsed, "global"));
			}
		} catch {
			// Skip malformed lines so a single bad append cannot break rollback.
		}
	}
	return results;
}

/**
 * Merge global and session refinement history, de-duplicating by id. Session entries
 * win on conflict so a session that is mid-flight still resolves its own latest result.
 */
export function mergeRefinementHistory(
	global: readonly RefinementResult[],
	session: readonly RefinementResult[],
): RefinementResult[] {
	const byId = new Map<string, RefinementResult>();
	for (const result of global) {
		byId.set(result.id, result);
	}
	for (const result of session) {
		const existing = byId.get(result.id);
		byId.set(result.id, result.scope || !existing?.scope ? result : { ...result, scope: existing.scope });
	}
	return [...byId.values()];
}

function parseAutoRefineReview(text: string): AutoRefineReview {
	const value = extractJsonObject(text);
	if (typeof value !== "object" || value === null || Array.isArray(value)) {
		throw new Error("Auto-refine review JSON must be an object");
	}
	const record = value as Record<string, unknown>;
	return {
		shouldRefine: record.shouldRefine === true,
		rationale: typeof record.rationale === "string" ? record.rationale : "No rationale provided.",
		instructions: typeof record.instructions === "string" ? record.instructions : undefined,
	};
}

/**
 * Auto-refine review gate (ported verbatim semantics from prime-agent): a small
 * non-reasoning LLM call that decides whether an auto-refine checkpoint should
 * proceed to planning.
 */
export async function reviewAutoRefine(
	messages: Parameters<typeof convertToLlm>[0],
	state: HarnessState,
	history: RefinementResult[],
	model: Model<any>,
	auth: { apiKey?: string; headers?: Record<string, string> },
	context: AutoRefineReviewContext,
	signal?: AbortSignal,
): Promise<AutoRefineReview> {
	const conversationText = serializeConversation(convertToLlm(messages)).slice(-40_000);
	const userPrompt = [
		`<trigger>\n${context.reason}; ${context.turnsSinceLastReview} assistant turns since last auto-refine review\n</trigger>`,
		`<current_harness_state>\n${overviewForPrompt(state)}\n</current_harness_state>`,
		`<refinement_history>\n${historyForPrompt(history)}\n</refinement_history>`,
		`<conversation>\n${conversationText}\n</conversation>`,
		"Return shouldRefine=true when the trajectory contains evidence useful to this session's future turns. Prefer local harness edits for current task progress, temporary blockers, and current-run coordination. Ask for global refinement only for durable cross-session lessons or explicitly project-qualified facts likely to be reused in future sessions.",
	].join("\n\n");
	const response = await completeSimple(
		model,
		{
			systemPrompt: AUTO_REFINE_REVIEW_SYSTEM_PROMPT,
			messages: [{ role: "user", content: [{ type: "text", text: userPrompt }], timestamp: Date.now() }],
		},
		{ maxTokens: autoRefineReviewMaxOutputTokens(model), signal, apiKey: auth.apiKey, headers: auth.headers },
	);
	if (response.stopReason === "error") {
		throw new Error(`Auto-refine review failed: ${response.errorMessage || "Unknown error"}`);
	}
	if (response.stopReason === "length") {
		throw new Error(`Auto-refine review failed: ${TRUNCATED_JSON_ERROR}`);
	}
	const text = response.content
		.filter((content): content is { type: "text"; text: string } => content.type === "text")
		.map((content) => content.text)
		.join("\n");
	return parseAutoRefineReview(text);
}
