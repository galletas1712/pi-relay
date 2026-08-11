// prime-autonomy (M4): prime-agent autonomy features as a pure extension on
// unpatched upstream pi.
//
//   G1 thread goals        — src/goals.ts
//   G2 rlm heartbeats      — src/heartbeats.ts
//   G3 autonomous mode     — src/autonomous.ts
//   G4 kernel compact API  — src/compact.ts
//
// PROVENANCE: extension wiring ported from prime-agent
//   packages/coding-agent/src/core/agent-session.ts
//   (_getContinuationMessages ordering, goal flag seeding, budget steering,
//   _parseGoalSlashCommand/_parseAutonomousSlashCommand, handle*HostRequest).
// The settle pipeline below mirrors PA's _getContinuationMessages composition:
// goal continuation first, then autonomous, and a pending kernel-requested
// compaction consumes the settle before either (PA _shouldStopAfterTurn order).
import type { AgentMessage } from "@earendil-works/pi-agent-core";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";
import {
	GoalRuntime,
	GOAL_STATE_CUSTOM_TYPE,
	createGoalContextMessage,
	validateGoalBudget,
	validateGoalObjective,
	type GoalContextMessage,
} from "./src/goals.ts";
import {
	createAutonomousRuntimeState,
	setAutonomousEnabled,
	autonomousStatus,
	addAutonomousUsage,
	nextAutonomousContinuation,
	type AgentAutonomousConfig,
	type AutonomousRuntimeState,
} from "./src/autonomous.ts";
import { HeartbeatRuntime } from "./src/heartbeats.ts";
import { CompactRuntime } from "./src/compact.ts";

// ---- prime-rlm cross-extension seam (lazy resolve; load order independent) ---

interface PrimeRlmSessionInfo {
	sessionId: string;
	sessionDir: string;
	cwd: string;
	agentDir: string;
	depth: number;
}

interface PrimeRlmHostApi {
	version: number;
	registerHostHandlerProvider(
		fn: (info: PrimeRlmSessionInfo) => Record<string, (payload: Record<string, unknown>) => unknown> | undefined,
	): void;
}

function getRlmHostApi(): PrimeRlmHostApi | undefined {
	const candidate = (globalThis as Record<symbol, unknown>)[Symbol.for("prime-rlm.host-api")];
	if (candidate && (candidate as PrimeRlmHostApi).version === 1) {
		return candidate as PrimeRlmHostApi;
	}
	return undefined;
}

// ---- per-session runtime --------------------------------------------------------

interface SessionRuntime {
	ctx?: ExtensionContext;
	goal: GoalRuntime;
	heartbeats: HeartbeatRuntime;
	compact: CompactRuntime;
	autonomous: AutonomousRuntimeState;
	lastAssistantMessage?: AgentMessage;
	/** Reentrancy guard: the settle pipeline is async (gates). */
	settleInFlight: boolean;
	/** A budget_limit context message owed to the model (set when the crossing
	 *  was detected mid-turn; delivered as soon as delivery is safe). */
	pendingBudgetLimit: boolean;
	seeded: boolean;
}

function positiveInt(value: unknown, name: string): number | undefined {
	if (value === undefined || value === false) return undefined;
	const raw = String(value);
	if (!/^\d+$/.test(raw) || Number.parseInt(raw, 10) <= 0) {
		throw new Error(`--${name} must be a positive integer`);
	}
	return Number.parseInt(raw, 10);
}

export default function (pi: ExtensionAPI) {
	// ---- CLI flags (PA cli/args.ts + cli/command-registry.ts help text) ---------
	pi.registerFlag("autonomous", {
		type: "boolean",
		description: "Continue until gates pass or a limit is reached",
	});
	pi.registerFlag("autonomous-gate", {
		type: "string",
		// Extension-land delta: pi flags are last-wins, so a single gate command
		// per flag. Chain gates with "&&" for a composite (PA allows repeats).
		description: "Run a completion gate (chain multiple gates with &&)",
	});
	pi.registerFlag("autonomous-gate-retries", {
		type: "string",
		description: "Set positive retries per failed gate (default: 3)",
	});
	pi.registerFlag("autonomous-gate-timeout-ms", {
		type: "string",
		description: "Set positive per-gate timeout in ms (default: 300000)",
	});
	pi.registerFlag("autonomous-max-continuations", {
		type: "string",
		description: "Set positive follow-up limit (default: 3)",
	});
	pi.registerFlag("autonomous-max-turns", {
		type: "string",
		description: "Set positive assistant-turn limit (default: 12)",
	});
	pi.registerFlag("autonomous-max-tokens", {
		type: "string",
		description: "Set positive token limit (default: 80000)",
	});
	pi.registerFlag("autonomous-timeout-ms", {
		type: "string",
		description: "Set positive wall-clock limit in ms (default: 1800000)",
	});
	pi.registerFlag("goal", {
		type: "string",
		description: "Start the session with an active thread goal objective",
	});
	pi.registerFlag("goal-token-budget", {
		type: "string",
		description: "Token budget for --goal (requires --goal)",
	});

	// ---- session runtimes ---------------------------------------------------------
	const runtimes = new Map<string, SessionRuntime>();

	function createRuntime(sessionId: string): SessionRuntime {
		const rt: SessionRuntime = {
			goal: undefined as unknown as GoalRuntime,
			heartbeats: undefined as unknown as HeartbeatRuntime,
			compact: undefined as unknown as CompactRuntime,
			autonomous: createAutonomousRuntimeState(autonomousConfigFromFlags()),
			settleInFlight: false,
			pendingBudgetLimit: false,
			seeded: false,
		};
		const getCtx = () => rt.ctx;
		rt.goal = new GoalRuntime(pi, getCtx);
		rt.heartbeats = new HeartbeatRuntime(pi, getCtx, {
			isCompacting: () => rt.compact.isCompacting(),
		});
		rt.compact = new CompactRuntime(getCtx, {
			onAfterCompaction: (lastRole) => {
				// PA _schedulePostCompactionContinue: resume without a message via
				// agent.continue(). Extensions cannot continue() without a message, so
				// when the last context message was not an assistant message we send a
				// minimal resume prompt (documented delta in M4-AUTONOMY.md).
				if (lastRole !== "assistant") {
					pi.sendUserMessage("Compaction complete. Continue where you left off.");
				}
			},
		});
		runtimes.set(sessionId, rt);
		return rt;
	}

	function runtimeFor(ctx: ExtensionContext): SessionRuntime {
		const sessionId = ctx.sessionManager.getSessionId();
		return runtimes.get(sessionId) ?? createRuntime(sessionId);
	}

	function autonomousConfigFromFlags(): AgentAutonomousConfig | undefined {
		const gates = pi.getFlag("autonomous-gate");
		const gateRetries = positiveInt(pi.getFlag("autonomous-gate-retries"), "autonomous-gate-retries");
		const gateTimeoutMs = positiveInt(pi.getFlag("autonomous-gate-timeout-ms"), "autonomous-gate-timeout-ms");
		const maxContinuations = positiveInt(pi.getFlag("autonomous-max-continuations"), "autonomous-max-continuations");
		const maxTurns = positiveInt(pi.getFlag("autonomous-max-turns"), "autonomous-max-turns");
		const maxTokens = positiveInt(pi.getFlag("autonomous-max-tokens"), "autonomous-max-tokens");
		const timeoutMs = positiveInt(pi.getFlag("autonomous-timeout-ms"), "autonomous-timeout-ms");
		const enabled =
			pi.getFlag("autonomous") === true ||
			gates !== undefined ||
			gateRetries !== undefined ||
			gateTimeoutMs !== undefined ||
			maxContinuations !== undefined ||
			maxTurns !== undefined ||
			maxTokens !== undefined ||
			timeoutMs !== undefined;
		if (!enabled) return undefined;
		const hasGateOptions = gates !== undefined || gateRetries !== undefined || gateTimeoutMs !== undefined;
		return {
			enabled: true,
			maxContinuations,
			maxTurns,
			maxTokens,
			timeoutMs,
			gates: hasGateOptions
				? {
						commands: typeof gates === "string" && gates.trim() ? [gates.trim()] : [],
						maxRetries: gateRetries,
						timeoutMs: gateTimeoutMs,
					}
				: undefined,
		};
	}

	/** PA _isBranchSeedable: only bootstrap entry types and no thread_goal_state. */
	function isBranchSeedable(ctx: ExtensionContext): boolean {
		for (const entry of ctx.sessionManager.getBranch() as unknown as Array<Record<string, unknown>>) {
			switch (entry?.type) {
				case "model_change":
				case "thinking_level_change":
				case "service_tier_change":
					continue;
				default:
					return false;
			}
		}
		return true;
	}

	// ---- kernel host handlers (goal.*, compact.*, rlm_heartbeat.*) ------------------
	getRlmHostApi()?.registerHostHandlerProvider((info) => {
		// Kernel host handlers are collected at first ipython execute (lazy
		// kernel), i.e. after session_start created the runtime. Create-if-missing
		// keeps rlm child sessions (whose event timing differs) functional too.
		const rt = runtimes.get(info.sessionId) ?? createRuntime(info.sessionId);
		return {
			"goal.get": async (payload) => rt.goal.handleHostRequest("goal.get", payload) as unknown as Record<string, unknown>,
			"goal.create": async (payload) =>
				rt.goal.handleHostRequest("goal.create", payload) as unknown as Record<string, unknown>,
			"goal.complete": async (payload) =>
				rt.goal.handleHostRequest("goal.complete", payload) as unknown as Record<string, unknown>,
			"compact.status": async (payload) => rt.compact.handleHostRequest("compact.status", payload),
			"compact.run": async (payload) => rt.compact.handleHostRequest("compact.run", payload),
			"rlm_heartbeat.list": async (payload) => rt.heartbeats.handleHostRequest("rlm_heartbeat.list", payload),
			"rlm_heartbeat.create": async (payload) => rt.heartbeats.handleHostRequest("rlm_heartbeat.create", payload),
			"rlm_heartbeat.update": async (payload) => rt.heartbeats.handleHostRequest("rlm_heartbeat.update", payload),
			"rlm_heartbeat.delete": async (payload) => rt.heartbeats.handleHostRequest("rlm_heartbeat.delete", payload),
		};
	});

	// ---- events ----------------------------------------------------------------------

	pi.on("session_start", async (_event, ctx) => {
		const rt = runtimeFor(ctx);
		rt.ctx = ctx;
		rt.lastAssistantMessage = undefined;
		rt.settleInFlight = false;
		rt.pendingBudgetLimit = false;
		rt.goal.restore();
		rt.heartbeats.reschedule();
		// PA agent-session L1340: --goal seeds only at depth 0 on a fresh branch.
		if (!rt.seeded && isBranchSeedable(ctx)) {
			const objective = pi.getFlag("goal");
			if (typeof objective === "string" && objective.trim()) {
				const budget = positiveInt(pi.getFlag("goal-token-budget"), "goal-token-budget");
				try {
					rt.goal.handleHostRequest("goal.create", {
						objective: validateGoalObjective(objective),
						token_budget: validateGoalBudget(budget),
					});
					rt.seeded = true;
					const msg = rt.goal.objectiveContextMessage("continuation");
					if (msg) {
						void pi.sendMessage(msg, { triggerTurn: false, deliverAs: "nextTurn" });
					}
				} catch (error) {
					ctx.ui.notify(`prime-autonomy: --goal seed failed: ${error instanceof Error ? error.message : error}`, "warning");
				}
			}
		}
	});

	pi.on("session_shutdown", async (_event, ctx) => {
		const rt = runtimes.get(ctx.sessionManager.getSessionId());
		rt?.heartbeats.stop();
	});

	pi.on("message_end", async (event, ctx) => {
		const rt = runtimeFor(ctx);
		rt.ctx = ctx;
		const message = event.message as AgentMessage & { stopReason?: string; errorMessage?: string };
		if (message.role !== "assistant") return;
		rt.lastAssistantMessage = message;
		// Goal accounting (PA _accountGoalUsageForAssistantMessage at message_end).
		if (rt.goal.accountAssistantMessage(message)) {
			// Budget newly crossed: PA steers the budget_limit context into the
			// active turn; at worst it is delivered at the next settle.
			rt.pendingBudgetLimit = true;
			try {
				const msg = rt.goal.objectiveContextMessage("budget_limit");
				if (msg) {
					if (ctx.isIdle()) {
						rt.pendingBudgetLimit = false;
						void pi.sendMessage(msg, { triggerTurn: true });
					} else {
						void pi.sendMessage(msg, { triggerTurn: false, deliverAs: "steer" });
					}
				}
			} catch {
				// delivery deferred to the settle pipeline
			}
		}
		rt.goal.noteTerminalAssistantMessage(message);
		// Autonomous usage accounting (PA addAutonomousUsage per assistant message).
		addAutonomousUsage(rt.autonomous, message.usage);
	});

	// ---- settle pipeline (PA _getContinuationMessages composition) --------------------
	async function runSettlePipeline(ctx: ExtensionContext): Promise<void> {
		const rt = runtimeFor(ctx);
		rt.ctx = ctx;
		if (rt.settleInFlight) return;
		rt.settleInFlight = true;
		try {
			// 1. A kernel-requested compaction consumes the settle (PA consumes
			//    _pendingRequestedCompaction before continuations are computed).
			if (rt.compact.runPendingAtSettle()) {
				return;
			}
			if (!ctx.isIdle() || ctx.hasPendingMessages()) {
				return;
			}
			// 2. Budget-limit context still owed (e.g. steer raced a turn end).
			if (rt.pendingBudgetLimit) {
				rt.pendingBudgetLimit = false;
				const msg = rt.goal.objectiveContextMessage("budget_limit");
				if (msg) {
					await pi.sendMessage(msg, { triggerTurn: true });
					return;
				}
			}
			// 3. Goal continuation first (PA _getGoalContinuationMessages).
			const goalMsg = rt.goal.continuationMessage();
			if (goalMsg) {
				await pi.sendMessage(goalMsg as GoalContextMessage, { triggerTurn: true });
				return;
			}
			// 4. Autonomous continuation (PA nextAutonomousContinuation). Skipped
			//    while a goal is/was driving this settle; PA composes both through
			//    the same continuation slot and the goal wins.
			if (rt.autonomous.enabled && rt.lastAssistantMessage) {
				try {
					const text = await nextAutonomousContinuation(rt.autonomous, rt.lastAssistantMessage as { stopReason?: string }, {
						cwd: ctx.cwd,
					});
					if (text) {
						pi.sendUserMessage(text);
					}
				} catch (error) {
					ctx.ui.notify(
						`prime-autonomy: autonomous continuation failed: ${error instanceof Error ? error.message : error}`,
						"warning",
					);
				}
			}
		} finally {
			rt.settleInFlight = false;
		}
	}

	pi.on("agent_settled", async (_event, ctx) => {
		try {
			await runSettlePipeline(ctx);
		} catch {
			// the settle pipeline must never crash the agent loop
		}
	});

	// ---- /goal command (PA _parseGoalSlashCommand subset) ------------------------------
	pi.registerCommand("goal", {
		description: "Manage the thread goal: /goal set <objective> | status | pause | resume | clear",
		handler: async (args, ctx) => {
			const rt = runtimeFor(ctx);
			rt.ctx = ctx;
			const text = (args ?? "").trim();
			const [sub, ...rest] = text.split(/\s+/);
			const remainder = rest.join(" ").trim();
			try {
				switch (sub) {
					case "set": {
						if (!remainder) {
							ctx.ui.notify("Usage: /goal set <objective>", "warning");
							return;
						}
						rt.goal.handleHostRequest("goal.create", {
							objective: validateGoalObjective(remainder),
							token_budget: undefined,
						});
						const msg = rt.goal.objectiveContextMessage("continuation");
						if (msg) await pi.sendMessage(msg, { triggerTurn: true });
						ctx.ui.notify("Goal set.", "info");
						return;
					}
					case "status":
					case "":
					case undefined:
						ctx.ui.notify(rt.goal.formatStatus(), "info");
						return;
					case "pause":
						rt.goal.pause();
						ctx.ui.notify("Goal paused.", "info");
						return;
					case "resume": {
						const reactivate = rt.goal.resume();
						ctx.ui.notify(reactivate ? "Goal resumed." : rt.goal.formatStatus(), "info");
						if (reactivate) {
							const msg = rt.goal.objectiveContextMessage("continuation");
							if (msg) await pi.sendMessage(msg, { triggerTurn: true });
						}
						return;
					}
					case "clear":
						rt.goal.clear();
						ctx.ui.notify("Goal cleared.", "info");
						return;
					default:
						ctx.ui.notify("Usage: /goal set <objective> | status | pause | resume | clear", "warning");
				}
			} catch (error) {
				ctx.ui.notify(`goal: ${error instanceof Error ? error.message : error}`, "error");
			}
		},
	});

	// ---- /autonomous command (PA _parseAutonomousSlashCommand) -------------------------
	pi.registerCommand("autonomous", {
		description: "Autonomous mode: /autonomous [on|off|status]",
		handler: async (args, ctx) => {
			const rt = runtimeFor(ctx);
			rt.ctx = ctx;
			const text = (args ?? "").trim().toLowerCase();
			if (text === "on") {
				setAutonomousEnabled(rt.autonomous, true);
				ctx.ui.notify("Autonomous mode enabled.", "info");
				return;
			}
			if (text === "off") {
				setAutonomousEnabled(rt.autonomous, false);
				ctx.ui.notify("Autonomous mode disabled.", "info");
				return;
			}
			if (text === "" || text === "status") {
				const s = autonomousStatus(rt.autonomous);
				ctx.ui.notify(
					[
						`Autonomous: ${s.enabled ? "on" : "off"}`,
						`Continuations: ${s.continuationsUsed}/${s.limits.maxContinuations}`,
						`Turns: ${s.turnsUsed}/${s.limits.maxTurns}`,
						`Tokens: ${s.tokensUsed}/${s.limits.maxTokens}`,
						`Gates: ${s.gates.commands.length > 0 ? s.gates.commands.join("; ") : "none"}`,
						s.lastGateFailure
							? `Last gate failure: ${s.lastGateFailure.command} (attempt ${s.lastGateFailure.attempt})`
							: undefined,
					]
						.filter(Boolean)
						.join("\n"),
					"info",
				);
				return;
			}
			ctx.ui.notify("Usage: /autonomous [on|off|status]", "warning");
		},
	});
}
