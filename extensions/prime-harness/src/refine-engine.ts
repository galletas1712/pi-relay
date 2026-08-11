// prime-harness refine engine (M2) — two-phase /refine orchestration ported
// from prime-agent's core/agent-session.ts wiring (refine(), _planRefine,
// _applyRefine, _runBackgroundPlan, _maybeAutoRefine, autoRefineInstructions).
//
// PA relies on fork-internal seams (assistant message_end hook to start
// background planning mid-turn, shouldStopAfterTurn as the quiescent apply
// boundary, agent.waitForIdle, event-queue draining). Upstream pi gives
// extensions these equivalents:
//   - planning start: refine.run arrives from the kernel DURING tool
//     execution, so background planning starts immediately on request —
//     equivalent timing to PA's message_end hook.
//   - apply boundary: the `agent_settled` extension event (post-run
//     quiescence). If the session is already idle (manual /refine between
//     turns), apply runs immediately inside the command handler.
//   - turn-entry barrier: `before_agent_start` awaits any in-flight apply
//     (file I/O only, sub-10ms) — PA's _waitForRefineIdle equivalent.
//   - auto-refine triggers: `turn_end` (assistant-turn counter →
//     turn_interval) and `session_before_compact` (compact flag consumed at
//     the next agent_settled).
//
// Invariants (PA parity):
//   - planning NEVER blocks the conversation (background completeSimple call)
//   - apply is the only blocking phase and is brief (re-read file, apply
//     edits, atomic save, append history, session custom entry)
//   - the base system prompt is immutable; refine only edits harness files,
//     which before_agent_start re-reads on every prompt build
//   - kernel rlm.harness writes during planning are not clobbered:
//     baselineState conflict rejection in applyRefinementProposal

import { appendFileSync, existsSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { sessionEntryToContextMessages, type ExtensionAPI, type ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { Model } from "@earendil-works/pi-ai/compat";
import { globalHarnessDir, localHarnessDir, sessionStateDir } from "./paths.ts";
import {
	appendGlobalRefinement,
	applyRefinementProposal,
	getRefinementHistory,
	inferRefinementResultScope,
	loadGlobalRefinementHistory,
	loadHarnessState,
	mergeHarnessStates,
	mergeRefinementHistory,
	planRefinement,
	REFINEMENT_CUSTOM_TYPE,
	reviewAutoRefine,
	saveHarnessState,
	type AutoRefineReason,
	type AutoRefineReview,
	type HarnessScope,
	type RefinementPlan,
	type RefinementResult,
	type RefineOptions,
} from "./store.ts";

// ---- settings (PA: settings.autoRefine; M2: env-tunable, PA-identical defaults) ---

export interface AutoRefineSettings {
	enabled: boolean;
	turnInterval: number;
	compact: boolean;
	cooldownMs: number;
}

export function getAutoRefineSettings(): AutoRefineSettings {
	const env = process.env;
	const enabled = (env.PRIME_REFINE_AUTO ?? "true").toLowerCase() !== "false";
	const turnIntervalRaw = Number(env.PRIME_REFINE_TURN_INTERVAL ?? "25");
	const cooldownRaw = Number(env.PRIME_REFINE_COOLDOWN_MS ?? String(20 * 60_000));
	return {
		enabled,
		turnInterval: Number.isFinite(turnIntervalRaw) ? Math.max(1, Math.floor(turnIntervalRaw)) : 25,
		compact: (env.PRIME_REFINE_COMPACT ?? "true").toLowerCase() !== "false",
		cooldownMs: Number.isFinite(cooldownRaw) ? Math.max(0, cooldownRaw) : 20 * 60_000,
	};
}

/** Ported verbatim from prime-agent's agent-session.ts autoRefineInstructions. */
function autoRefineInstructions(reason: AutoRefineReason, review: AutoRefineReview): string {
	const detail = review.instructions
		? `
Reviewer instructions: ${review.instructions}`
		: "";
	return `Automatic refine review triggered by ${reason}. Only create/update/delete local harness entries if there is clear evidence that should help this session continue. Prefer an empty edits array over speculative or one-off memories. Do not promote anything global unless explicitly requested. Reviewer rationale: ${review.rationale}${detail}`;
}

// ---- per-session engine state -------------------------------------------------

export interface RefineEngineState {
	sessionId: string;
	pi: ExtensionAPI;
	ctx?: ExtensionContext;
	agentDir: string;

	/** refine.run requested but planning not yet started (coalescing point). */
	pendingRequest?: RefineOptions;
	/** Background planning promise (never blocks turns). */
	planInFlight?: Promise<RefinementPlan>;
	/** Finished plan awaiting the apply boundary. */
	pendingPlan?: { plan: RefinementPlan; options: RefineOptions };
	/** In-flight apply (the ONLY phase turn entry waits on). */
	applyInFlight?: Promise<void>;
	lastResult?: RefinementResult;
	lastError?: string;

	// auto-refine bookkeeping
	turnsSinceReview: number;
	lastReviewAt: number;
	compactReviewPending: boolean;
	reviewInFlight?: Promise<void>;
}

const engines = new Map<string, RefineEngineState>();

export function registerEngine(sessionId: string, pi: ExtensionAPI, agentDir: string): RefineEngineState {
	let engine = engines.get(sessionId);
	if (!engine) {
		engine = {
			sessionId,
			pi,
			agentDir,
			turnsSinceReview: 0,
			lastReviewAt: 0,
			compactReviewPending: false,
		};
		engines.set(sessionId, engine);
	} else {
		engine.pi = pi;
		engine.agentDir = agentDir;
	}
	return engine;
}

export function getEngine(sessionId: string): RefineEngineState | undefined {
	return engines.get(sessionId);
}

export function engineStatus(engine: RefineEngineState): Record<string, unknown> {
	return {
		pending: engine.pendingRequest !== undefined || engine.pendingPlan !== undefined,
		in_flight: engine.planInFlight !== undefined || engine.applyInFlight !== undefined,
		lastResult: engine.lastResult?.id,
		lastError: engine.lastError,
	};
}

// ---- helpers ------------------------------------------------------------------

async function resolveModelAuth(ctx: ExtensionContext): Promise<{
	model: Model<any>;
	auth: { apiKey?: string; headers?: Record<string, string> };
}> {
	const model = ctx.model;
	if (!model) throw new Error("refine requires an active model");
	const resolved = await ctx.modelRegistry.getApiKeyAndHeaders(model);
	if (!resolved.ok) throw new Error(`model auth resolution failed: ${resolved.error}`);
	const headers = Object.fromEntries(
		Object.entries(resolved.headers ?? {}).filter(([, v]) => typeof v === "string"),
	) as Record<string, string>;
	return { model, auth: { apiKey: resolved.apiKey, headers } };
}

function contextMessages(ctx: ExtensionContext) {
	return ctx.sessionManager.buildContextEntries().flatMap((entry) => sessionEntryToContextMessages(entry));
}

function loadMergedHistory(engine: RefineEngineState, ctx: ExtensionContext) {
	const gDir = globalHarnessDir(engine.agentDir);
	return mergeRefinementHistory(loadGlobalRefinementHistory(gDir), getRefinementHistory(ctx.sessionManager.getEntries()));
}

function logResult(engine: RefineEngineState, ctx: ExtensionContext, result: RefinementResult): void {
	try {
		const dir = sessionStateDir(ctx);
		mkdirSync(dir, { recursive: true });
		appendFileSync(join(dir, "refine-results.jsonl"), `${JSON.stringify(result)}\n`, "utf8");
	} catch {
		// observability only
	}
}

function notify(ctx: ExtensionContext | undefined, message: string, level: "info" | "warning" | "error" = "info"): void {
	try {
		ctx?.ui.notify(message, level);
	} catch {
		// notification is best-effort
	}
}

// ---- planning phase (background; never blocks the conversation) ---------------

async function planPhase(engine: RefineEngineState, ctx: ExtensionContext, options: RefineOptions): Promise<RefinementPlan> {
	const { model, auth } = await resolveModelAuth(ctx);
	const gDir = globalHarnessDir(engine.agentDir);
	const lDir = localHarnessDir(ctx);
	const requestedScope: HarnessScope = options.global ? "global" : "local";

	const globalPlanningState = loadHarnessState(gDir, "global");
	const localPlanningState = loadHarnessState(lDir, "local");
	const planningState =
		requestedScope === "global" ? globalPlanningState : mergeHarnessStates(globalPlanningState, localPlanningState);
	const history = loadMergedHistory(engine, ctx);

	// PA parity: rollback plans resolve their baseline from the recorded
	// harnessStatePath so local/global targeting survives legacy records.
	const rollbackTarget = options.rollbackId ? history.find((item) => item.id === options.rollbackId) : undefined;
	let baselineScope: HarnessScope = rollbackTarget
		? (inferRefinementResultScope(rollbackTarget) ?? requestedScope)
		: requestedScope;
	let baselineDir = baselineScope === "global" ? gDir : lDir;
	if (rollbackTarget?.harnessStatePath) {
		baselineDir = dirname(rollbackTarget.harnessStatePath);
		baselineScope = resolve(baselineDir) === resolve(gDir) ? "global" : "local";
	}
	const baselineState = loadHarnessState(baselineDir, baselineScope);

	const plan = await planRefinement(
		contextMessages(ctx),
		planningState,
		history,
		model,
		auth,
		options,
		ctx.signal,
	);
	return { ...plan, baselineState };
}

// ---- apply phase (fast; the only phase turn entry waits on) -------------------

async function applyPhase(
	engine: RefineEngineState,
	ctx: ExtensionContext,
	plan: RefinementPlan,
	options: RefineOptions,
): Promise<RefinementResult> {
	const gDir = globalHarnessDir(engine.agentDir);
	const lDir = localHarnessDir(ctx);
	const requestedScope: HarnessScope = options.global ? "global" : "local";
	const history = loadMergedHistory(engine, ctx);
	const rollbackTarget = options.rollbackId ? history.find((item) => item.id === options.rollbackId) : undefined;
	let targetScope: HarnessScope = plan.rollbackScope ?? requestedScope;
	let targetDir = targetScope === "global" ? gDir : lDir;
	if (targetScope === "local" && rollbackTarget?.harnessStatePath) {
		if (!existsSync(rollbackTarget.harnessStatePath)) {
			throw new Error(`Local refinement ${rollbackTarget.id} state file not found: ${rollbackTarget.harnessStatePath}`);
		}
		targetDir = dirname(rollbackTarget.harnessStatePath);
		if (resolve(targetDir) === resolve(gDir)) {
			targetScope = "global";
		}
	}

	// Re-read the target state immediately before applying so concurrent kernel
	// (rlm.harness) writes during the LLM pass are not clobbered.
	const state = loadHarnessState(targetDir, targetScope);
	// Strip display-only scope prefixes from edit ids (PA parity).
	const proposal = {
		...plan.proposal,
		edits: plan.proposal.edits.map((edit) => ({
			...edit,
			id: edit.id?.startsWith("local:")
				? edit.id.slice("local:".length)
				: edit.id?.startsWith("global:")
					? edit.id.slice("global:".length)
					: edit.id,
		})),
	};
	const result = applyRefinementProposal(state, proposal, {
		id: plan.id,
		rollbackOf: plan.rollbackOf,
		scope: targetScope,
		baselineState: plan.baselineState,
	});
	result.harnessStatePath = saveHarnessState(targetDir, state);
	if (targetScope === "global") {
		appendGlobalRefinement(gDir, result);
	}
	engine.pi.appendEntry(REFINEMENT_CUSTOM_TYPE, result);
	logResult(engine, ctx, result);
	return result;
}

function notifyResult(ctx: ExtensionContext | undefined, result: RefinementResult): void {
	const applied = result.appliedEdits.filter((e) => e.applied).length;
	const failed = result.appliedEdits.length - applied;
	notify(
		ctx,
		`Refinement ${result.id} (${result.scope ?? "local"}): ${result.summary} — ${applied} edit(s) applied${failed > 0 ? `, ${failed} failed` : ""}`,
		failed > 0 ? "warning" : "info",
	);
}

// ---- orchestration -------------------------------------------------------------

/**
 * Queue a refine request (kernel refine.run or busy /refine). Coalesces with any
 * in-flight or pending refine by updating instructions, then ensures background
 * planning is running. Never blocks the conversation.
 */
export function requestRefine(engine: RefineEngineState, options: RefineOptions): Record<string, unknown> {
	if (!engine.ctx) {
		return { scheduled: false, reason: "session context not bound yet" };
	}
	// PA parity: calling run again before the refine lands only updates the
	// pending instructions (one request per turn is enough).
	if (engine.pendingPlan) {
		if (options.instructions) engine.pendingPlan.options.instructions = options.instructions;
		return { scheduled: true, coalesced: "pending_plan" };
	}
	if (engine.pendingRequest) {
		if (options.instructions) engine.pendingRequest.instructions = options.instructions;
		return { scheduled: true, coalesced: "pending_request" };
	}
	engine.pendingRequest = { ...options };
	if (!engine.planInFlight && !engine.applyInFlight) {
		void startBackgroundPlan(engine);
	}
	// If a plan/apply is in flight, the queued request drains when it finishes.
	return { scheduled: true, coalesced: engine.planInFlight || engine.applyInFlight ? "queued_after_in_flight" : undefined };
}

/** Kick planning for a queued request once no plan/apply is in flight. */
function drainRequestQueue(engine: RefineEngineState): void {
	if (engine.pendingRequest && !engine.planInFlight && !engine.applyInFlight && !engine.pendingPlan) {
		void startBackgroundPlan(engine);
	}
}

/** Start the background planning phase for the pending request, if any. */
function startBackgroundPlan(engine: RefineEngineState): void {
	const ctx = engine.ctx;
	const options = engine.pendingRequest;
	if (!ctx || !options) return;
	engine.pendingRequest = undefined;
	const planRun = planPhase(engine, ctx, options);
	engine.planInFlight = planRun;
	planRun
		.then((plan) => {
			engine.planInFlight = undefined;
			engine.pendingPlan = { plan, options };
			// Apply immediately when idle; otherwise agent_settled consumes it.
			if (engine.ctx?.isIdle()) {
				void applyPendingPlan(engine);
			}
		})
		.catch((error) => {
			engine.planInFlight = undefined;
			engine.lastError = error instanceof Error ? error.message : String(error);
			engine.lastReviewAt = Date.now(); // cooldown so a persistent failure doesn't spin
			notify(engine.ctx, `Refine planning failed: ${engine.lastError}`, "warning");
			drainRequestQueue(engine);
		});
}

/** Apply the finished plan (agent_settled boundary or immediate-when-idle). */
export async function applyPendingPlan(engine: RefineEngineState): Promise<void> {
	const pending = engine.pendingPlan;
	const ctx = engine.ctx;
	if (!pending || !ctx || engine.applyInFlight) return;
	if (!ctx.isIdle()) return; // boundary only; never mid-run
	const applyRun = (async () => {
		try {
			const result = await applyPhase(engine, ctx, pending.plan, pending.options);
			engine.lastResult = result;
			engine.lastError = undefined;
			engine.turnsSinceReview = 0;
			engine.lastReviewAt = Date.now();
			notifyResult(ctx, result);
		} catch (error) {
			engine.lastError = error instanceof Error ? error.message : String(error);
			engine.lastReviewAt = Date.now(); // stamp cooldown so failures don't spin
			notify(ctx, `Refine apply failed: ${engine.lastError}`, "warning");
		}
	})();
	engine.applyInFlight = applyRun;
	engine.pendingPlan = undefined;
	try {
		await applyRun;
	} finally {
		if (engine.applyInFlight === applyRun) engine.applyInFlight = undefined;
		drainRequestQueue(engine);
	}
}

/** agent_settled hook: apply pending plan, then consider auto-refine review. */
export async function onAgentSettled(engine: RefineEngineState, ctx: ExtensionContext): Promise<void> {
	engine.ctx = ctx;
	await applyPendingPlan(engine);
	if (engine.compactReviewPending) {
		engine.compactReviewPending = false;
		await maybeAutoRefine(engine, ctx, "compact");
	}
}

/** turn_end hook: count assistant turns; trigger interval auto-refine review. */
export async function onTurnEnd(engine: RefineEngineState, ctx: ExtensionContext): Promise<void> {
	engine.ctx = ctx;
	engine.turnsSinceReview++;
	await maybeAutoRefine(engine, ctx, "turn_interval");
}

/** session_before_compact hook: remember to run a compact-reason review after. */
export function onBeforeCompact(engine: RefineEngineState): void {
	engine.compactReviewPending = true;
}

async function maybeAutoRefine(engine: RefineEngineState, ctx: ExtensionContext, reason: AutoRefineReason): Promise<void> {
	const settings = getAutoRefineSettings();
	if (!settings.enabled) return;
	if (reason === "compact" && !settings.compact) reason = "turn_interval";
	if (reason === "turn_interval" && engine.turnsSinceReview < settings.turnInterval) return;
	if (engine.planInFlight || engine.applyInFlight || engine.pendingPlan || engine.pendingRequest || engine.reviewInFlight) return;
	const nowMs = Date.now();
	if (engine.lastReviewAt > 0 && nowMs - engine.lastReviewAt < settings.cooldownMs) return;

	const reviewRun = (async () => {
		try {
			const { model, auth } = await resolveModelAuth(ctx);
			const gDir = globalHarnessDir(engine.agentDir);
			const merged = mergeHarnessStates(
				loadHarnessState(gDir, "global"),
				loadHarnessState(localHarnessDir(ctx), "local"),
			);
			const review = await reviewAutoRefine(
				contextMessages(ctx),
				merged,
				loadMergedHistory(engine, ctx),
				model,
				auth,
				{ reason, turnsSinceLastReview: engine.turnsSinceReview },
			);
			engine.lastReviewAt = Date.now();
			// Observability for R4: every auto-refine review decision is logged.
			try {
				const dir = sessionStateDir(ctx);
				mkdirSync(dir, { recursive: true });
				appendFileSync(
					join(dir, "refine-results.jsonl"),
					`${JSON.stringify({ type: "auto_review", ts: new Date().toISOString(), reason, review })}\n`,
					"utf8",
				);
			} catch {
				// observability only
			}
			if (!review.shouldRefine) {
				engine.turnsSinceReview = 0;
				return;
			}
			requestRefine(engine, { instructions: autoRefineInstructions(reason, review) });
		} catch (error) {
			// Failed review: stamp cooldown so a persistent failure doesn't retry every turn.
			engine.lastReviewAt = Date.now();
			engine.lastError = error instanceof Error ? error.message : String(error);
		}
	})();
	engine.reviewInFlight = reviewRun;
	try {
		await reviewRun;
	} finally {
		if (engine.reviewInFlight === reviewRun) engine.reviewInFlight = undefined;
	}
}

/**
 * Full manual /refine (PA refine() equivalent): plan, then apply once idle.
 * Used by the /refine command when the session is between turns.
 */
export async function refineNow(engine: RefineEngineState, ctx: ExtensionContext, options: RefineOptions): Promise<RefinementResult> {
	engine.ctx = ctx;
	// Serialize against any in-flight refine activity.
	while (engine.planInFlight || engine.applyInFlight) {
		if (engine.planInFlight) await engine.planInFlight.catch(() => undefined);
		if (engine.applyInFlight) await engine.applyInFlight;
	}
	// A queued background plan takes precedence: apply it directly (PA applies
	// the exact background plan without re-planning).
	if (engine.pendingPlan) {
		await waitForIdle(ctx);
		await applyPendingPlan(engine);
		if (!engine.lastResult) throw new Error(engine.lastError ?? "refine apply failed");
		return engine.lastResult;
	}
	const plan = await planPhase(engine, ctx, options);
	await waitForIdle(ctx);
	const applyRun = (async () => {
		const result = await applyPhase(engine, ctx, plan, options);
		engine.lastResult = result;
		engine.lastError = undefined;
		engine.turnsSinceReview = 0;
		engine.lastReviewAt = Date.now();
		return result;
	})();
	engine.applyInFlight = applyRun.then(
		() => undefined,
		() => undefined,
	);
	try {
		return await applyRun;
	} finally {
		engine.applyInFlight = undefined;
	}
}

/** Poll ctx.isIdle() (ExtensionContext has no waitForIdle; command ctx does). */
async function waitForIdle(ctx: ExtensionContext): Promise<void> {
	while (!ctx.isIdle()) {
		await new Promise((r) => setTimeout(r, 50));
	}
}

/** before_agent_start barrier: turn entry waits ONLY on the apply phase. */
export async function waitForRefineApplyIdle(engine: RefineEngineState): Promise<void> {
	while (engine.applyInFlight) {
		await engine.applyInFlight;
	}
}
