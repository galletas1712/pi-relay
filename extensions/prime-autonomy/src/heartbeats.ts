// Internal RLM heartbeats for prime-autonomy (M4, G2).
//
// PROVENANCE: ported from prime-agent
//   packages/coding-agent/src/core/cron-jobs.ts (rlm_heartbeat job model,
//   interval parsing incl. 10s minimum, deferral rules, host response shape)
//   and the heartbeat blocks of packages/coding-agent/src/modes/daemon/
//   daemon-mode.ts (runCronJob heartbeat delivery: prompt the raw instruction;
//   steer interrupts the current turn, follow_up waits).
// Extension-land adaptations:
// - Interval-only schedules: PA's cron text parsing is reduced to what the
//   rlm-heartbeat skill accepts ("every 30s" / "30s" / "every 5m"), preserving
//   the >=10s minimum and "must be recurring" validation. One-shot ("in 5m",
//   "at <iso>") and cron-expression schedules are not supported here.
// - Storage is a small per-session JSON file at
//   `<sessionFile>.heartbeats.json` (PA uses daemon-level scheduled-jobs.json
//   with proper-lockfile; a single extension-owned session never races).
// - Delivery via pi.sendMessage: idle -> {triggerTurn:true}; busy ->
//   {triggerTurn:true, deliverAs:"steer"|"followUp"}. Deferred when a
//   compaction is pending/running or messages are already queued (PA
//   shouldDeferHeartbeatCronJob parity).
import { randomUUID } from "node:crypto";
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { dirname } from "node:path";
import type { ExtensionAPI, ExtensionContext } from "@earendil-works/pi-coding-agent";

export type RlmHeartbeatStatus = "active" | "paused" | "cancelled";
export type RlmHeartbeatDeliveryMode = "steer" | "follow_up";

export interface RlmHeartbeat {
	id: string;
	status: RlmHeartbeatStatus;
	source: "rlm_heartbeat";
	deliveryMode: RlmHeartbeatDeliveryMode;
	label?: string;
	prompt: string;
	schedule: { kind: "interval"; expression: string; intervalMs: number };
	createdAt: string;
	updatedAt: string;
	nextRunAt?: string;
	lastRunAt?: string;
	lastError?: string;
	runCount: number;
}

const ONE_SECOND_MS = 1000;
const ONE_MINUTE_MS = 60_000;
export const DEFAULT_HEARTBEAT_SCHEDULE = "every 5m";
export const DEFAULT_HEARTBEAT_DELIVERY_MODE: RlmHeartbeatDeliveryMode = "steer";

/** PA parseAgentCronSchedule, restricted to recurring "every <n><unit>" forms. */
function parseHeartbeatInterval(input: string, now = new Date()): { intervalMs: number; nextRunAt: Date } {
	const text = input.trim().replace(/^"(.*)"$/, "$1").replace(/^'(.*)'$/, "$1").trim();
	if (!text) {
		throw new Error("Heartbeat schedule cannot be empty");
	}
	const everyMatch =
		/^(?:every|each)?\s*(\d+)\s*(s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours)$/i.exec(
			text,
		);
	if (!everyMatch) {
		throw new Error(
			'Unsupported heartbeat schedule. Use a recurring interval such as "every 30s", "every 5m", or "every 1h".',
		);
	}
	const amount = Number.parseInt(everyMatch[1]!, 10);
	const unit = everyMatch[2]!.toLowerCase();
	const multiplier = unit.startsWith("s") ? ONE_SECOND_MS : unit.startsWith("m") ? ONE_MINUTE_MS : 60 * ONE_MINUTE_MS;
	const intervalMs = amount * multiplier;
	if (intervalMs < 10 * ONE_SECOND_MS) {
		throw new Error("Recurring interval must be at least 10 seconds");
	}
	return { intervalMs, nextRunAt: new Date(now.getTime() + intervalMs) };
}

export function normalizeHeartbeatSchedule(input: string | undefined): string {
	const text = input?.trim();
	if (!text) {
		return DEFAULT_HEARTBEAT_SCHEDULE;
	}
	if (/^\d+\s*(s|sec|secs|second|seconds|m|min|mins|minute|minutes|h|hr|hrs|hour|hours)$/i.test(text)) {
		return `every ${text}`;
	}
	return text;
}

export function normalizeHeartbeatDeliveryMode(value: unknown): RlmHeartbeatDeliveryMode | undefined {
	if (value === undefined || value === null) {
		return undefined;
	}
	if (value === "steer" || value === "follow_up") {
		return value;
	}
	throw new Error('Heartbeat delivery mode must be "steer" or "follow_up"');
}

/** PA rlmHeartbeatHostResponse (agent-session.ts): snake_case payload for the kernel skill. */
export function rlmHeartbeatHostResponse(job: RlmHeartbeat): Record<string, unknown> {
	return {
		id: job.id,
		status: job.status,
		label: job.label ?? null,
		delivery_mode: job.deliveryMode ?? "steer",
		instruction: job.prompt,
		schedule: job.schedule,
		created_at: job.createdAt,
		updated_at: job.updatedAt,
		next_run_at: job.nextRunAt ?? null,
		last_run_at: job.lastRunAt ?? null,
		last_error: job.lastError ?? null,
		run_count: job.runCount,
	};
}

interface HeartbeatsFile {
	heartbeats?: unknown;
}

function isRlmHeartbeat(value: unknown): value is RlmHeartbeat {
	if (!value || typeof value !== "object") return false;
	const r = value as Record<string, unknown>;
	return (
		typeof r.id === "string" &&
		(r.status === "active" || r.status === "paused" || r.status === "cancelled") &&
		typeof r.prompt === "string" &&
		typeof r.runCount === "number" &&
		!!r.schedule &&
		typeof (r.schedule as Record<string, unknown>).intervalMs === "number"
	);
}

export interface HeartbeatRuntimeOptions {
	/** True while a kernel-requested compaction is pending or running (PA: isCompacting defers). */
	isCompacting?: () => boolean;
}

/**
 * Per-session heartbeat runtime: store + single-timer scheduler + host handlers.
 */
export class HeartbeatRuntime {
	private heartbeats: RlmHeartbeat[] = [];
	private timer: NodeJS.Timeout | undefined;
	private loaded = false;

	constructor(
		private readonly pi: ExtensionAPI,
		private readonly getCtx: () => ExtensionContext | undefined,
		private readonly options: HeartbeatRuntimeOptions = {},
	) {}

	private storePath(): string | undefined {
		const ctx = this.getCtx();
		const sessionFile = ctx?.sessionManager.getSessionFile();
		if (!sessionFile) return undefined;
		return `${sessionFile}.heartbeats.json`;
	}

	private load(): void {
		if (this.loaded) return;
		this.loaded = true;
		const path = this.storePath();
		if (!path || !existsSync(path)) return;
		try {
			const raw = JSON.parse(readFileSync(path, "utf8")) as HeartbeatsFile;
			if (Array.isArray(raw.heartbeats)) {
				this.heartbeats = raw.heartbeats.filter(isRlmHeartbeat);
			}
		} catch {
			// corrupt store: start clean rather than crashing the session
		}
	}

	private persist(): void {
		const path = this.storePath();
		if (!path) return;
		try {
			mkdirSync(dirname(path), { recursive: true });
			const tmp = `${path}.tmp`;
			writeFileSync(tmp, JSON.stringify({ heartbeats: this.heartbeats }, null, 2));
			renameSync(tmp, path);
		} catch {
			// persistence must never crash the agent loop
		}
	}

	// ---- CRUD (PA AgentCronJobStore rlm-heartbeat methods) ----------------------

	listRlmHeartbeats(options: { includeInactive?: boolean } = {}): RlmHeartbeat[] {
		this.load();
		return this.heartbeats
			.filter((job) => options.includeInactive || job.status === "active" || job.status === "paused")
			.sort((a, b) => a.createdAt.localeCompare(b.createdAt));
	}

	createRlmHeartbeat(input: {
		instruction: string;
		interval?: string;
		label?: string;
		deliveryMode?: RlmHeartbeatDeliveryMode;
	}): RlmHeartbeat {
		this.load();
		const scheduleText = normalizeHeartbeatSchedule(input.interval);
		const now = new Date();
		const parsed = parseHeartbeatInterval(scheduleText, now);
		const prompt = input.instruction.trim();
		if (!prompt) {
			throw new Error("RLM heartbeat instruction cannot be empty");
		}
		const nowIso = now.toISOString();
		const label = input.label?.trim() || undefined;
		const job: RlmHeartbeat = {
			id: randomUUID(),
			status: "active",
			source: "rlm_heartbeat",
			deliveryMode: input.deliveryMode ?? DEFAULT_HEARTBEAT_DELIVERY_MODE,
			label,
			prompt,
			schedule: { kind: "interval", expression: scheduleText, intervalMs: parsed.intervalMs },
			createdAt: nowIso,
			updatedAt: nowIso,
			nextRunAt: parsed.nextRunAt.toISOString(),
			runCount: 0,
		};
		this.heartbeats.push(job);
		this.persist();
		this.reschedule();
		return job;
	}

	updateRlmHeartbeat(input: {
		id: string;
		instruction?: string;
		interval?: string;
		label?: string;
		status?: "pause" | "resume";
		deliveryMode?: RlmHeartbeatDeliveryMode;
	}): RlmHeartbeat | undefined {
		this.load();
		const now = new Date();
		let updated: RlmHeartbeat | undefined;
		this.heartbeats = this.heartbeats.map((job) => {
			if (job.id !== input.id) return job;
			if (job.status === "cancelled") return job;
			let next: RlmHeartbeat = { ...job, updatedAt: now.toISOString() };
			if (input.label !== undefined) {
				next.label = input.label.trim() || undefined;
			}
			if (input.deliveryMode !== undefined) {
				next.deliveryMode = input.deliveryMode;
			}
			if (input.instruction !== undefined) {
				const prompt = input.instruction.trim();
				if (!prompt) {
					throw new Error("RLM heartbeat instruction cannot be empty");
				}
				next.prompt = prompt;
			}
			if (input.interval !== undefined) {
				const scheduleText = normalizeHeartbeatSchedule(input.interval);
				const parsed = parseHeartbeatInterval(scheduleText, now);
				next.schedule = { kind: "interval", expression: scheduleText, intervalMs: parsed.intervalMs };
				if (next.status === "active") {
					next.nextRunAt = parsed.nextRunAt.toISOString();
				}
			}
			if (input.status === "pause" && next.status === "active") {
				next.status = "paused";
				next.nextRunAt = undefined;
			} else if (input.status === "resume" && next.status === "paused") {
				next.status = "active";
				if (input.interval === undefined) {
					next.nextRunAt = new Date(now.getTime() + next.schedule.intervalMs).toISOString();
				}
			}
			updated = next;
			return next;
		});
		if (updated) {
			this.persist();
			this.reschedule();
		}
		return updated;
	}

	deleteRlmHeartbeat(id: string): RlmHeartbeat | undefined {
		this.load();
		const job = this.heartbeats.find((candidate) => candidate.id === id);
		if (!job) return undefined;
		job.status = "cancelled";
		job.updatedAt = new Date().toISOString();
		job.nextRunAt = undefined;
		this.persist();
		this.reschedule();
		return job;
	}

	// ---- host requests -----------------------------------------------------------

	handleHostRequest(type: string, payload: Record<string, unknown> = {}): Record<string, unknown> {
		switch (type) {
			case "rlm_heartbeat.list": {
				const includeInactive = payload.include_inactive === true || payload.includeInactive === true;
				return {
					heartbeats: this.listRlmHeartbeats({ includeInactive }).map((heartbeat) =>
						rlmHeartbeatHostResponse(heartbeat),
					),
				};
			}
			case "rlm_heartbeat.create": {
				if (typeof payload.instruction !== "string") {
					throw new Error("rlm_heartbeat.create instruction must be a string");
				}
				if (payload.interval !== undefined && typeof payload.interval !== "string") {
					throw new Error("rlm_heartbeat.create interval must be a string when provided");
				}
				if (payload.label !== undefined && typeof payload.label !== "string") {
					throw new Error("rlm_heartbeat.create label must be a string when provided");
				}
				const deliveryMode = normalizeHeartbeatDeliveryMode(payload.delivery_mode ?? payload.deliveryMode);
				return {
					heartbeat: rlmHeartbeatHostResponse(
						this.createRlmHeartbeat({
							instruction: payload.instruction,
							interval: payload.interval,
							label: payload.label,
							deliveryMode,
						}),
					),
				};
			}
			case "rlm_heartbeat.update": {
				if (typeof payload.id !== "string") {
					throw new Error("rlm_heartbeat.update id must be a string");
				}
				if (payload.instruction !== undefined && typeof payload.instruction !== "string") {
					throw new Error("rlm_heartbeat.update instruction must be a string when provided");
				}
				if (payload.interval !== undefined && typeof payload.interval !== "string") {
					throw new Error("rlm_heartbeat.update interval must be a string when provided");
				}
				if (payload.label !== undefined && typeof payload.label !== "string") {
					throw new Error("rlm_heartbeat.update label must be a string when provided");
				}
				if (payload.status !== undefined && payload.status !== "pause" && payload.status !== "resume") {
					throw new Error('rlm_heartbeat.update status must be "pause" or "resume" when provided');
				}
				const rawDeliveryMode = payload.delivery_mode ?? payload.deliveryMode;
				const deliveryMode = normalizeHeartbeatDeliveryMode(rawDeliveryMode);
				if (
					payload.instruction === undefined &&
					payload.interval === undefined &&
					payload.label === undefined &&
					payload.status === undefined &&
					rawDeliveryMode === undefined
				) {
					throw new Error("rlm_heartbeat.update requires at least one field to update");
				}
				const heartbeat = this.updateRlmHeartbeat({
					id: payload.id,
					instruction: payload.instruction as string | undefined,
					interval: payload.interval as string | undefined,
					label: payload.label as string | undefined,
					status: payload.status as "pause" | "resume" | undefined,
					deliveryMode,
				});
				return {
					heartbeat: heartbeat ? rlmHeartbeatHostResponse(heartbeat) : null,
				};
			}
			case "rlm_heartbeat.delete": {
				if (typeof payload.id !== "string") {
					throw new Error("rlm_heartbeat.delete id must be a string");
				}
				const heartbeat = this.deleteRlmHeartbeat(payload.id);
				return {
					heartbeat: heartbeat ? rlmHeartbeatHostResponse(heartbeat) : null,
				};
			}
			default:
				throw new Error(`unknown RLM heartbeat request type "${type}"`);
		}
	}

	// ---- scheduler ---------------------------------------------------------------

	/** Recompute the wake timer; call on session_start and after every mutation. */
	reschedule(): void {
		this.load();
		if (this.timer) {
			clearTimeout(this.timer);
			this.timer = undefined;
		}
		const now = Date.now();
		const due = this.heartbeats
			.filter((job) => job.status === "active" && job.nextRunAt)
			.map((job) => Date.parse(job.nextRunAt!))
			.filter((ts) => Number.isFinite(ts))
			.sort((a, b) => a - b);
		if (due.length === 0) return;
		const delay = Math.max(0, due[0]! - now);
		this.timer = setTimeout(() => {
			void this.fireDue();
		}, delay);
		if (typeof this.timer.unref === "function") this.timer.unref();
	}

	stop(): void {
		if (this.timer) {
			clearTimeout(this.timer);
			this.timer = undefined;
		}
	}

	/** PA shouldDeferHeartbeatCronJob parity for extension-observable activity. */
	private shouldDefer(job: RlmHeartbeat, ctx: ExtensionContext): boolean {
		if (this.options.isCompacting?.() === true) return true;
		if (ctx.hasPendingMessages()) return true;
		const busy = !ctx.isIdle();
		if (!busy) return false;
		// steer heartbeats interrupt the current turn; follow_up waits
		return job.deliveryMode === "follow_up";
	}

	private async fireDue(): Promise<void> {
		const ctx = this.getCtx();
		this.timer = undefined;
		if (!ctx) {
			this.reschedule();
			return;
		}
		const now = new Date();
		const nowMs = now.getTime();
		for (const job of this.heartbeats) {
			if (job.status !== "active" || !job.nextRunAt) continue;
			if (Date.parse(job.nextRunAt) > nowMs) continue;
			// Always advance the schedule first so a deferral/error cannot spin.
			job.nextRunAt = new Date(nowMs + job.schedule.intervalMs).toISOString();
			job.updatedAt = now.toISOString();
			if (this.shouldDefer(job, ctx)) {
				this.persist();
				continue;
			}
			job.runCount += 1;
			job.lastRunAt = now.toISOString();
			try {
				const idle = ctx.isIdle();
				// PA daemon-mode runCronJob heartbeat branch: prompt the raw instruction;
				// steer interrupts the current turn, follow_up waits for it to finish.
				this.pi.sendMessage(
					{
						customType: "rlm_heartbeat",
						content: job.prompt,
						display: true,
						details: { heartbeatId: job.id, label: job.label, runCount: job.runCount },
					},
					idle
						? { triggerTurn: true }
						: { triggerTurn: true, deliverAs: job.deliveryMode === "follow_up" ? "followUp" : "steer" },
				);
				job.lastError = undefined;
			} catch (error) {
				job.lastError = error instanceof Error ? error.message : String(error);
			}
			this.persist();
		}
		this.reschedule();
	}
}
