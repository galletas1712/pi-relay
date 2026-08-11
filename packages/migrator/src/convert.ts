// TranscriptItem (pi-relay, rust/crates/agent-vocab) -> pi session v3 entries.
// Model-visible mapping (verified against pi-relay providers + pi-coding-agent loader):
//   user_message            -> message{role:user}
//   assistant_message       -> message{role:assistant, content:[text|toolCall], usage zeros, stopReason}
//   tool_result             -> message{role:toolResult}
//   daemon_tool_observation -> custom_message (model-visible user-role text, as in pi-relay openai.rs)
//   compaction_summary      -> compaction{summary,tokensBefore,firstKeptEntryId:null} (null keeps NOTHING
//                              pre-compaction in model context = pi-relay's summary+suffix semantics)
//   turn_started/turn_finished/tool_call_started -> SKIPPED (not model-visible in pi-relay either;
//                              tool_call_started arms are empty in openai.rs/anthropic.rs). Counted for audit.
// provider_replay           -> dropped (openai-native replay state; meaningless to the GLM stack). Counted.
// Parent links skip over non-emitted entries (turn boundaries) to the nearest emitted ancestor.
import type { EntryRow } from "./pgsource.ts";
import { EntryIds } from "./ids.ts";

export interface V3Entry {
	type: string;
	id?: string;
	parentId?: string | null;
	timestamp?: string;
	[k: string]: unknown;
}

export interface ConvStats {
	sourceEntries: number;
	emittedEntries: number;
	byType: Record<string, number>;
	skippedByType: Record<string, number>;
	replayDropped: number;
	argsParseFailures: number;
	crossSessionCompactions: number;
	compactions: number;
}

const ZERO_USAGE = {
	input: 0,
	output: 0,
	cacheRead: 0,
	cacheWrite: 0,
	totalTokens: 0,
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
};

function isoFromMs(ms: string | number): string {
	return new Date(Number(ms)).toISOString();
}

function apiFor(kind: string | undefined): string {
	return kind === "claude" ? "anthropic-messages" : "openai-responses";
}

function effortToThinking(effort: string | undefined): string | null {
	if (!effort) return null;
	const e = effort.toLowerCase();
	if (e === "none" || e === "off") return "off";
	if (["minimal", "low", "medium", "high", "xhigh", "max"].includes(e)) return e;
	return "high"; // unknown effort: conservative default, provenance kept in session-info custom entry
}

/** Model-visible canonical content of ONE source item, for V1 chain-hash comparison.
 * Returns null for non-model-visible items (skipped in v3). */
export function canonicalSourceContent(item: Record<string, unknown>, ms?: number): unknown | null {
	const t = item.type as string;
	switch (t) {
		case "user_message": {
			const blocks = (item.content as Array<Record<string, unknown>>) ?? [];
			return ["user", ms ?? 0, blocks.map((b) => (b.text !== undefined ? ["text", String(b.text)] : ["image", JSON.stringify(b)]))];
		}
		case "assistant_message": {
			const items = (item.items as Array<Record<string, unknown>>) ?? [];
			return [
				"assistant",
				ms ?? 0,
				items.map((it) =>
					it.type === "tool_call"
						? ["toolCall", String(it.id), String(it.tool_name ?? it.name ?? ""), canonArgs(String(it.args_json ?? ""))]
						: ["text", String(it.text ?? "")],
				),
			];
		}
		case "tool_result":
			return ["toolResult", ms ?? 0, String(item.tool_call_id ?? ""), String(item.tool_name ?? ""), String(item.output ?? ""), String(item.status ?? "")];
		case "daemon_tool_observation":
			return ["daemonObservation", ms ?? 0, String(item.tool_name ?? ""), String(item.summary ?? ""), String(item.status ?? "")];
		case "compaction_summary":
			return ["compaction", ms ?? 0, String(item.summary ?? ""), Number(item.tokens_before ?? 0)];
		default:
			return null; // turn_started, turn_finished, tool_call_started: not model-visible
	}
}

function imageBlock(image: Record<string, unknown> | undefined): Record<string, unknown> {
	const mime = String(image?.mime_type ?? "image/png");
	const src = image?.source as Record<string, unknown> | undefined;
	if (src?.kind === "base64") return { type: "image", data: String(src.value ?? ""), mimeType: mime };
	return { type: "text", text: `[image: ${String(src?.value ?? "unavailable")}]` };
}

function canonArgs(argsJson: string): string {
	try {
		return JSON.stringify(JSON.parse(argsJson));
	} catch {
		return JSON.stringify(argsJson);
	}
}

function parseArgs(argsJson: unknown, stats: ConvStats): Record<string, unknown> {
	const s = String(argsJson ?? "");
	try {
		const v = JSON.parse(s);
		if (v !== null && typeof v === "object" && !Array.isArray(v)) return v as Record<string, unknown>;
		return { value: v };
	} catch {
		stats.argsParseFailures += 1;
		return { raw: s };
	}
}

export interface ConvertedSession {
	entries: V3Entry[]; // WITHOUT header; caller prepends header + appends marker
	idMap: Record<string, string>; // source entry id -> new id (emitted entries only)
	activeBranchNewIds: string[]; // new ids on the active path (incl. marker anchor)
	activeLeafNewId: string | null;
	stats: ConvStats;
	/** canonical model-visible contents along the ACTIVE branch (V1 comparison) */
	activeBranchContents: unknown[];
}

const EMITTED = new Set(["user_message", "assistant_message", "tool_result", "daemon_tool_observation", "compaction_summary"]);

export function convertSessionEntries(
	sessionId: string,
	rows: EntryRow[],
	activeLeafId: string | null,
	provider: { kind?: string; model?: string; reasoning_effort?: string } | null,
): ConvertedSession {
	const ids = new EntryIds(sessionId);
	const stats: ConvStats = {
		sourceEntries: rows.length,
		emittedEntries: 0,
		byType: {},
		skippedByType: {},
		replayDropped: 0,
		argsParseFailures: 0,
		crossSessionCompactions: 0,
		compactions: 0,
	};
	const byId = new Map<string, EntryRow>();
	for (const r of rows) byId.set(r.id, r);

	// effective parent: nearest emitted ancestor (skipping turn boundaries)
	const effParent = (r: EntryRow): string | null => {
		let p = r.parent_id;
		let hops = 0;
		while (p && hops < rows.length + 8) {
			const pr = byId.get(p);
			if (!pr) return null; // parent outside this session's rows (shouldn't happen)
			if (EMITTED.has(pr.item.type as string)) return p;
			p = pr.parent_id;
			hops += 1;
		}
		return null;
	};
	// for compaction: effective EMITTED entry for a (possibly skipped) source leaf id
	const effEntry = (id: string | null | undefined): string | null => {
		let p = id ?? null;
		let hops = 0;
		while (p && hops < rows.length + 8) {
			const pr = byId.get(p);
			if (!pr) return null;
			if (EMITTED.has(pr.item.type as string)) return p;
			p = pr.parent_id;
			hops += 1;
		}
		return null;
	};

	const out: V3Entry[] = [];
	const idMap: Record<string, string> = {};
	const newIdOf = new Map<string, string>();

	for (const r of rows) {
		const item = r.item;
		const t = item.type as string;
		if (r.has_replay) stats.replayDropped += 1;
		if (!EMITTED.has(t)) {
			stats.skippedByType[t] = (stats.skippedByType[t] ?? 0) + 1;
			continue;
		}
		stats.byType[t] = (stats.byType[t] ?? 0) + 1;
		const id = ids.id(r.id);
		idMap[r.id] = id;
		newIdOf.set(r.id, id);
		const parentSrc = t === "compaction_summary" ? null : effParent(r); // compaction parent resolved below
		const parentId = parentSrc ? (newIdOf.get(parentSrc) ?? null) : null;
		const timestamp = isoFromMs(r.timestamp_ms);
		const tsMs = Number(r.timestamp_ms);

		if (t === "user_message") {
			const blocks = ((item.content as Array<Record<string, unknown>>) ?? []).map((b) =>
				b.text !== undefined
					? { type: "text", text: String(b.text) }
					: imageBlock(b.image as Record<string, unknown> | undefined),
			);
			out.push({ type: "message", id, parentId, timestamp, message: { role: "user", content: blocks, timestamp: tsMs } });
		} else if (t === "assistant_message") {
			const items = (item.items as Array<Record<string, unknown>>) ?? [];
			const content = items.map((it) =>
				it.type === "tool_call"
					? { type: "toolCall", id: String(it.id), name: String(it.tool_name ?? it.name ?? ""), arguments: parseArgs(it.args_json, stats) }
					: { type: "text", text: String(it.text ?? "") },
			);
			const hasCalls = content.some((c) => c.type === "toolCall");
			out.push({
				type: "message",
				id,
				parentId,
				timestamp,
				message: {
					role: "assistant",
					content,
					api: apiFor(provider?.kind),
					provider: provider?.kind ?? "openai",
					model: provider?.model ?? "unknown",
					usage: { ...ZERO_USAGE },
					stopReason: hasCalls ? "toolUse" : "stop",
					timestamp: tsMs,
				},
			});
		} else if (t === "tool_result") {
			out.push({
				type: "message",
				id,
				parentId,
				timestamp,
				message: {
					role: "toolResult",
					toolCallId: String(item.tool_call_id ?? ""),
					toolName: String(item.tool_name ?? ""),
					content: [{ type: "text", text: String(item.output ?? "") }],
					isError: String(item.status ?? "Success") !== "Success", // Success|Error|Crashed
					details: { pi_relay_status: String(item.status ?? "Success") },
					timestamp: tsMs,
				},
			});
		} else if (t === "daemon_tool_observation") {
			out.push({
				type: "custom_message",
				id,
				parentId,
				timestamp,
				customType: "pi-relay-daemon-observation",
				content: [{ type: "text", text: `[${String(item.tool_name ?? "daemon")} ${String(item.status ?? "")}] ${String(item.summary ?? "")}` }],
				display: true,
				details: { pi_relay: { tool_name: item.tool_name ?? null, status: item.status ?? null, summary: item.summary ?? null, args_json: item.args_json ?? null } },
			});
		} else if (t === "compaction_summary") {
			stats.compactions += 1;
			const srcSession = String(item.source_session_id ?? "");
			const srcLeaf = item.source_leaf_id as string | undefined;
			let cParent: string | null = null;
			if (srcSession === sessionId && srcLeaf) {
				const eff = effEntry(srcLeaf);
				cParent = eff ? (newIdOf.get(eff) ?? null) : null;
				if (eff && cParent === null) {
					// source leaf comes LATER in sequence order than the compaction (shouldn't happen); keep null
				}
			} else {
				stats.crossSessionCompactions += 1;
			}
			out.push({
				type: "compaction",
				id,
				parentId: cParent,
				timestamp,
				summary: String(item.summary ?? ""),
				firstKeptEntryId: null, // nothing pre-compaction retained = pi-relay summary+suffix semantics
				tokensBefore: Number(item.tokens_before ?? 0),
				details: {
					pi_relay: {
						source_session_id: item.source_session_id ?? null,
						source_leaf_id: item.source_leaf_id ?? null,
						last_turn_id: item.last_turn_id ?? null,
						turn_started_at_ms: item.turn_started_at_ms ?? null,
					},
				},
			});
		}
	}
	stats.emittedEntries = out.length;

	// active branch: walk from active_leaf_id with compaction source hops (same-session only),
	// mirroring rust agent-store transcript.rs active_branch CTE.
	const activeContents: unknown[] = [];
	const activeNewIds: string[] = [];
	{
		const path: EntryRow[] = [];
		const seen = new Set<string>();
		let cur: string | null = activeLeafId ?? (rows.length > 0 ? rows[rows.length - 1].id : null);
		while (cur && !seen.has(cur)) {
			seen.add(cur);
			const r: EntryRow | undefined = byId.get(cur);
			if (!r) break;
			path.push(r);
			if ((r.item.type as string) === "compaction_summary") {
				const src = r.item.source_session_id === sessionId ? (r.item.source_leaf_id as string | undefined) : undefined;
				cur = src ?? null;
			} else {
				cur = r.parent_id;
			}
		}
		path.reverse();
		for (const r of path) {
			const c = canonicalSourceContent(r.item, Number(r.timestamp_ms));
			if (c !== null) activeContents.push(c);
			const nid = newIdOf.get(r.id);
			if (nid) activeNewIds.push(nid);
		}
	}

	const activeLeafNewId = activeNewIds.length > 0 ? activeNewIds[activeNewIds.length - 1] : null;
	return { entries: out, idMap, activeBranchNewIds: activeNewIds, activeLeafNewId, stats, activeBranchContents: activeContents };
}

export function thinkingEntry(provider: { kind?: string; model?: string; reasoning_effort?: string } | null): V3Entry | null {
	const thinking = effortToThinking(provider?.reasoning_effort);
	if (!thinking) return null;
	return { type: "thinking_level_change", thinkingLevel: thinking };
}
