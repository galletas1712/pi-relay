// prime-rlm — M1 milestone extension for unpatched upstream pi.
//
// Proves prime-agent's differentiators as a pure extension package:
//   (a) persistent per-session IPython kernel exposed as the only model tool
//   (b) in-kernel rlm() spawning in-process child agent sessions (admission-async, M3)
//   (c) minimal RLM system prompt replacing pi's default coding prompt
//
// The extension factory runs once per AgentSession in this process; all
// cross-session state is keyed by sessionId in registry.ts.
import { homedir } from "node:os";
import { join } from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import {
	_bindRegistryAccessors,
	collectBootstrapContributions,
	collectHostHandlers,
	type PrimeRlmSessionInfo,
} from "./src/api.ts";
import { createIpythonToolDefinition, KernelProvisioner, type ReplReporter } from "./src/ipython-tool.ts";
import type { KernelSentAgentMessage } from "./src/kernel.ts";
import { ReplConsole } from "./src/repl-console.ts";
import { buildRlmSystemPrompt } from "./src/prompt.ts";
import {
	disposeSessionState,
	getSessionState,
	listSessionStates,
	peekSessionState,
} from "./src/registry.ts";
import { cleanupRunsForParent, createRlmHostHandlers, rlmMaxDepth } from "./src/rlm-host.ts";
import { detectPeerExtensions } from "./src/roles.ts";

// Publish the public host API (globalThis seam for prime-harness / prime-comms;
// see src/api.ts) and bind it to this module instance's registry. Importing
// ./src/api.ts has the side effect of publishing; _bindRegistryAccessors gives
// it read access to the session registry.
_bindRegistryAccessors(peekSessionState, listSessionStates);

function agentDir(): string {
	return process.env.PI_CODING_AGENT_DIR ?? join(homedir(), ".pi", "agent");
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

function sessionIdOf(ctx: ExtensionContext): string {
	return ctx.sessionManager.getSessionId();
}

/** M7: kernel bootstrap contribution importing a role's python_preload skills
 * (pi-relay preload doctrine, kernel side). Undefined when no role/no python
 * preloads. */
function roleBootstrapContribution(
	sessionId: string,
): { python?: string; env?: Record<string, string> } | undefined {
	const role = peekSessionState(sessionId)?.role;
	const pythonSkills = role?.preload.filter((s) => s.pythonImport && s.pythonPath) ?? [];
	if (pythonSkills.length === 0) return undefined;
	const imports = pythonSkills
		.map((s) => `    (${JSON.stringify(s.pythonPath!)}, ${JSON.stringify(s.pythonImport!)}),`)
		.join("\n");
	return {
		python: `
import sys as _rlm_role_sys

for _rr_src, _rr_mod in [
${imports}
]:
    if _rr_src not in _rlm_role_sys.path:
        _rlm_role_sys.path.insert(0, _rr_src)
    globals()[_rr_mod] = __import__(_rr_mod)
`.trim(),
	};
}

function sessionInfoOf(ctx: ExtensionContext): PrimeRlmSessionInfo {
	return {
		sessionId: sessionIdOf(ctx),
		sessionDir: ctx.sessionManager.getSessionDir(),
		cwd: ctx.cwd,
		agentDir: agentDir(),
		depth: getSessionState(sessionIdOf(ctx)).depth,
	};
}

export default function primeRlm(pi: ExtensionAPI): void {
	// Per-session provisioner registry. Created lazily on first ipython execute
	// so kernels are never spawned for sessions that don't use the tool.
	const provisioners = new Map<string, KernelProvisioner>();
	// Latest ctx per session, so kernel host handlers (rlm.run) can reach the
	// owning session's model registry / cwd / sessionManager.
	const latestCtx = new Map<string, ExtensionContext>();

	// M4 (O2): record kernel-emitted agent-message receipts as session entries —
	// the pi equivalent of PA's appendCustomEntry("ipython_sent_agent_message").
	// Entries are rpc-visible (entry_appended events) but never enter LLM context.
	const recordSentAgentMessage = (toolCallId: string | undefined, message: KernelSentAgentMessage): void => {
		try {
			pi.appendEntry("ipython_sent_agent_message", { toolCallId: toolCallId ?? null, message });
		} catch (error) {
			console.warn(`[prime-rlm] failed to record sent agent message: ${errorMessage(error)}`);
		}
	};

	// M9: per-factory (per-session) repl console. Emits repl_cell/repl_output
	// session entries via appendEntry — the same host→bridge pipe as
	// rlm_child_lifecycle — and consumes sentinel repl payloads from rpc input.
	// User cells never start a turn and never enter model context; the shared
	// kernel namespace with the model is the point.
	const replConsole = new ReplConsole({
		emit: (customType, data) => {
			try {
				pi.appendEntry(customType, data);
			} catch (error) {
				console.warn(`[prime-rlm] failed to emit repl event: ${errorMessage(error)}`);
			}
		},
		getProvisioner: (ctx) => getProvisioner(ctx),
		peekManager: (sessionId) => provisioners.get(sessionId)?.manager,
		sessionIdOf,
	});

	const getProvisioner = (ctx: ExtensionContext): KernelProvisioner => {
		const sessionId = sessionIdOf(ctx);
		latestCtx.set(sessionId, ctx);
		let provisioner = provisioners.get(sessionId);
		if (!provisioner) {
			const info = sessionInfoOf(ctx);
			provisioner = new KernelProvisioner({
				cwd: ctx.cwd,
				agentDir: agentDir(),
				sessionId,
				hostHandlers: {
					...createRlmHostHandlers({
						getState: () => getSessionState(sessionId),
						provisionerFor: () => provisioners.get(sessionId),
					}),
					// M2 seam: sibling-extension host handlers (agent_message.*, ...).
					...collectHostHandlers(info),
				},
				// M7: role python_preload imports run even in rlm-only loads
				// (prime-harness pre-imports ALL discovered python skills, but an
				// rlm-only session has no harness contribution).
				contributions: () =>
					[roleBootstrapContribution(sessionId), ...collectBootstrapContributions(info)].filter(
						(c): c is { python?: string; env?: Record<string, string> } => c !== undefined,
					),
				// M3: per-session kernel-state snapshot lives under the session's
				// prime/ dir (same convention as prime-harness + prime-comms).
				snapshotDir: join(info.sessionDir, "prime", sessionId, "kernel"),
				onLateSentAgentMessage: (message) => recordSentAgentMessage(undefined, message),
				onCellEvent: (ev) => replConsole.cellEvent(ev),
			});
			provisioners.set(sessionId, provisioner);
			getSessionState(sessionId).provisioner = provisioner;
		}
		return provisioner;
	};

	// M9: model tool-cells flow into the repl console with provenance "model".
	const replReporter: ReplReporter = {
		queued: (toolCallId, code, ctx) => replConsole.reportModelQueued(toolCallId, code, ctx),
		executeError: (meta, error) => replConsole.reportExecuteError(meta, error),
		settled: (meta, result) => replConsole.reportExecuteSettled(meta, result),
	};
	pi.registerTool(createIpythonToolDefinition(getProvisioner, recordSentAgentMessage, replReporter));

	// M9: claim sentinel repl payloads from rpc input. pi's rpc-mode prompt path
	// fires `input` BEFORE the streaming/busy check and BEFORE context
	// injection, so returning {action:"handled"} acks immediately with no turn.
	// Execution is fire-and-forget; repl_cell/repl_output events carry the
	// lifecycle. Non-sentinel input is untouched.
	pi.on("input", (event, ctx) => {
		if (event.source !== "rpc") return { action: "continue" as const };
		return replConsole.handleInput(event.text, ctx)
			? { action: "handled" as const }
			: { action: "continue" as const };
	});

	pi.on("session_start", async (_event, ctx) => {
		const sessionId = sessionIdOf(ctx);
		latestCtx.set(sessionId, ctx);
		// M9: capability marker for the bridge — repl.execute refuses to forward
		// sentinel payloads to a host whose extension has not reported ready
		// (otherwise the payload would be treated as a normal prompt and start
		// a garbage turn). Control frame; the bridge does not spool it.
		//
		// Timing: rpc-mode binds extensions (firing session_start) BEFORE it
		// subscribes the rpc event sink (rpc-mode.js rebindSession), so an
		// entry appended synchronously here never reaches the wire. Defer past
		// the bind macrotask, with retries — duplicates are harmless (the bridge
		// only flips a ready flag and never spools this entry).
		const emitReadyMarker = () => {
			try {
				pi.appendEntry("prime_rlm_ready", { features: ["repl"], version: 1 });
			} catch {
				/* best effort */
			}
		};
		setTimeout(emitReadyMarker, 0);
		setTimeout(emitReadyMarker, 100);
		setTimeout(emitReadyMarker, 500);
		// Materialize state now so pendingDepth is consumed even if the model
		// never calls the tool.
		const state = getSessionState(sessionId);
		state.ctx = ctx;
		// M3: capture this session's ExtensionAPI for the rlm-only result
		// fallback (direct injection when prime-comms is not loaded).
		state.pi = pi;
	});

	pi.on("before_agent_start", async (event, ctx) => {
		const sessionId = sessionIdOf(ctx);
		latestCtx.set(sessionId, ctx);
		const state = getSessionState(sessionId);
		state.ctx = ctx;
		const peers = detectPeerExtensions();
		return {
			systemPrompt: buildRlmSystemPrompt({
				cwd: ctx.cwd,
				depth: state.depth,
				maxDepth: rlmMaxDepth(),
				parentAgent: undefined,
				// PA rlm.ts:76 conversation-log line.
				messagesPath: ctx.sessionManager.getSessionFile() ?? undefined,
				// M7: role snapshot + parent id for the subagent contract.
				role: state.role,
				parentSessionId: state.parentRef?.parentSessionId,
				// PA's hasAgentMessage/hasAgentObserve/includeRefineExamples flags.
				peers: { comms: peers.comms, refine: peers.harness },
			}),
		};
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		const sessionId = sessionIdOf(ctx);
		latestCtx.delete(sessionId);
		provisioners.delete(sessionId);
		// Tombstone this session's child runs first so detached tasks stop
		// without delivering outcomes to a dying parent.
		cleanupRunsForParent(sessionId);
		await disposeSessionState(sessionId);
	});
}
