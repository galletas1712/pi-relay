// Event-sourced per-session projection for bridge contract v0.
//
// The bridge guarantees a per-session monotonic seq and gap/dup-free delivery
// (replay holds the event lock; per-connection watermarks suppress dupes).
// This reducer is the client-side enforcement point: it drops seqs at or below
// the applied watermark (dupes), flags discontinuities above watermark+1
// (gaps), and projects the event stream into transcript blocks, session state,
// and the subagent table the UI renders.
//
// Pure module: no client, no React — unit-tested in isolation.
import type {
	CommsMessageData,
	ContractEventEnvelope,
	MessageDeltaData,
	SessionErrorData,
	SessionModelData,
	SessionStateEventData,
	SubagentLifecycleData,
	ToolExecData,
} from "./types.ts";

export interface MessageBlock {
	kind: "message";
	/** stream-ordinal key; local user echoes use "local:<idempotencyKey>" */
	id: string;
	role: string;
	text: string;
	thinking: string;
	complete: boolean;
	/** locally appended on prompt send before the stream echoes it */
	local?: boolean;
	/** seqs that touched this block (start..end), for debugging */
	fromSeq?: number;
}

export interface ToolExecBlock {
	kind: "tool";
	toolCallId: string;
	toolName: string;
	args?: string;
	partialResult?: string;
	result?: string;
	isError?: boolean;
	done: boolean;
	fromSeq?: number;
}

/** M11a: inter-agent comms annotation (comms.message events). Rendered as a
 * slim expandable row — orange accent (--primary), sender → target + preview
 * collapsed; never a boxed card, never generic "custom". */
export interface CommsBlock {
	kind: "comms";
	id: string;
	direction: "in" | "out";
	from: string;
	to: string;
	role: string | null;
	text: string;
	deliveryStatus: string;
	at: string | null;
	fromSeq?: number;
}

/** M11a: current model + effective thinking level (session.model events,
 * getState reconcile). */
export interface ModelInfo {
	provider: string | null;
	modelId: string | null;
	name: string | null;
	thinkingLevel: string | null;
}

export type TranscriptBlock = MessageBlock | ToolExecBlock | CommsBlock;

export interface SessionErrorEntry {
	seq: number;
	at: string;
	code: string;
	message: string;
}

export interface SubagentProjection {
	rlmChildId: string;
	name: string | null;
	childSessionId: string | null;
	status: string;
	phases: string[];
	lastSeq: number;
}

export interface StreamGap {
	/** first seq the stream skipped */
	expected: number;
	got: number;
	at: string;
}

export interface SessionProjection {
	sessionId: string;
	/** highest applied seq (dup watermark); 0 = nothing applied */
	watermark: number;
	/** server head seq reported by the last attach */
	headSeq: number;
	attached: boolean;
	state: string;
	blocks: TranscriptBlock[];
	subagents: SubagentProjection[];
	errors: SessionErrorEntry[];
	/** M11a: current model surface (session.model events + getState). */
	model: ModelInfo | null;
	/** set when a seq discontinuity was detected in the live stream */
	gap: StreamGap | null;
	/** set when the projection was rebuilt via session.getState after event_gap */
	rebuiltAt: string | null;
}

export function emptySessionProjection(sessionId: string): SessionProjection {
	return {
		sessionId,
		watermark: 0,
		headSeq: 0,
		attached: false,
		state: "starting",
		blocks: [],
		subagents: [],
		errors: [],
		model: null,
		gap: null,
		rebuiltAt: null,
	};
}

/** Optimistic local echo for a prompt this client sent (the stream's user
 * message start/end carries no text, so the local copy is the only place the
 * prompt body exists client-side in contract v0). */
export function appendLocalUserMessage(
	state: SessionProjection,
	idempotencyKey: string,
	text: string,
): SessionProjection {
	const block: MessageBlock = {
		kind: "message",
		id: `local:${idempotencyKey}`,
		role: "user",
		text,
		thinking: "",
		complete: true,
		local: true,
	};
	return { ...state, blocks: [...state.blocks, block] };
}

function lastOpenMessageIndex(blocks: TranscriptBlock[]): number {
	for (let i = blocks.length - 1; i >= 0; i -= 1) {
		const b = blocks[i];
		if (b.kind === "message" && !b.complete) return i;
	}
	return -1;
}

function replaceBlock(blocks: TranscriptBlock[], index: number, next: TranscriptBlock): TranscriptBlock[] {
	return [...blocks.slice(0, index), next, ...blocks.slice(index + 1)];
}

function applyMessageDelta(state: SessionProjection, ev: ContractEventEnvelope): SessionProjection {
	const data = ev.data as unknown as MessageDeltaData;
	const role = data.role ?? "assistant";
	// toolResult envelopes duplicate what tool.exec already renders.
	if (role === "toolResult") return state;

	if (data.kind === "start") {
		const blocks = [...state.blocks];
		if (role === "user") {
			// Match the optimistic local echo instead of duplicating it.
			const last = blocks[blocks.length - 1];
			if (last?.kind === "message" && last.role === "user" && last.local) {
				blocks[blocks.length - 1] = { ...last, local: false, fromSeq: ev.seq };
				return { ...state, blocks };
			}
		}
		blocks.push({
			kind: "message",
			id: `m${ev.seq}`,
			role,
			text: "",
			thinking: "",
			complete: false,
			fromSeq: ev.seq,
		});
		return { ...state, blocks };
	}

	const open = lastOpenMessageIndex(state.blocks);
	if (data.kind === "end") {
		if (open < 0) return state;
		const b = state.blocks[open] as MessageBlock;
		return { ...state, blocks: replaceBlock(state.blocks, open, { ...b, complete: true }) };
	}
	if (data.kind === "text" || data.kind === "thinking") {
		const delta = data.delta ?? "";
		if (!delta) return state;
		if (open < 0) {
			// delta without a start (e.g. attached mid-message): open a block so
			// the text is not lost.
			const block: MessageBlock = {
				kind: "message",
				id: `m${ev.seq}`,
				role,
				text: data.kind === "text" ? delta : "",
				thinking: data.kind === "thinking" ? delta : "",
				complete: false,
				fromSeq: ev.seq,
			};
			return { ...state, blocks: [...state.blocks, block] };
		}
		const b = state.blocks[open] as MessageBlock;
		const next: MessageBlock =
			data.kind === "text" ? { ...b, text: b.text + delta } : { ...b, thinking: b.thinking + delta };
		return { ...state, blocks: replaceBlock(state.blocks, open, next) };
	}
	// kind "toolcall": the assembled call; tool.exec start already carries args.
	return state;
}

function applyToolExec(state: SessionProjection, ev: ContractEventEnvelope): SessionProjection {
	const data = ev.data as unknown as ToolExecData;
	const blocks = [...state.blocks];
	let index = -1;
	for (let i = blocks.length - 1; i >= 0; i -= 1) {
		const b = blocks[i];
		if (b.kind === "tool" && b.toolCallId === data.toolCallId) {
			index = i;
			break;
		}
	}
	if (data.phase === "start" || index < 0) {
		if (data.phase !== "start" && index < 0) {
			// late attach: synthesize the block so updates/ends remain visible
		}
		const block: ToolExecBlock = {
			kind: "tool",
			toolCallId: data.toolCallId,
			toolName: data.toolName,
			args: data.args,
			partialResult: data.partialResult,
			result: data.phase === "end" ? data.result : undefined,
			isError: data.isError,
			done: data.phase === "end",
			fromSeq: ev.seq,
		};
		if (index >= 0) {
			// duplicate start after watermark checks should not happen; merge.
			blocks[index] = { ...(blocks[index] as ToolExecBlock), ...block, fromSeq: (blocks[index] as ToolExecBlock).fromSeq };
			return { ...state, blocks };
		}
		return { ...state, blocks: [...blocks, block] };
	}
	const prev = blocks[index] as ToolExecBlock;
	const next: ToolExecBlock = {
		...prev,
		partialResult: data.partialResult ?? prev.partialResult,
		result: data.phase === "end" ? (data.result ?? prev.result) : prev.result,
		isError: data.isError ?? prev.isError,
		done: prev.done || data.phase === "end",
	};
	blocks[index] = next;
	return { ...state, blocks };
}

function applySubagentLifecycle(state: SessionProjection, ev: ContractEventEnvelope): SessionProjection {
	const data = ev.data as unknown as SubagentLifecycleData;
	const id = data.rlm_child_id;
	if (!id) return state;
	const statusFromPhase: Record<string, string> = {
		admitted: "running",
		completed: "completed",
		error: "error",
		deleted: "deleted",
	};
	const existing = state.subagents.find((s) => s.rlmChildId === id);
	const phases = existing?.phases ?? [];
	// Terminal statuses are sticky: a duplicate/out-of-order non-terminal phase
	// (e.g. file+spool double-absorb replaying "admitted") must not regress a
	// completed child back to running. Explicit data.status still wins.
	const terminal = new Set(["completed", "error", "deleted"]);
	const phaseIsTerminal = terminal.has(statusFromPhase[data.phase] ?? "");
	const status =
		data.status ??
		(terminal.has(existing?.status ?? "") && !phaseIsTerminal
			? existing?.status
			: (statusFromPhase[data.phase] ?? existing?.status ?? "running"));
	const next: SubagentProjection = {
		rlmChildId: id,
		name: data.session_name ?? existing?.name ?? null,
		childSessionId: data.session_id ?? existing?.childSessionId ?? null,
		status: status ?? "running",
		phases: phases.includes(data.phase) ? phases : [...phases, data.phase],
		lastSeq: ev.seq,
	};
	const subagents = existing
		? state.subagents.map((s) => (s.rlmChildId === id ? next : s))
		: [...state.subagents, next];
	return { ...state, subagents };
}

/** Apply one event. Dupes (seq <= watermark) return the state unchanged; a
 * discontinuity (seq > watermark + 1) is flagged in `gap` but still applied so
 * the projection stays as current as the stream allows. */
export function applyContractEvent(state: SessionProjection, ev: ContractEventEnvelope): SessionProjection {
	if (ev.sessionId !== state.sessionId) return state;
	if (ev.seq <= state.watermark) return state;
	let next: SessionProjection = {
		...state,
		watermark: ev.seq,
		headSeq: Math.max(state.headSeq, ev.seq),
	};
	if (ev.seq > state.watermark + 1 && state.attached) {
		next = { ...next, gap: { expected: state.watermark + 1, got: ev.seq, at: ev.at } };
	}
	switch (ev.event) {
		case "session.state":
			return { ...next, state: (ev.data as unknown as SessionStateEventData).state ?? next.state };
		case "message.delta":
			return applyMessageDelta(next, ev);
		case "tool.exec":
			return applyToolExec(next, ev);
		case "subagent.lifecycle":
			return applySubagentLifecycle(next, ev);
		case "session.model": {
			// M11a: fields absent/null mean "unchanged" (thinking_level_changed
			// carries only the level). Merge non-null fields.
			const d = ev.data as unknown as SessionModelData;
			const cur = next.model ?? { provider: null, modelId: null, name: null, thinkingLevel: null };
			return {
				...next,
				model: {
					provider: d.provider ?? cur.provider,
					modelId: d.modelId ?? cur.modelId,
					name: d.name ?? cur.name,
					thinkingLevel: d.thinkingLevel ?? cur.thinkingLevel,
				},
			};
		}
		case "comms.message": {
			// M11a: inter-agent annotation. Inbound (direction "in" from the
			// bridge's agent_message mapping) or outbound (prime-comms terminal
			// transition). Skip events that carry nothing displayable.
			const d = ev.data as unknown as CommsMessageData & { direction?: "in" | "out" };
			const direction = d.direction ?? "out";
			const from = direction === "in" ? (d.fromName ?? d.fromSessionId ?? "agent") : "this session";
			const to =
				direction === "in"
					? "this session"
					: (d.receiverName ?? (d.targetSessionId ? `${d.targetSessionId.slice(0, 8)}…` : "unknown"));
			const text = typeof d.message === "string" ? d.message : "";
			const status = d.deliveryStatus ?? "delivered";
			if (!text && !d.fromName && !d.receiverName) return next;
			const block: CommsBlock = {
				kind: "comms",
				id: `c${ev.seq}`,
				direction,
				from,
				to,
				role: d.role ?? null,
				text,
				deliveryStatus: status,
				at: d.ts ?? ev.at,
				fromSeq: ev.seq,
			};
			return { ...next, blocks: [...next.blocks, block] };
		}
		case "session.error": {
			const data = ev.data as unknown as SessionErrorEntry;
			return { ...next, errors: [...next.errors, { seq: ev.seq, at: ev.at, code: data.code, message: String(data.message ?? "") }] };
		}
		default:
			return next;
	}
}

/** Model-relevant slice of session.getState (live.model is "provider/id"). */
export interface SnapshotModelState {
	live?: { model?: string | null; thinkingLevel?: string | null } | null;
	model?: { provider: string | null; modelId: string | null; thinkingLevel: string | null } | null;
}

/** Derive the projection's model surface from a getState snapshot: the live
 * host state wins; the session-file fallback covers host_down. Returns the
 * input when the snapshot carries nothing. */
export function modelFromSnapshot(snapshot: SnapshotModelState): ModelInfo | null {
	const liveModel = snapshot.live?.model;
	if (typeof liveModel === "string" && liveModel.includes("/")) {
		const slash = liveModel.indexOf("/");
		return {
			provider: liveModel.slice(0, slash),
			modelId: liveModel.slice(slash + 1),
			name: null,
			thinkingLevel: snapshot.live?.thinkingLevel ?? null,
		};
	}
	if (snapshot.model && (snapshot.model.provider || snapshot.model.modelId || snapshot.model.thinkingLevel)) {
		return {
			provider: snapshot.model.provider,
			modelId: snapshot.model.modelId,
			name: null,
			thinkingLevel: snapshot.model.thinkingLevel,
		};
	}
	return null;
}

/** Merge a getState snapshot's model surface into the projection (initial
 * attach seed + post-mutation reconcile). */
export function mergeSnapshotModel(state: SessionProjection, snapshot: SnapshotModelState): SessionProjection {
	const model = modelFromSnapshot(snapshot);
	if (!model) return state;
	return {
		...state,
		model: {
			provider: model.provider ?? state.model?.provider ?? null,
			modelId: model.modelId ?? state.model?.modelId ?? null,
			name: model.name ?? state.model?.name ?? null,
			thinkingLevel: model.thinkingLevel ?? state.model?.thinkingLevel ?? null,
		},
	};
}

/** Optimistic patch while a set_model / set_thinking_level request is in
 * flight; reconciled against getState on settle. */
export function applyModelPatch(state: SessionProjection, patch: Partial<ModelInfo>): SessionProjection {
	const cur = state.model ?? { provider: null, modelId: null, name: null, thinkingLevel: null };
	return { ...state, model: { ...cur, ...patch } };
}

/** Rebuild after a typed event_gap: the trimmed range is unrecoverable over
 * contract v0 (getState carries no transcript bodies), so the projection keeps
 * what it has, re-baselines the watermark at the server head, and records the
 * rebuild for the UI to surface. */
export function rebuildFromState(
	state: SessionProjection,
	snapshot: { state: string; headSeq: number; transcript?: { blocks: TranscriptBlock[] } } & SnapshotModelState,
	at: string,
): SessionProjection {
	const base: SessionProjection = {
		...state,
		watermark: Math.max(state.watermark, snapshot.headSeq),
		headSeq: snapshot.headSeq,
		state: snapshot.state,
		// M8 (G2): adopt the bridge-rebuilt transcript when present (trimmed spool)
		blocks: snapshot.transcript?.blocks ?? state.blocks,
		rebuiltAt: at,
	};
	return mergeSnapshotModel(base, snapshot);
}

export function markAttached(state: SessionProjection, headSeq: number): SessionProjection {
	return { ...state, attached: true, headSeq: Math.max(state.headSeq, headSeq) };
}

export function markDetached(state: SessionProjection): SessionProjection {
	return { ...state, attached: false };
}

/** contiguity assertion used by tests and the F2 trace: seqs a store applied
 * (excluding dupes) must form watermark progress without holes. */
export function seqContiguity(seqs: number[]): { contiguous: boolean; firstGapAt: number | null } {
	let prev = 0;
	for (const s of seqs) {
		if (s <= prev) continue;
		if (prev !== 0 && s !== prev + 1) return { contiguous: false, firstGapAt: prev + 1 };
		prev = s;
	}
	return { contiguous: true, firstGapAt: null };
}
