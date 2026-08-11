// Transcript rebuild (M8, G2; M11b-enriched): parse a pi session JSONL (v3,
// tree with id/parentId) into the web projection's TranscriptBlock[] for the
// CURRENT branch (leaf = last entry with an id, walk parents to the root).
// Used by session.getState so a client that hit event_gap (trimmed spool) can
// rebuild the conversation without the live event stream.
//
// M11b block kinds (additive): beyond message/tool the branch walk now also
// surfaces comms (prime-comms agent_message deliveries), repl_cell (folded
// repl_cell+repl_output custom entries — the ipython tool's cell detail), and
// compaction summaries, all in branch order. Unknown/custom entry types are
// skipped by design (the UI renders unknowns as NOTHING per the owner's
// transcript rules).
// M11c: assistant message entries with no text AND no thinking (tool-call-only
// steps) emit NO message block — their tool blocks still emit; an empty block
// would render as an empty assistant bubble in the web transcript.
import { readFileSync } from "node:fs";

export interface MessageBlock {
	kind: "message";
	id: string;
	role: string;
	text: string;
	thinking: string;
	complete: boolean;
	ts?: string;
}

export interface ToolExecBlock {
	kind: "tool";
	toolCallId: string;
	toolName: string;
	args?: string;
	result?: string;
	isError?: boolean;
	done: boolean;
	ts?: string;
}

/** M11b: inbound prime-comms delivery persisted in this session's context. */
export interface CommsBlock {
	kind: "comms";
	id: string;
	ts?: string;
	direction: "in";
	fromSessionId: string | null;
	fromName: string | null;
	message: string;
}

/** M11b: one ipython cell (code + captured outputs), folded from the
 * repl_cell/repl_output custom entries on the branch. */
export interface ReplCellBlock {
	kind: "repl_cell";
	id: string;
	ts?: string;
	cellId: string;
	provenance: string | null;
	status: string;
	code: string | null;
	toolCallId?: string;
	durationMs?: number;
	error?: { ename: string; evalue: string };
	outputs: Array<{ stream: string; data: string; mimeType?: string; truncated?: boolean }>;
}

/** M11b: pi compaction entry (manual or auto). */
export interface CompactionBlock {
	kind: "compaction";
	id: string;
	ts?: string;
	summary: string;
	tokensBefore?: number;
	fromHook?: boolean;
}

export type TranscriptBlock = MessageBlock | ToolExecBlock | CommsBlock | ReplCellBlock | CompactionBlock;

export interface SessionRecord {
	type?: string;
	customType?: string;
	id?: string;
	parentId?: string | null;
	timestamp?: string;
	// header fields (type === "session")
	version?: number;
	cwd?: string | null;
	message?: {
		role?: string;
		content?: Array<Record<string, unknown>>;
		toolCallId?: string;
		toolName?: string;
		isError?: boolean;
	};
	details?: Record<string, unknown>;
	data?: Record<string, unknown>;
	summary?: string;
	tokensBefore?: number;
	fromHook?: boolean;
}

/** Full JSONL parse (header + entries) in file order. Shared with fork.ts. */
export function readSessionRecords(sessionFile: string | null): SessionRecord[] {
	if (!sessionFile) return [];
	let raw: string;
	try {
		raw = readFileSync(sessionFile, "utf8");
	} catch {
		return [];
	}
	const out: SessionRecord[] = [];
	for (const line of raw.split("\n")) {
		if (!line.trim()) continue;
		try {
			out.push(JSON.parse(line) as SessionRecord);
		} catch {
			/* torn tail line — skip */
		}
	}
	return out;
}

/** Current-branch entry list (leaf→root walk, reversed into chronological
 * order). Branch tip = last record with an id. Corrupt parent cycles stop. */
export function currentBranch(records: SessionRecord[]): SessionRecord[] {
	const byId = new Map<string, SessionRecord>();
	let leaf: string | null = null;
	for (const rec of records) {
		if (typeof rec.id === "string" && rec.id) {
			byId.set(rec.id, rec);
			leaf = rec.id; // file order: last id is the branch tip
		}
	}
	if (!leaf) return [];
	const branch: SessionRecord[] = [];
	const seen = new Set<string>();
	for (let cur: string | null = leaf; cur; ) {
		if (seen.has(cur)) break;
		seen.add(cur);
		const entry = byId.get(cur);
		if (!entry) break;
		branch.push(entry);
		cur = typeof entry.parentId === "string" ? entry.parentId : null;
	}
	branch.reverse();
	return branch;
}

function textOf(content: Array<Record<string, unknown>> | undefined, type: string, field: string): string {
	return (content ?? [])
		.filter((c) => c?.type === type)
		.map((c) => String(c[field] ?? ""))
		.join("");
}

export interface BranchTranscript {
	blocks: TranscriptBlock[];
	/** Real id of the branch tip entry (any kind — session header, custom,
	 * message). The legacy daemon's active leaf is this tip, never null for a
	 * session with history; M11b's adapter reports it when the branch has no
	 * renderable blocks. */
	tipId: string | null;
}

/** Rebuild the current-branch transcript + tip. Empty result for
 * missing/unreadable files. */
export function rebuildBranchTranscript(sessionFile: string | null): BranchTranscript {
	const branch = currentBranch(readSessionRecords(sessionFile));

	const blocks: TranscriptBlock[] = [];
	const toolByCallId = new Map<string, ToolExecBlock>();
	const cellByCellId = new Map<string, ReplCellBlock>();
	for (const entry of branch) {
		if (entry.type === "custom") {
			// ipython cell lifecycle (model tool calls + user console cells).
			if (entry.customType === "repl_cell") {
				const d = entry.data ?? {};
				const cellId = typeof d.cell_id === "string" ? d.cell_id : "";
				if (!cellId) continue;
				let cell = cellByCellId.get(cellId);
				if (!cell) {
					cell = {
						kind: "repl_cell",
						id: String(entry.id ?? cellId),
						ts: entry.timestamp,
						cellId,
						provenance: null,
						status: "unknown",
						code: null,
						outputs: [],
					};
					cellByCellId.set(cellId, cell);
					blocks.push(cell);
				}
				if (typeof d.provenance === "string") cell.provenance = d.provenance;
				if (typeof d.status === "string") cell.status = d.status;
				if (typeof d.code === "string") cell.code = d.code;
				if (typeof d.tool_call_id === "string") cell.toolCallId = d.tool_call_id;
				if (typeof d.duration_ms === "number") cell.durationMs = d.duration_ms;
				if (d.error && typeof d.error === "object") cell.error = d.error as { ename: string; evalue: string };
				continue;
			}
			if (entry.customType === "repl_output") {
				const d = entry.data ?? {};
				const cellId = typeof d.cell_id === "string" ? d.cell_id : "";
				if (!cellId) continue;
				let cell = cellByCellId.get(cellId);
				if (!cell) {
					// output without its cell on this branch (pruned) — keep visible
					cell = {
						kind: "repl_cell",
						id: String(entry.id ?? cellId),
						ts: entry.timestamp,
						cellId,
						provenance: null,
						status: "unknown",
						code: null,
						outputs: [],
					};
					cellByCellId.set(cellId, cell);
					blocks.push(cell);
				}
				cell.outputs.push({
					stream: String(d.stream ?? "stdout"),
					data: String(d.data ?? ""),
					mimeType: typeof d.mime_type === "string" ? d.mime_type : undefined,
					truncated: d.truncated === true ? true : undefined,
				});
				continue;
			}
			continue; // every other custom entry renders as NOTHING
		}
		if (entry.type === "custom_message") {
			if (entry.customType !== "agent_message") continue;
			const d = (entry.details ?? {}) as { from?: { sessionId?: string; name?: string }; message?: unknown };
			blocks.push({
				kind: "comms",
				id: String(entry.id),
				ts: entry.timestamp,
				direction: "in",
				fromSessionId: d.from?.sessionId ?? null,
				fromName: d.from?.name ?? null,
				message: typeof d.message === "string" ? d.message : "",
			});
			continue;
		}
		if (entry.type === "compaction") {
			blocks.push({
				kind: "compaction",
				id: String(entry.id),
				ts: entry.timestamp,
				summary: typeof entry.summary === "string" ? entry.summary : "",
				tokensBefore: typeof entry.tokensBefore === "number" ? entry.tokensBefore : undefined,
				fromHook: entry.fromHook === true ? true : undefined,
			});
			continue;
		}
		if (entry.type !== "message" || !entry.message) continue;
		const msg = entry.message;
		if (msg.role === "toolResult") {
			const callId = String(msg.toolCallId ?? "");
			const tool = toolByCallId.get(callId);
			const resultText = textOf(msg.content, "text", "text");
			if (tool) {
				tool.result = resultText;
				tool.isError = msg.isError === true;
				tool.done = true;
			} else {
				// tool call happened on a pruned branch; keep the result visible
				const orphan: ToolExecBlock = {
					kind: "tool",
					toolCallId: callId,
					toolName: String(msg.toolName ?? "tool"),
					result: resultText,
					isError: msg.isError === true,
					done: true,
					ts: entry.timestamp,
				};
				toolByCallId.set(callId, orphan);
				blocks.push(orphan);
			}
			continue;
		}
		if (msg.role !== "user" && msg.role !== "assistant") continue;
		const id = String(entry.id);
		const text = textOf(msg.content, "text", "text");
		const thinking = textOf(msg.content, "thinking", "thinking");
		// M11c (empty-bubble fix): skip assistant message blocks that carry no
		// text AND no thinking (tool-call-only steps) — such a block would
		// render as an empty assistant bubble and inflate the turn's
		// agent-message count; the tool blocks below remain the visible trace.
		// User messages always emit (an empty user bubble still marks the turn).
		if (msg.role === "user" || text.trim().length > 0 || thinking.trim().length > 0) {
			blocks.push({
				kind: "message",
				id,
				role: msg.role,
				text,
				thinking,
				complete: true, // a persisted message is complete by definition
				ts: entry.timestamp,
			});
		}
		if (msg.role === "assistant") {
			for (const c of msg.content ?? []) {
				if (c?.type !== "toolCall") continue;
				const tool: ToolExecBlock = {
					kind: "tool",
					toolCallId: String(c.id ?? ""),
					toolName: String(c.name ?? "tool"),
					args: typeof c.arguments === "string" ? c.arguments : JSON.stringify(c.arguments ?? {}),
					done: false,
					ts: entry.timestamp,
				};
				toolByCallId.set(tool.toolCallId, tool);
				blocks.push(tool);
			}
		}
	}
	return { blocks, tipId: branch.length > 0 ? String(branch[branch.length - 1].id ?? "") || null : null };
}

/** Rebuild the current-branch transcript. Returns [] for missing/unreadable files. */
export function rebuildTranscript(sessionFile: string | null): TranscriptBlock[] {
	return rebuildBranchTranscript(sessionFile).blocks;
}
