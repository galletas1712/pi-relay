// Kernel-driven compaction scheduling for prime-autonomy (M4, G4).
//
// PROVENANCE: ported from prime-agent
//   packages/coding-agent/src/core/agent-session.ts handleCompactHostRequest +
//   _runAutoCompaction/_schedulePostCompactionContinue.
// Extension-land adaptations:
// - pi's prepareCompaction() is not exported from the package root, so the
//   scheduled:false pre-checks replicate its two falsy conditions directly:
//   last branch entry is a compaction ("already compacted") and fewer than two
//   message entries on the branch ("session is too short to compact").
// - Execution at agent_settled via ctx.compact({customInstructions}) (pi
//   session.compact() aborts the active run first, so it must not be called
//   mid-turn; PA schedules at the same turn boundary).
// - PA resumes post-compaction with agent.continue() (no message), which pi's
//   extension API cannot do; instead we resume with a short user message only
//   when the last context message is not an assistant message (PA
//   _continueAfterThresholdCompaction parity).
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";

export interface PendingRequestedCompaction {
	customInstructions?: string;
}

/**
 * Minimal port of pi core/compaction/compaction.js prepareCompaction()'s two
 * falsy conditions, so compact.run can return PA's exact scheduled:false
 * reasons synchronously (pi does not export prepareCompaction to extensions).
 */
export function compactionPreparationReason(ctx: ExtensionContext): "already compacted" | "session is too short to compact" | undefined {
	const branch = ctx.sessionManager.getBranch() as unknown as Array<Record<string, unknown>>;
	if (branch.length > 0 && branch[branch.length - 1]?.type === "compaction") {
		return "already compacted";
	}
	let messages = 0;
	for (const entry of branch) {
		if (entry?.type === "message") messages++;
		if (messages >= 2) return undefined;
	}
	return "session is too short to compact";
}

export interface CompactRuntimeOptions {
	/** Resumed continuation after a completed compaction (goal/autonomous pipeline from index.ts). */
	onAfterCompaction?: (lastRole: string | undefined) => void;
}

export class CompactRuntime {
	private pending: PendingRequestedCompaction | undefined;
	private running = false;

	constructor(
		private readonly getCtx: () => ExtensionContext | undefined,
		private readonly options: CompactRuntimeOptions = {},
	) {}

	isCompacting(): boolean {
		return this.running;
	}

	hasPending(): boolean {
		return this.pending !== undefined;
	}

	/** PA handleCompactHostRequest. Host requests only arrive mid-turn (the kernel
	 *  only runs while a turn is active), so PA's !isStreaming check is implied. */
	handleHostRequest(type: string, payload: Record<string, unknown> = {}): Record<string, unknown> {
		switch (type) {
			case "compact.status": {
				const ctx = this.getCtx();
				const usage = ctx?.getContextUsage();
				return {
					tokens: usage?.tokens ?? null,
					context_window: usage?.contextWindow ?? null,
					percent: usage?.percent ?? null,
					scheduled: this.pending !== undefined,
				};
			}
			case "compact.run": {
				const instructions = payload.instructions;
				if (instructions !== undefined && typeof instructions !== "string") {
					throw new Error("compact.run instructions must be a string when provided");
				}
				const ctx = this.getCtx();
				if (!ctx) {
					return { scheduled: false, reason: "no active turn; compaction can only be requested while a turn is running" };
				}
				const reason = compactionPreparationReason(ctx);
				if (reason) {
					return { scheduled: false, reason };
				}
				this.pending = { customInstructions: instructions };
				return {
					scheduled: true,
					note: "Compaction runs when the current turn ends; you resume automatically afterwards. Continue working normally.",
				};
			}
			default:
				throw new Error(`unknown compact request type "${type}"`);
		}
	}

	/**
	 * Consume a pending kernel-requested compaction at the turn boundary.
	 * Returns true when a compaction was triggered (the settle pipeline should
	 * stop; the onComplete/onError callback drives the resume).
	 */
	runPendingAtSettle(): boolean {
		const pending = this.pending;
		if (!pending) return false;
		const ctx = this.getCtx();
		if (!ctx) {
			this.pending = undefined;
			return false;
		}
		this.pending = undefined;
		this.running = true;
		ctx.compact({
			customInstructions: pending.customInstructions,
			onComplete: () => {
				this.running = false;
				this.maybeResumeAfterCompaction();
			},
			onError: () => {
				this.running = false;
			},
		});
		return true;
	}

	/** PA _schedulePostCompactionContinue parity, minus agent.continue(). */
	private maybeResumeAfterCompaction(): void {
		const ctx = this.getCtx();
		if (!ctx || !ctx.isIdle() || ctx.hasPendingMessages()) return;
		const branch = ctx.sessionManager.getBranch() as unknown as Array<Record<string, unknown>>;
		let lastRole: string | undefined;
		for (let i = branch.length - 1; i >= 0; i--) {
			const entry = branch[i];
			if (entry?.type === "message") {
				lastRole = (entry.message as { role?: string } | undefined)?.role;
				break;
			}
		}
		this.options.onAfterCompaction?.(lastRole);
	}
}
