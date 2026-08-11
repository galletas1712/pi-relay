// M9: event-sourced REPL console projection (contract v0.1 repl.* events).
//
// Same disciplines as eventStore.ts: the bridge guarantees per-session
// monotonic seq with gap/dup-free delivery, so the reducer drops seqs at or
// below the applied watermark and projects repl.cell/repl.output into the
// console view. The projection interleaves user cells (provenance "user") and
// the model's own ipython tool cells (provenance "model", toolCallId link) —
// everything the shared kernel runs.
//
// Pure reducer functions + a BridgeClient-subscribing store (mirrors
// sessionStore); unit-tested in isolation.
import { BridgeClient, newIdempotencyKey } from "./client.ts";
import type { ContractEventEnvelope, ReplCellData, ReplOutputData, ReplOutputStream } from "./types.ts";

export interface ReplOutputItem {
	seq: number;
	stream: ReplOutputStream;
	data: string;
	mimeType?: string;
	truncated?: boolean;
}

export type ReplCellViewStatus = "queued" | "running" | "done" | "error" | "unknown";

export interface ReplCellView {
	cellId: string;
	provenance: "user" | "model" | null;
	status: ReplCellViewStatus;
	code: string | null;
	codeTruncated: boolean;
	clientCellId?: string;
	toolCallId?: string;
	position?: number;
	queuedAt?: string;
	startedAt?: string;
	finishedAt?: string;
	durationMs?: number;
	error?: { ename: string; evalue: string };
	stdoutTruncated?: boolean;
	stderrTruncated?: boolean;
	outputs: ReplOutputItem[];
	/** stream-ordinal key of first sighting (console ordering) */
	firstSeq: number;
	lastSeq: number;
}

export interface ReplProjection {
	sessionId: string;
	/** highest applied seq (dup watermark); 0 = nothing applied */
	watermark: number;
	cells: ReplCellView[];
}

/** Bound per-session console history: drop oldest FINISHED cells beyond this. */
const MAX_CELLS = 300;

export function emptyReplProjection(sessionId: string): ReplProjection {
	return { sessionId, watermark: 0, cells: [] };
}

function mergeCell(prev: ReplCellView | null, data: ReplCellData, seq: number): ReplCellView {
	const terminal = prev?.status === "done" || prev?.status === "error";
	// Terminal stickiness: a late/out-of-order non-terminal frame must not
	// regress a finished cell (defensive; watermark dedupe already drops dupes).
	const status = terminal && (data.status === "queued" || data.status === "running") ? prev.status : data.status;
	return {
		cellId: data.cell_id,
		provenance: data.provenance ?? prev?.provenance ?? null,
		status,
		code: data.code ?? prev?.code ?? null,
		codeTruncated: data.code_truncated ?? prev?.codeTruncated ?? false,
		clientCellId: data.client_cell_id ?? prev?.clientCellId,
		toolCallId: data.tool_call_id ?? prev?.toolCallId,
		position: data.position ?? prev?.position,
		queuedAt: data.queued_at ?? prev?.queuedAt,
		startedAt: data.started_at ?? prev?.startedAt,
		finishedAt: data.finished_at ?? prev?.finishedAt,
		durationMs: data.duration_ms ?? prev?.durationMs,
		error: data.error ?? prev?.error,
		stdoutTruncated: data.stdout_truncated ?? prev?.stdoutTruncated,
		stderrTruncated: data.stderr_truncated ?? prev?.stderrTruncated,
		outputs: prev?.outputs ?? [],
		firstSeq: prev?.firstSeq ?? seq,
		lastSeq: seq,
	};
}

function trimCells(cells: ReplCellView[]): ReplCellView[] {
	if (cells.length <= MAX_CELLS) return cells;
	const out = [...cells];
	for (let i = 0; i < out.length && out.length > MAX_CELLS; ) {
		const c = out[i];
		if (c.status === "done" || c.status === "error") {
			out.splice(i, 1);
		} else {
			i += 1;
		}
	}
	// still over: drop oldest regardless of status
	while (out.length > MAX_CELLS) out.shift();
	return out;
}

/** Apply one repl.* event. Dupes (seq <= watermark) return the state
 * unchanged. Non-repl events pass through untouched. */
export function applyReplEvent(state: ReplProjection, ev: ContractEventEnvelope): ReplProjection {
	if (ev.sessionId !== state.sessionId) return state;
	if (ev.event !== "repl.cell" && ev.event !== "repl.output") return state;
	if (ev.seq <= state.watermark) return state;
	let cells = state.cells;
	if (ev.event === "repl.cell") {
		const data = ev.data as unknown as ReplCellData;
		if (!data.cell_id) return { ...state, watermark: ev.seq };
		const index = cells.findIndex((c) => c.cellId === data.cell_id);
		const merged = mergeCell(index >= 0 ? cells[index] : null, data, ev.seq);
		cells = index >= 0 ? [...cells.slice(0, index), merged, ...cells.slice(index + 1)] : [...cells, merged];
	} else {
		const data = ev.data as unknown as ReplOutputData;
		if (!data.cell_id) return { ...state, watermark: ev.seq };
		const index = cells.findIndex((c) => c.cellId === data.cell_id);
		const item: ReplOutputItem = {
			seq: ev.seq,
			stream: data.stream,
			data: data.data ?? "",
			mimeType: data.mime_type,
			truncated: data.truncated,
		};
		if (index >= 0) {
			const prev = cells[index];
			cells = [...cells.slice(0, index), { ...prev, outputs: [...prev.outputs, item], lastSeq: ev.seq }, ...cells.slice(index + 1)];
		} else {
			// late attach mid-cell: synthesize the cell so output stays visible
			cells = [
				...cells,
				{
					cellId: data.cell_id,
					provenance: null,
					status: "running",
					code: null,
					codeTruncated: false,
					outputs: [item],
					firstSeq: ev.seq,
					lastSeq: ev.seq,
				},
			];
		}
	}
	return { ...state, watermark: ev.seq, cells: trimCells(cells) };
}

/** queued + running counts for the busy indicator. */
export function replBusyCounts(state: ReplProjection): { queued: number; running: number } {
	let queued = 0;
	let running = 0;
	for (const c of state.cells) {
		if (c.status === "queued") queued += 1;
		else if (c.status === "running") running += 1;
	}
	return { queued, running };
}

type Listener = () => void;

/** Store: subscribes to the shared BridgeClient event stream and projects
 * repl.* events per session. Attach/detach lifecycle is owned by
 * BridgeSessionStore — the bridge only sends events for subscribed sessions,
 * so projections here are created lazily on first repl event. */
export class ReplStore {
	private readonly projections = new Map<string, ReplProjection>();
	private readonly listeners = new Set<Listener>();
	private readonly offEvent: () => void;

	constructor(readonly client: BridgeClient) {
		this.offEvent = client.onEvent((ev) => this.handleEvent(ev));
	}

	dispose(): void {
		this.offEvent();
		this.listeners.clear();
	}

	subscribe = (listener: Listener): (() => void) => {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	};

	getSnapshot = (sessionId: string): ReplProjection | null => {
		return this.projections.get(sessionId) ?? null;
	};

	private handleEvent(ev: ContractEventEnvelope): void {
		if (ev.event !== "repl.cell" && ev.event !== "repl.output") return;
		const current = this.projections.get(ev.sessionId) ?? emptyReplProjection(ev.sessionId);
		const next = applyReplEvent(current, ev);
		if (next !== current) {
			this.projections.set(ev.sessionId, next);
			for (const listener of this.listeners) listener();
		}
	}

	/** Run a user cell. client_cell_id is always generated so transport retries
	 * are idempotent end-to-end (bridge idem + deterministic cell_id + host
	 * dedupe). */
	async execute(sessionId: string, code: string): Promise<{ cellId: string; replay: boolean }> {
		const clientCellId = newIdempotencyKey(`m9repl:${sessionId}`);
		const res = await this.client.replExecute(sessionId, code, clientCellId);
		return { cellId: res.cell_id, replay: res.replay ?? false };
	}
}
