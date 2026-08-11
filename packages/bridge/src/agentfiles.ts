// M11a: pi session-file readers for the drill-down + comms surfaces.
//
// Layouts (verified on disk 2026-08-10):
//  - rlm child sessions: <dirname(parent sessionFile)>/sub-<rlmChildId>/
//    <ts>_<childSessionId>.jsonl (prime-rlm SessionManager.create(cwd, dir)).
//  - comms outbox: <sessionDir>/prime/<sessionId>/comms/outbox.jsonl where
//    sessionDir = dirname(sessionFile) — uniform for roots and rlm children.
//  - inbound comms in a session file: {type:"custom_message",
//    customType:"agent_message", details:{id,message,from,target}, timestamp}.
//  - repl cells in a session file: {type:"custom", customType:"repl_cell"|
//    "repl_output", data:{…}} — same data shapes as the wire events (M9).
import { existsSync, readdirSync, readFileSync } from "node:fs";
import path from "node:path";
import * as db from "./db.ts";
import { BridgeError } from "./supervisor.ts";
import { rebuildTranscript, type TranscriptBlock } from "./transcript.ts";

function readLines(file: string): string[] {
	try {
		return readFileSync(file, "utf8").split("\n");
	} catch {
		return [];
	}
}

// ---- repl cells (fold of repl_cell/repl_output custom entries) --------------

export interface ReplOutputItem {
	stream: string;
	data: string;
	mimeType?: string;
	truncated?: boolean;
}

export interface ReplCell {
	cellId: string;
	provenance: string | null;
	status: string;
	code: string | null;
	toolCallId?: string;
	queuedAt?: string;
	startedAt?: string;
	finishedAt?: string;
	durationMs?: number;
	error?: { ename: string; evalue: string };
	outputs: ReplOutputItem[];
}

/** Fold repl_cell/repl_output custom entries into per-cell views. Not a
 * branch walk: custom entries are append-only and branch-agnostic (the
 * tree-walk is for model messages; cells are execution facts). */
export function replCellsFromFile(sessionFile: string): ReplCell[] {
	const cells = new Map<string, ReplCell>();
	for (const line of readLines(sessionFile)) {
		if (!line.includes("repl_")) continue;
		let o: { type?: string; customType?: string; data?: Record<string, unknown> };
		try {
			o = JSON.parse(line);
		} catch {
			continue;
		}
		if (o.type !== "custom" || (o.customType !== "repl_cell" && o.customType !== "repl_output")) continue;
		const d = o.data ?? {};
		const cellId = typeof d.cell_id === "string" ? d.cell_id : "";
		if (!cellId) continue;
		let cell = cells.get(cellId);
		if (!cell) {
			cell = { cellId, provenance: null, status: "unknown", code: null, outputs: [] };
			cells.set(cellId, cell);
		}
		if (o.customType === "repl_cell") {
			if (typeof d.provenance === "string") cell.provenance = d.provenance;
			if (typeof d.status === "string") cell.status = d.status;
			if (typeof d.code === "string") cell.code = d.code;
			if (typeof d.tool_call_id === "string") cell.toolCallId = d.tool_call_id;
			if (typeof d.queued_at === "string") cell.queuedAt = d.queued_at;
			if (typeof d.started_at === "string") cell.startedAt = d.started_at;
			if (typeof d.finished_at === "string") cell.finishedAt = d.finished_at;
			if (typeof d.duration_ms === "number") cell.durationMs = d.duration_ms;
			if (d.error && typeof d.error === "object") cell.error = d.error as { ename: string; evalue: string };
		} else {
			cell.outputs.push({
				stream: String(d.stream ?? "stdout"),
				data: String(d.data ?? ""),
				mimeType: typeof d.mime_type === "string" ? d.mime_type : undefined,
				truncated: d.truncated === true ? true : undefined,
			});
		}
	}
	return [...cells.values()];
}

// ---- subagent transcript (M8 tree-walk reuse on the child's file) -----------

export interface SubagentTranscriptResult {
	sessionId: string;
	childId: string;
	childSessionId: string | null;
	sessionFile: string;
	blocks: TranscriptBlock[];
	replCells: ReplCell[];
}

/** Resolve the child's session file inside <parentDir>/sub-<childId>/.
 * Prefers the file whose name carries the lifecycle-recorded child session
 * id; falls back to the sole file, then the lexically newest (ts-prefixed). */
export function resolveChildSessionFile(parentSessionFile: string, childId: string, childSessionId: string | null): string | null {
	const dir = path.join(path.dirname(parentSessionFile), `sub-${childId}`);
	if (!existsSync(dir)) return null;
	let files: string[];
	try {
		files = readdirSync(dir).filter((f) => f.endsWith(".jsonl"));
	} catch {
		return null;
	}
	if (files.length === 0) return null;
	if (childSessionId) {
		const exact = files.find((f) => f.includes(childSessionId));
		if (exact) return path.join(dir, exact);
	}
	files.sort();
	return path.join(dir, files[files.length - 1]);
}

// ---- comms -------------------------------------------------------------------

export interface CommsMessage {
	id: string;
	ts: string;
	direction: "in" | "out";
	from: { sessionId: string; name?: string; depth?: number };
	to: { sessionId: string; name?: string } | null;
	role?: string;
	content: string;
	deliveryStatus: string;
	deliveryMode?: string;
	detail?: string;
}

/** Inbound: custom_message agent_message entries persisted into THIS
 * session's context by prime-comms delivery. Delivery into the session file
 * is the delivery proof → status "delivered". */
export function inboundComms(sessionFile: string, sessionId: string): CommsMessage[] {
	const out: CommsMessage[] = [];
	for (const line of readLines(sessionFile)) {
		if (!line.includes("agent_message")) continue;
		let o: {
			type?: string;
			customType?: string;
			details?: { id?: string; message?: string; from?: { sessionId: string; name?: string; depth?: number } };
			timestamp?: string;
		};
		try {
			o = JSON.parse(line);
		} catch {
			continue;
		}
		if (o.type !== "custom_message" || o.customType !== "agent_message") continue;
		const d = o.details ?? {};
		out.push({
			id: String(d.id ?? ""),
			ts: String(o.timestamp ?? ""),
			direction: "in",
			from: d.from ?? { sessionId: "unknown" },
			to: { sessionId },
			content: String(d.message ?? ""),
			deliveryStatus: "delivered",
		});
	}
	return out;
}

/** Outbound: fold the outbox by message id — the LAST record per id is the
 * terminal truth (queued → delivered/failed/persisted/recovered). */
export function outboundComms(sessionDir: string, sessionId: string): CommsMessage[] {
	const outboxPath = path.join(sessionDir, "prime", sessionId, "comms", "outbox.jsonl");
	const last = new Map<string, CommsMessage & { order: number }>();
	let order = 0;
	for (const line of readLines(outboxPath)) {
		const trimmed = line.trim();
		if (!trimmed) continue;
		let r: {
			id?: string;
			ts?: string;
			from?: { sessionId: string; name?: string; depth?: number };
			role?: string;
			receiverName?: string;
			target?: { sessionId?: string; name?: string };
			message?: string;
			status?: string;
			detail?: string;
			deliveryMode?: string;
		};
		try {
			r = JSON.parse(trimmed);
		} catch {
			continue; // torn tail line (crash mid-append)
		}
		if (typeof r.id !== "string" || typeof r.status !== "string") continue;
		order += 1;
		last.set(r.id, {
			id: r.id,
			ts: String(r.ts ?? ""),
			direction: "out",
			from: r.from ?? { sessionId },
			to: r.target?.sessionId ? { sessionId: r.target.sessionId, name: r.target.name ?? r.receiverName } : r.receiverName ? { sessionId: "", name: r.receiverName } : null,
			role: r.role,
			content: String(r.message ?? ""),
			deliveryStatus: r.status,
			deliveryMode: r.deliveryMode,
			detail: r.detail,
			order,
		});
	}
	return [...last.values()].map(({ order: _order, ...m }) => m);
}

/** session row + its session dir (dirname of the session file — the prime/
 * dir lives next to the file for roots AND rlm children). */
export async function sessionFileInfo(sessionId: string): Promise<{ sessionFile: string; sessionDir: string }> {
	const row = await db.getSession(sessionId);
	if (!row) throw new BridgeError("session_not_found", `unknown session ${sessionId}`);
	if (!row.session_file) throw new BridgeError("session_not_found", `session ${sessionId} has no session file yet`);
	return { sessionFile: row.session_file, sessionDir: path.dirname(row.session_file) };
}
