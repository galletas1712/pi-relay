// M9: per-session REPL console host side.
//
// The bridge forwards `repl.execute` as an rpc `prompt` whose text is a
// sentinel-prefixed JSON payload. pi's rpc-mode prompt path fires the `input`
// extension event BEFORE the streaming/busy check and BEFORE any context
// injection; this handler claims sentinel input ({action:"handled"}) so user
// cells NEVER start an agent turn and NEVER enter model context. The cell is
// then executed on the session's kernel through the same serialized queue the
// ipython tool uses — a shared namespace with the model is the point.
//
// Lifecycle + output are reported host-side via pi.appendEntry("repl_cell" |
// "repl_output", ...) — the same entry_appended pipe rlm_child_lifecycle uses —
// so the bridge spools them like every other contract event (reconnect-
// replayable, watermark-contiguous). Model tool-cells are reported too
// (provenance:"model") via the reporter wired into the ipython tool.
import type { ExtensionContext } from "@earendil-works/pi-coding-agent";
import type { KernelCellEvent, KernelCellMeta, KernelManager } from "./kernel.ts";
import type { KernelProvisioner } from "./ipython-tool.ts";

/** Control-char sentinel prefix for repl payloads (never model-visible). */
export const REPL_SENTINEL = "\u0001prime-repl:";

export interface ReplExecutePayload {
	v: 1;
	op: "execute";
	cell_id: string;
	code: string;
	client_cell_id?: string;
}

/** Parse a sentinel payload. Returns "not-repl" for ordinary input. */
export function parseReplPayload(text: string): ReplExecutePayload | "not-repl" | "invalid" {
	if (!text.startsWith(REPL_SENTINEL)) return "not-repl";
	let raw: unknown;
	try {
		raw = JSON.parse(text.slice(REPL_SENTINEL.length));
	} catch {
		return "invalid";
	}
	if (typeof raw !== "object" || raw === null) return "invalid";
	const p = raw as Record<string, unknown>;
	if (p.v !== 1 || p.op !== "execute") return "invalid";
	if (typeof p.cell_id !== "string" || p.cell_id.length === 0) return "invalid";
	if (typeof p.code !== "string") return "invalid";
	if (p.client_cell_id !== undefined && typeof p.client_cell_id !== "string") return "invalid";
	return { v: 1, op: "execute", cell_id: p.cell_id, code: p.code, client_cell_id: p.client_cell_id };
}

/** Contract caps (see packages/bridge README contract v0.1). */
export const REPL_CODE_ECHO_CAP = 16 * 1024;
export const REPL_TEXT_EVENT_CAP = 16 * 1024;
/** Display payloads (base64 png/jpeg) get a larger cap — the WS frame cap is 8MiB. */
export const REPL_DISPLAY_EVENT_CAP = 3_000_000;
/** Coalesce stream chunks within this window into fewer repl.output events. */
const FLUSH_MS = 60;
const SEEN_CAP = 512;
const FINISHED_CAP = 1024;

export interface ReplConsoleDeps {
	/** pi.appendEntry — emits to the owning session's rpc clients + session file. */
	emit: (customType: "repl_cell" | "repl_output", data: Record<string, unknown>) => void;
	/** Lazy-creating provisioner accessor (same one the ipython tool uses). */
	getProvisioner: (ctx: ExtensionContext) => KernelProvisioner;
	/** Non-creating manager peek for queue-position reporting. */
	peekManager: (sessionId: string) => KernelManager | undefined;
	/** Session id for a ctx (index.ts sessionIdOf). */
	sessionIdOf: (ctx: ExtensionContext) => string;
}

interface StreamBuffer {
	stdout: string;
	stderr: string;
	timer?: ReturnType<typeof setTimeout>;
}

function errorMessage(error: unknown): string {
	return error instanceof Error ? error.message : String(error);
}

export class ReplConsole {
	private readonly seenKeys = new Set<string>();
	private readonly finishedCells = new Set<string>();
	private readonly buffers = new Map<string, StreamBuffer>();

	constructor(private readonly deps: ReplConsoleDeps) {}

	/**
	 * input-event entrypoint. Returns true when the text was a repl payload and
	 * was consumed (executed or deduped); false for ordinary input.
	 */
	handleInput(text: string, ctx: ExtensionContext): boolean {
		const payload = parseReplPayload(text);
		if (payload === "not-repl") return false;
		if (payload === "invalid") {
			console.warn("[prime-rlm] ignoring malformed repl payload");
			return true;
		}
		const key = payload.client_cell_id ?? payload.cell_id;
		if (this.seenKeys.has(key)) {
			// Idempotent replay at the host seam (bridge crash-mid-flight retry):
			// the cell was already queued/executed; do not run it twice.
			return true;
		}
		this.remember(this.seenKeys, key, SEEN_CAP);
		const meta: KernelCellMeta = { cellId: payload.cell_id, provenance: "user", clientCellId: payload.client_cell_id };
		this.emitQueued(meta, payload.code, this.deps.peekManager(this.deps.sessionIdOf(ctx)));
		void this.runUserCell(payload, meta, ctx);
		return true;
	}

	/** ipython-tool seam: announce a model cell BEFORE provisioner.ensure(). */
	reportModelQueued(toolCallId: string, code: string, ctx: ExtensionContext): KernelCellMeta {
		const meta: KernelCellMeta = { cellId: `m_${toolCallId}`, provenance: "model", toolCallId };
		this.emitQueued(meta, code, this.deps.peekManager(this.deps.sessionIdOf(ctx)));
		return meta;
	}

	/** ipython-tool seam: synthesize a finished event when execute() threw
	 * before the kernel took the cell (start failure, shutdown, pre-link abort). */
	reportExecuteError(meta: KernelCellMeta, error: unknown): void {
		this.emitFinishedFallback(meta, "error", { ename: "KernelStartError", evalue: errorMessage(error) });
	}

	/** ipython-tool seam: safety net — emit finished if the kernel didn't. */
	reportExecuteSettled(meta: KernelCellMeta, result: { status: string; durationMs: number; error?: { ename: string; evalue: string } }): void {
		if (this.finishedCells.has(meta.cellId)) return;
		this.emitFinishedFallback(
			meta,
			result.status === "ok" ? "done" : "error",
			result.error ?? (result.status === "aborted" ? { ename: "Aborted", evalue: "execution interrupted" } : undefined),
			result.durationMs,
		);
	}

	/** KernelProvisioner onCellEvent hook — lifecycle + output for cell-meta'd executions. */
	cellEvent(ev: KernelCellEvent): void {
		const cellId = ev.meta.cellId;
		switch (ev.kind) {
			case "running":
				this.flush(cellId);
				this.deps.emit("repl_cell", { cell_id: cellId, status: "running", started_at: ev.startedAt });
				break;
			case "stream":
				this.bufferText(cellId, ev.name, ev.text);
				break;
			case "result":
				this.flush(cellId);
				this.emitDisplay(cellId, "text/plain", ev.text, REPL_TEXT_EVENT_CAP);
				break;
			case "display":
				this.flush(cellId);
				this.emitDisplay(cellId, ev.mimeType, ev.data, REPL_DISPLAY_EVENT_CAP);
				break;
			case "error": {
				this.flush(cellId);
				const tb = ev.traceback.length > 0 ? ev.traceback.join("\n") : `${ev.ename}: ${ev.evalue}`;
				this.emitText(cellId, "error", tb);
				break;
			}
			case "finished": {
				this.flush(cellId);
				this.buffers.delete(cellId);
				this.remember(this.finishedCells, cellId, FINISHED_CAP);
				this.deps.emit("repl_cell", {
					cell_id: cellId,
					status: ev.status === "ok" ? "done" : "error",
					finished_at: ev.finishedAt,
					duration_ms: ev.durationMs,
					...(ev.error ? { error: ev.error } : {}),
					...(ev.status === "aborted" && !ev.error ? { error: { ename: "Aborted", evalue: "execution interrupted" } } : {}),
					...(ev.stdoutTruncated ? { stdout_truncated: true } : {}),
					...(ev.stderrTruncated ? { stderr_truncated: true } : {}),
				});
				break;
			}
		}
	}

	private async runUserCell(payload: ReplExecutePayload, meta: KernelCellMeta, ctx: ExtensionContext): Promise<void> {
		try {
			const provisioner = this.deps.getProvisioner(ctx);
			const manager = await provisioner.ensure();
			const result = await manager.execute(payload.code, { cell: meta });
			this.reportExecuteSettled(meta, result);
		} catch (error) {
			this.reportExecuteError(meta, error);
		}
	}

	private emitQueued(meta: KernelCellMeta, code: string, manager: KernelManager | undefined): void {
		const snap = manager?.cellQueueSnapshot;
		const position = snap ? (snap.running ? 1 : 0) + snap.queued.length : 0;
		this.deps.emit("repl_cell", {
			cell_id: meta.cellId,
			provenance: meta.provenance,
			...(meta.toolCallId ? { tool_call_id: meta.toolCallId } : {}),
			...(meta.clientCellId ? { client_cell_id: meta.clientCellId } : {}),
			code: code.length > REPL_CODE_ECHO_CAP ? code.slice(0, REPL_CODE_ECHO_CAP) : code,
			...(code.length > REPL_CODE_ECHO_CAP ? { code_truncated: true } : {}),
			status: "queued",
			queued_at: new Date().toISOString(),
			position,
		});
	}

	private emitFinishedFallback(
		meta: KernelCellMeta,
		status: "done" | "error",
		error: { ename: string; evalue: string } | undefined,
		durationMs?: number,
	): void {
		if (this.finishedCells.has(meta.cellId)) return;
		this.remember(this.finishedCells, meta.cellId, FINISHED_CAP);
		this.flush(meta.cellId);
		this.buffers.delete(meta.cellId);
		this.deps.emit("repl_cell", {
			cell_id: meta.cellId,
			status,
			finished_at: new Date().toISOString(),
			...(durationMs !== undefined ? { duration_ms: durationMs } : {}),
			...(error ? { error } : {}),
		});
	}

	// ---- output coalescing -------------------------------------------------

	private bufferText(cellId: string, name: "stdout" | "stderr", text: string): void {
		let buf = this.buffers.get(cellId);
		if (!buf) {
			buf = { stdout: "", stderr: "" };
			this.buffers.set(cellId, buf);
		}
		buf[name] += text;
		if (buf[name].length >= REPL_TEXT_EVENT_CAP) {
			this.flush(cellId);
			return;
		}
		if (!buf.timer) {
			buf.timer = setTimeout(() => this.flush(cellId), FLUSH_MS);
			if (buf.timer && typeof buf.timer === "object" && "unref" in buf.timer) buf.timer.unref();
		}
	}

	/** Flush buffered stream text for a cell (split into <=cap events, no loss). */
	private flush(cellId: string): void {
		const buf = this.buffers.get(cellId);
		if (!buf) return;
		if (buf.timer) {
			clearTimeout(buf.timer);
			buf.timer = undefined;
		}
		if (buf.stdout) {
			this.emitText(cellId, "stdout", buf.stdout);
			buf.stdout = "";
		}
		if (buf.stderr) {
			this.emitText(cellId, "stderr", buf.stderr);
			buf.stderr = "";
		}
	}

	private emitText(cellId: string, stream: "stdout" | "stderr" | "error", text: string): void {
		for (let i = 0; i < text.length; i += REPL_TEXT_EVENT_CAP) {
			this.deps.emit("repl_output", { cell_id: cellId, stream, data: text.slice(i, i + REPL_TEXT_EVENT_CAP) });
		}
	}

	private emitDisplay(cellId: string, mimeType: string, data: string, cap: number): void {
		if (data.length > cap) {
			this.deps.emit("repl_output", { cell_id: cellId, stream: "display", mime_type: mimeType, data: "", truncated: true });
			return;
		}
		this.deps.emit("repl_output", { cell_id: cellId, stream: "display", mime_type: mimeType, data });
	}

	private remember(set: Set<string>, key: string, cap: number): void {
		set.add(key);
		if (set.size > cap) {
			const oldest = set.values().next().value;
			if (oldest !== undefined) set.delete(oldest);
		}
	}
}
