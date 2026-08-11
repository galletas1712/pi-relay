// M11b: bridge TranscriptBlock[] → legacy daemon-shaped TranscriptEntry[] /
// TurnCard[] synthesis. Pure module — no I/O, no React; unit-testable.
//
// The bridge transcript is a per-branch block projection of the pi session
// file (message / tool / comms / repl_cell / compaction blocks with REAL pi
// entry ids where the block maps to a persisted entry). The legacy daemon
// transcript is an entry tree with explicit turn boundaries. This module
// re-synthesizes the daemon shape:
//
//   message(user)      → turn boundary: close any open turn
//                        (turn_finished{outcome:"Graceful"}), emit
//                        turn_started{turn_id}, then user_message entry
//   message(assistant) → assistant_message entry {items:[text?]} (thinking is
//                        dropped — the legacy renderer has no thinking part)
//   tool               → tool_call item folded into the open assistant entry
//                        + a tool_result entry (paired by tool_call_id; the
//                        legacy indexToolEntries merges them into the tool
//                        group). Orphan tool blocks (no open assistant entry)
//                        emit only the standalone tool_result.
//   comms              → comms_message entry (M11b additive variant — the one
//                        properly-marked non-chat row)
//   compaction         → compaction_summary entry (source_leaf_id = previous
//                        entry in the synthesized chain)
//   repl_cell          → SKIPPED in the transcript. Model ipython cells are
//                        already represented by their paired tool blocks
//                        (linked by toolCallId); user console cells live in
//                        the REPL pane.
//
// Turn ids and boundary entry ids are DETERMINISTIC (position-based) so a
// live continuation and a rebuild agree on ids wherever content agrees.
// Sequence numbers are dense per synthesized branch (1..N) — they only feed
// client-side pagination ranges.

import type { TranscriptBlock } from "./types.ts";
import type { AssistantItem, TranscriptEntry, TurnCard } from "../types.ts";
import { appendTurnCard } from "../selectedSessionCache/turns.ts";

export interface SynthesisCounters {
	/** Next turn number (1-based; incremented per user message). */
	turnId: number;
	/** Next dense sequence number (1-based). */
	sequence: number;
}

export interface SynthesizedTranscript {
	entries: TranscriptEntry[];
	cards: TurnCard[];
	/** Id of the last entry in the synthesized branch (may be synthetic). */
	leafId: string | null;
	/** Id of the last entry backed by a REAL persisted pi entry (message /
	 * comms / compaction). Null when the branch carries no pi entries. */
	lastRealEntryId: string | null;
	counters: SynthesisCounters;
	/** True when the final turn was left open (no turn_finished) — the caller
	 * closes it when the session is idle at rebuild time. */
	finalTurnOpen: boolean;
}

export function initialCounters(): SynthesisCounters {
	return { turnId: 0, sequence: 0 };
}

export function synthesizeEntriesFromBlocks(
	sessionId: string,
	blocks: TranscriptBlock[],
	counters: SynthesisCounters = initialCounters(),
	options: { closeFinalTurn?: boolean } = {},
): SynthesizedTranscript {
	const entries: TranscriptEntry[] = [];
	let { turnId, sequence } = counters;
	let lastId: string | null = null;
	let lastRealId: string | null = null;
	let turnOpen = false;
	let foldAssistant: TranscriptEntry | null = null; // last assistant entry while its tool group runs

	const ts = (iso: string | undefined): number => {
		if (!iso) return Date.now();
		const ms = Date.parse(iso);
		return Number.isNaN(ms) ? Date.now() : ms;
	};
	const push = (id: string, parentId: string | null, timestampMs: number, item: TranscriptEntry["item"]): TranscriptEntry => {
		sequence += 1;
		const entry: TranscriptEntry = { id, parent_id: parentId, timestamp_ms: timestampMs, sequence, item };
		entries.push(entry);
		lastId = id;
		return entry;
	};
	const closeTurn = (timestampMs: number) => {
		if (!turnOpen) return;
		push(`tf-${turnId}`, lastId, timestampMs, { type: "turn_finished", turn_id: turnId, outcome: "Graceful" });
		turnOpen = false;
		foldAssistant = null;
	};

	for (const block of blocks) {
		if (block.kind === "message" && block.role === "user") {
			const at = ts(block.ts);
			closeTurn(at);
			turnId += 1;
			turnOpen = true;
			foldAssistant = null;
			push(`ts-${turnId}`, lastId, at, { type: "turn_started", turn_id: turnId });
			push(block.id, lastId, at, {
				type: "user_message",
				content: [{ type: "text", text: block.text }],
			});
			lastRealId = block.id;
			continue;
		}
		if (block.kind === "message" && block.role === "assistant") {
			const items: AssistantItem[] = [];
			if (block.text.trim().length > 0) items.push({ type: "text", text: block.text });
			// M11c (empty-bubble fix): skip textless assistant entries entirely.
			// Thinking is dropped by this projection and tool blocks emit their
			// own tool_result entry either way, so a textless assistant block
			// would surface as an empty bubble (and inflate the turn's
			// agent-message count). Unfolded tool results render as standalone
			// entries (grouped by the display model).
			if (items.length === 0) {
				foldAssistant = null;
				continue;
			}
			foldAssistant = push(block.id, lastId, ts(block.ts), { type: "assistant_message", items });
			lastRealId = block.id;
			continue;
		}
		if (block.kind === "tool") {
			const at = ts(block.ts);
			if (foldAssistant && foldAssistant.item.type === "assistant_message") {
				foldAssistant.item.items.push({
					type: "tool_call",
					id: block.toolCallId,
					tool_name: block.toolName,
					args_json: block.args ?? "",
				});
			}
			push(`tr-${block.toolCallId}`, lastId, at, {
				type: "tool_result",
				tool_call_id: block.toolCallId,
				tool_name: block.toolName,
				output: block.result ?? "",
				status: block.isError ? "Error" : "Success",
				// when nothing folded the call (textless assistant skipped), the
				// result carries the args so the group body can show the input
				...(foldAssistant ? {} : { args_json: block.args ?? "" }),
			});
			continue;
		}
		if (block.kind === "comms") {
			foldAssistant = null;
			push(block.id, lastId, ts(block.ts), {
				type: "comms_message",
				direction: block.direction,
				from_name: block.fromName ?? block.fromSessionId ?? "agent",
				to_name: "this session",
				message: block.message,
			});
			lastRealId = block.id;
			continue;
		}
		if (block.kind === "compaction") {
			foldAssistant = null;
			push(block.id, lastId, ts(block.ts), {
				type: "compaction_summary",
				source_session_id: sessionId,
				source_leaf_id: lastId ?? block.id,
				summary: block.summary,
				tokens_before: block.tokensBefore ?? null,
				last_turn_id: turnId,
			});
			lastRealId = block.id;
			continue;
		}
		// repl_cell + anything else: renders as nothing (owner rule).
	}

	let finalTurnOpen = turnOpen;
	if (turnOpen && options.closeFinalTurn) {
		closeTurn(entries.at(-1)?.timestamp_ms ?? Date.now());
		finalTurnOpen = false;
	}

	let cards = new Map<string, TurnCard>();
	let order: string[] = [];
	for (const entry of entries) {
		const next = appendTurnCard(cards, order, entry);
		cards = next.turnCardsById;
		order = next.turnOrder;
	}

	return {
		entries,
		cards: order.flatMap((id) => {
			const card = cards.get(id);
			return card ? [card] : [];
		}),
		leafId: lastId,
		lastRealEntryId: lastRealId,
		counters: { turnId, sequence },
		finalTurnOpen,
	};
}

/** Live-side synthesis helpers shared by the event synthesizer. */
export function synthesizeTurnStarted(counters: SynthesisCounters, parentId: string | null, timestampMs: number): { entry: TranscriptEntry; counters: SynthesisCounters } {
	const turnId = counters.turnId + 1;
	const sequence = counters.sequence + 1;
	return {
		entry: { id: `ts-${turnId}`, parent_id: parentId, timestamp_ms: timestampMs, sequence, item: { type: "turn_started", turn_id: turnId } },
		counters: { turnId, sequence },
	};
}

export function synthesizeTurnFinished(turnId: number, counters: SynthesisCounters, parentId: string | null, timestampMs: number): { entry: TranscriptEntry; counters: SynthesisCounters } {
	const sequence = counters.sequence + 1;
	return {
		entry: { id: `tf-${turnId}`, parent_id: parentId, timestamp_ms: timestampMs, sequence, item: { type: "turn_finished", turn_id: turnId, outcome: "Graceful" } },
		counters: { ...counters, sequence },
	};
}
