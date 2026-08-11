// BridgeSessionStore: owns the per-session projections and the attach/resume
// lifecycle on top of BridgeClient.
//
// Resume discipline (contract v0): sessions attach at the projection's applied
// watermark, so a reconnect re-attaches with fromSeq=watermark and the bridge
// replays exactly watermark+1..head (flagged replayed:true) before live events
// continue. The bridge delivers replay frames BEFORE the attach response
// (replay holds the per-session event lock server-side), so attach marks the
// projection attached optimistically; a failed attach rolls that back. If the
// requested range was trimmed from the spool ring, the bridge answers with
// typed event_gap; the store then rebuilds via session.getState and re-attaches
// at head (the only v0-correct recovery).
//
// UI binding: useSyncExternalStore-compatible subscribe/getSnapshot.
import { BridgeClient, eventGapData, isBridgeErrorCode, newIdempotencyKey } from "./client.ts";
import {
	appendLocalUserMessage,
	applyContractEvent,
	applyModelPatch,
	emptySessionProjection,
	markAttached,
	markDetached,
	mergeSnapshotModel,
	rebuildFromState,
	type ModelInfo,
	type SessionProjection,
} from "./eventStore.ts";
import type { ContractEventEnvelope } from "./types.ts";

type Listener = () => void;

export class BridgeSessionStore {
	private readonly projections = new Map<string, SessionProjection>();
	private readonly listeners = new Set<Listener>();
	private readonly attachedIds = new Set<string>();
	private readonly attachInFlight = new Map<string, Promise<SessionProjection>>();
	private reattachInFlight: Promise<void> | null = null;
	private readonly offEvent: () => void;
	private readonly offStatus: () => void;

	constructor(readonly client: BridgeClient) {
		this.offEvent = client.onEvent((ev) => this.handleEvent(ev));
		this.offStatus = client.onStatus((status) => {
			if (status === "open") void this.reattachAll();
		});
	}

	dispose(): void {
		this.offEvent();
		this.offStatus();
		this.listeners.clear();
	}

	subscribe = (listener: Listener): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	getSnapshot = (sessionId: string): SessionProjection | null => {
		return this.projections.get(sessionId) ?? null;
	};

	private setProjection(next: SessionProjection): void {
		const prev = this.projections.get(next.sessionId);
		if (prev === next) return;
		this.projections.set(next.sessionId, next);
		for (const listener of this.listeners) listener();
	}

	private projection(sessionId: string): SessionProjection {
		return this.projections.get(sessionId) ?? emptySessionProjection(sessionId);
	}

	private handleEvent(ev: ContractEventEnvelope): void {
		const current = this.projections.get(ev.sessionId);
		if (!current?.attached) return; // only attached sessions project events
		this.setProjection(applyContractEvent(current, ev));
	}

	/** Subscribe to a session's event stream, resuming from the applied
	 * watermark (0 = full replay from the spool). */
	attach(sessionId: string): Promise<SessionProjection> {
		const inFlight = this.attachInFlight.get(sessionId);
		if (inFlight) return inFlight;
		const promise = this.attachNow(sessionId).finally(() => {
			this.attachInFlight.delete(sessionId);
		});
		this.attachInFlight.set(sessionId, promise);
		return promise;
	}

	private async attachNow(sessionId: string): Promise<SessionProjection> {
		const current = this.projection(sessionId);
		this.attachedIds.add(sessionId);
		// optimistic: replay frames arrive before the attach response
		this.setProjection({ ...current, attached: true });
		try {
			const res = await this.client.attachSession(sessionId, current.watermark);
			const next = markAttached(this.projection(sessionId), res.headSeq);
			this.setProjection(next);
			// M11a: seed the model surface (live host state or session-file
			// fallback). Best-effort — the pickers stay empty when it fails.
			void this.reconcileModel(sessionId).catch(() => {});
			return next;
		} catch (error) {
			this.attachedIds.delete(sessionId);
			this.setProjection(markDetached(this.projection(sessionId)));
			throw error;
		}
	}

	/** M11a: reconcile the projection's model surface against getState. */
	async reconcileModel(sessionId: string): Promise<void> {
		const snapshot = await this.client.getState(sessionId);
		this.setProjection(mergeSnapshotModel(this.projection(sessionId), snapshot));
	}

	/** M11a: optimistic model/level patch while a mutation is in flight. */
	applyOptimisticModel(sessionId: string, patch: Partial<ModelInfo>): void {
		this.setProjection(applyModelPatch(this.projection(sessionId), patch));
	}

	async detach(sessionId: string): Promise<void> {
		this.attachedIds.delete(sessionId);
		const current = this.projections.get(sessionId);
		if (current) this.setProjection(markDetached(current));
		await this.client.detachSession(sessionId);
	}

	/** Re-attach every attached session after a (re)connect. Serialized so a
	 * flapping socket cannot run overlapping replay cycles. */
	private reattachAll(): Promise<void> {
		if (this.attachedIds.size === 0) return Promise.resolve();
		if (this.reattachInFlight) return this.reattachInFlight;
		this.reattachInFlight = (async () => {
			for (const sessionId of this.attachedIds) {
				const watermark = this.projection(sessionId).watermark;
				try {
					const res = await this.client.attachSession(sessionId, watermark);
					this.setProjection(markAttached(this.projection(sessionId), res.headSeq));
				} catch (error) {
					const gap = eventGapData(error);
					if (!gap) {
						if (isBridgeErrorCode(error, "session_not_found")) {
							this.setProjection({ ...this.projection(sessionId), state: "closed", attached: false });
							this.attachedIds.delete(sessionId);
						}
						continue;
					}
					// trimmed range: rebuild from getState, re-attach at head
					const snapshot = await this.client.getState(sessionId);
					this.setProjection(rebuildFromState(this.projection(sessionId), snapshot, new Date().toISOString()));
					const res = await this.client.attachSession(sessionId, snapshot.headSeq);
					this.setProjection(markAttached(this.projection(sessionId), res.headSeq));
				}
			}
		})().finally(() => {
			this.reattachInFlight = null;
		});
		return this.reattachInFlight;
	}

	/** Send a prompt with an idempotency key and optimistic local echo. On
	 * BridgeTransportError the outcome is uncertain; retry with the SAME
	 * returned key and the bridge replays the stored response (no double-apply). */
	async sendPrompt(sessionId: string, text: string): Promise<{ seq: number; queued: boolean; replay: boolean; idempotencyKey: string }> {
		const idempotencyKey = newIdempotencyKey(`m6prompt:${sessionId}`);
		const res = await this.client.sendPrompt(sessionId, text, idempotencyKey);
		if (!res.replay) {
			this.setProjection(appendLocalUserMessage(this.projection(sessionId), idempotencyKey, text));
		}
		return { seq: res.seq, queued: res.queued, replay: res.replay ?? false, idempotencyKey };
	}

	async steer(sessionId: string, text: string): Promise<void> {
		await this.client.steer(sessionId, text);
	}

	async followUp(sessionId: string, text: string): Promise<void> {
		await this.client.followUp(sessionId, text);
	}

	async abort(sessionId: string): Promise<void> {
		await this.client.abort(sessionId);
	}
}
