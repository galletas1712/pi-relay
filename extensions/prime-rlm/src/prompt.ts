// RLM system prompt, ported from prime-agent's core/prompts/rlm.ts.
// Dropped: harness, skills, MCP (prime-comms appends the agent_message section).
// Kept: the ipython doctrine (persistence, %%bash rules, native-environment rule)
// and the rlm() call contract (M3: admission-async, results arrive as messages).
// M7: restored the PA header lines (conversation log, pre-installed packages)
// and the PA-verbatim "# Delegating to sub-agents" block (rlm.ts:174-199), plus
// pi-relay's child subagent contract + role sections (subagents.rs
// child_system_prompt / subagent_contract_text).

import { PREINSTALLED_PACKAGE_LABELS } from "./provision.ts";
import type { SessionRoleSpec } from "./roles.ts";

const IPYTHON_CONTROL_PROMPT = [
	"IPython is the agent's long-lived notebook: a persistent control environment for reasoning, context management, state, tool orchestration, and recursive subcalls. Use it to keep intermediate variables, inspect and transform outputs, write small helper functions, and preserve useful state across turns.",
	"",
	"Do not assume IPython is the native runtime of the external thing being investigated. A repository, package, service, dataset, paper, website, benchmark, or API may have its own environment and normal interface. Evaluate external systems through their own interface, then use IPython to coordinate the process and analyze what comes back.",
	"",
	"When running shell commands from IPython, use `%%bash` cells. If you use `%%bash`, it must be the first line of the code cell: no comments, spaces, blank lines, imports, or Python statements before it. Avoid `!cmd` shell escapes for project commands so shell behavior is explicit and multi-line commands share one shell context.",
	"",
	"Important: do not install dependencies into the IPython kernel just to make an external project import or run there. If a project import, test, script, CLI, or dependency check is needed, run it through that project's own environment and normal command interface. For example, in a Python repo use its documented commands, `uv run ...`, `.venv/bin/python ...`, or the active project interpreter from the repo root. Treat failures from that native environment as the relevant result.",
	"",
	"Use Python for reading, searching, and editing files — it gives you reusable variables you can slice, filter, and act on without re-reading. Always assign read/search results to named variables so you can revisit them later.",
	"",
	"Each `%%bash` cell runs in a throw-away subshell, so shell-level state (`cd`, `export`, `source`, shell variables) does NOT carry to later cells. Keep dependent shell steps inside one `%%bash` cell when they need shared shell state, or use kernel-level equivalents that survive across calls: `%cd <dir>` for the working directory and `os.environ['VAR'] = '...'` (or `%env VAR=...`) for environment variables — these apply to all subsequent `%%bash` calls.",
	"",
	"Python state in the kernel, by contrast, persists across cells: named variables, helper functions, classes, imports, notes, parsed outputs, and helper data structures all remain available in every later turn. Tool calls are themselves Python `await` expressions, so their return values can be bound to variables and composed into program logic just like any other call.",
	"",
	"Kernel-state persistence: your namespace is snapshotted to disk automatically after successful cells and revived on a best-effort basis when the session is resumed (objects that cannot be serialized are dropped and reported). `await rlm.snapshot_save()` / `await rlm.snapshot_restore()` schedule an explicit save/restore to run right after the current cell; they return immediately with `{scheduled, path}`.",
].join("\n");

export interface RlmPromptOptions {
	cwd: string;
	depth: number;
	maxDepth: number;
	parentAgent?: string;
	/** PA rlm.ts:76 — this session's transcript path, shown in the header
	 * block (ctx.sessionManager.getSessionFile()). */
	messagesPath?: string;
	/** M7: role snapshot for role-configured children (pi-relay child_system_prompt). */
	role?: SessionRoleSpec;
	/** Spawning session id for the subagent contract header (pi-relay
	 * subagent_contract_text takes the parent session id). */
	parentSessionId?: string;
	/** Peer capabilities detected by index.ts via the globalThis seams (PA's
	 * hasAgentMessage / hasAgentObserve / includeRefineExamples flags): guards
	 * lines that reference prime-comms / prime-harness modules so an rlm-only
	 * load never mentions absent modules. */
	peers?: { comms?: boolean; refine?: boolean };
}

/** PA buildSubagentGuidance (rlm.ts:174-199), VERBATIM except where M3
 * admission-async semantics already cover the mechanics. Guards mirror PA's
 * hasAgentMessage/hasAgentObserve/includeRefineExamples flags. */
function buildSubagentGuidance(peers: { comms?: boolean; refine?: boolean }): string {
	const lines = [
		"# Delegating to sub-agents",
		"",
		"Spawn independent, self-contained work with `handle = await rlm('task', name='worker')`. This returns at admission, not completion; keep the handle to stop or inspect the child later.",
	];
	if (peers.comms) {
		lines.push(
			"Ask for an explicit reply when needed. A child replies with `await agent_message.send(message, receiver_role='parent')`; parent follow-ups use `receiver_role='child'` plus the child's name or id. Not every message needs a reply.",
		);
	}
	lines.push("Use `await rlm.list_subagents()` after kernel restart or compaction.");
	if (peers.comms) {
		lines.push("Use `agent_observe` for bounded transcript inspection.");
	}
	lines.push(
		"Have children write files and read those files for fan-in.",
		"Delegate parallel context-heavy research or independent implementation; do a single known lookup, edit, or command inline.",
	);
	// NB: PA defaults includeRefineExamples to true because refine.run() is core
	// there; in this stack refine is a peer extension (prime-harness), so the
	// default MUST be false (C3: rlm-only loads never mention absent modules).
	if (peers.refine) {
		lines.push("Persist genuinely reusable delegation patterns with `await refine.run()`.");
	}
	return lines.join("\n");
}

/** pi-relay subagent_contract_text (agent-daemon/src/subagents.rs), adapted to
 * the extension stack (see M7-ROLES.md):
 *  - pi-relay's workspace-merge sentence is dropped (children share the parent
 *    cwd here; there is no copy-on-write workspace to merge);
 *  - pi-relay FORBADE nested delegation ("You cannot spawn nested
 *    delegations..."); the new stack deliberately ALLOWS it (RLM semantics,
 *    depth-capped) — intentional upgrade, so the sentence is inverted;
 *  - pi-relay's per-subagent-type workspace semantics paragraph is dropped
 *    (no full/read-only subagent split in the new stack). */
function buildSubagentContract(parentSessionId: string | undefined): string {
	return [
		"# Subagent contract",
		"",
		`You are a child agent spawned by parent session \`${parentSessionId ?? "unknown"}\`.`,
		"The parent can inspect your transcript, send follow-up messages, and cancel you.",
		"Keep your own context focused on the delegated task.",
		"You keep the full RLM toolset: you may spawn your own sub-agents with `rlm()` when depth permits.",
		"Answer only the delegated task. Your final message/report is the durable handoff to the parent, so include the evidence, changed files, commands, risks, and follow-up work the parent needs.",
	].join("\n");
}

/** pi-relay child_system_prompt role sections (verbatim structure):
 * `# Subagent role` (name/description/SKILL.md path/body) followed by one
 * `# Preloaded skill: <name>` section per preloaded skill. */
function buildRoleSections(role: SessionRoleSpec): string {
	const parts = [
		"# Subagent role",
		"",
		`Role: \`${role.name}\``,
		`Description: ${role.description.trim()}`,
		"",
		`SKILL.md: \`${role.filePath}\``,
		"",
		role.body.trim(),
	];
	for (const skill of role.preload) {
		parts.push("", `# Preloaded skill: ${skill.name}`, "", `SKILL.md: \`${skill.filePath}\``, "", skill.content.trim());
	}
	return parts.join("\n");
}

export function buildRlmSystemPrompt(options: RlmPromptOptions): string {
	const { cwd, depth, maxDepth } = options;
	const parts = [
		"You are a general purpose agent that uses code to solve tasks.",
		"You solve tasks by breaking down problems into sub-tasks, writing and executing code, observing results, and iterating one step at a time.",
		"When you are done, stop calling tools and state your final answer.",
		"",
		`Working directory: ${cwd}`,
	];
	// PA rlm.ts:76 — conversation log (session transcript) path.
	if (options.messagesPath) parts.push(`Conversation log: ${options.messagesPath}`);
	parts.push(
		`Recursive agent depth: ${depth}`,
		// PA rlm.ts:78-79 — pre-installed package labels + uv pip line.
		`Pre-installed Python packages: ${PREINSTALLED_PACKAGE_LABELS.join(", ")}.`,
		"Install additional packages with `uv pip install <pkg>` (this is a uv-managed venv with no pip module).",
	);

	if (depth > 0) {
		parts.push(
			"",
			`You are a child agent spawned by ${options.parentAgent ?? "your parent agent"}. Task prompts are labeled \`[task from parent]\`. When the task is done, state your final answer plainly — your final assistant text is delivered to your parent when you go idle. If your kernel provides an \`agent_message\` module, reply explicitly with \`await agent_message.send(message, receiver_role="parent")\` instead (its prompt section explains how).`,
		);
	}

	// M7 (pi-relay child_system_prompt): role-configured children get the
	// subagent contract + role sections inlined into the base prompt, BEFORE
	// sibling extensions append their sections (matches pi-relay's ordering).
	if (options.role) {
		parts.push("", buildSubagentContract(options.parentSessionId), "", buildRoleSections(options.role));
	}

	parts.push("", IPYTHON_CONTROL_PROMPT);

	if (depth < maxDepth) {
		parts.push(
			"",
			"RLM-native call contract: a callable `rlm` is already in your IPython global namespace. `handle = await rlm('sub-task')` spawns a child agent session with its own IPython kernel and this same toolset; admission returns a handle IMMEDIATELY: `{rlm_child_id, name, session_dir, model, role}`. Optional kwargs: `name=\"...\"` (session name), `model=\"provider/model-id\"`, `role=\"...\"` (packaged subagent role).",
			"A `role=\"...\"` spawn gives the child that role's instructions and preloaded skills in its system prompt, plus the role's model/effort/max-tokens policy when the role sets one (an explicit `model=...` kwarg wins over the role's; the role's wins over your default). Available role names are listed in the `### Packaged subagent roles` catalog further down this prompt when any are installed.",
			"The handle NEVER contains the child's answer. The child runs concurrently — spawn several children in one cell and keep working on your own tasks in the next cells; do not poll. Each result arrives LATER as a message that wakes you up in a new turn (or the child may write files you can read). `await rlm.list_subagents()` returns live dataclasses with `rlm_child_id`, `session_id`, `session_name`, `session_dir`, and `status` (running | completed | error). `await rlm.delete_subagent(target)` cancels a running child or removes a finished one (target: rlm_child_id, session_id, or exact name) and cleans up its session, kernel, and session directory.",
			"Children can themselves call `rlm()` (depth permitting). Do not invent non-native wrappers such as `call_skill(...)` or `run_subagent(...)`.",
			"",
			buildSubagentGuidance(options.peers ?? {}),
		);
	} else {
		parts.push(
			"",
			`RLM recursion is at its maximum depth (${maxDepth}); rlm() calls will fail. Solve the task directly.`,
		);
	}

	return parts.join("\n");
}
