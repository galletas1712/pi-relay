import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { TranscriptEntry, TranscriptItem } from "./types.ts";

export type HistoryPlacement = "at" | "before";

export interface HistoryTargetOption {
	id: string | null;
	actionLeafId: string | null;
	expectedActiveLeafId?: string | null;
	sourceEntryId?: string;
	placement?: HistoryPlacement;
	restoreText?: string;
	turnLabel: string;
	title: string;
	preview: string;
	meta: string;
	isActive: boolean;
}

export interface HistoryTreeRow {
	entry: TranscriptEntry;
	depth: number;
	isActive: boolean;
	isOnActivePath: boolean;
	parentId?: string | null;
	isBranchRoot?: boolean;
	descendantCount?: number;
}

export interface HistoryEntryDisplay {
	turnLabel: string;
	title: string;
	preview: string;
	meta: string;
}

type AssistantTranscriptEntry = TranscriptEntry & {
	item: Extract<TranscriptItem, { type: "assistant_message" }>;
};

export function historyForkOptions(entries: TranscriptEntry[], activeLeafId: string | null): HistoryTargetOption[] {
	const options: HistoryTargetOption[] = [];
	for (const entry of entries) {
		if (entry.item.type === "turn_started") continue;
		const branch = branchEntriesFor(entries, entry.id);
		const index = branch.length - 1;
		const option = forkOptionForEntry(entry, branch, index, turnIdAt(branch, index), activeLeafId);
		if (option) options.push(option);
	}
	return options.reverse();
}

export function historySwitchOptions(entries: TranscriptEntry[], activeLeafId: string | null): HistoryTargetOption[] {
	const options: HistoryTargetOption[] = [];
	for (const entry of entries) {
		const branch = branchEntriesFor(entries, entry.id);
		const option = switchOptionForEntry(entry, branch, branch.length - 1, turnIdAt(branch, branch.length - 1), activeLeafId);
		if (option) options.push(option);
	}
	return options.reverse();
}

export function historyTreeRows(entries: TranscriptEntry[], activeLeafId: string | null): HistoryTreeRow[] {
	const byId = new Map(entries.map((entry) => [entry.id, entry]));
	const order = new Map(entries.map((entry, index) => [entry.id, index]));
	const children = new Map<string | null, TranscriptEntry[]>();
	for (const entry of entries) {
		const parentId = entry.parent_id && byId.has(entry.parent_id) ? entry.parent_id : null;
		const siblings = children.get(parentId) ?? [];
		siblings.push(entry);
		children.set(parentId, siblings);
	}
	for (const siblings of children.values()) {
		siblings.sort((left, right) => (order.get(left.id) ?? 0) - (order.get(right.id) ?? 0));
	}

	const activePath = new Set(branchEntriesFor(entries, activeLeafId).map((entry) => entry.id));
	const sizeCache = new Map<string, number>();
	const branchSize = (entryId: string): number => {
		const cached = sizeCache.get(entryId);
		if (cached !== undefined) return cached;
		const size = 1 + (children.get(entryId) ?? []).reduce((sum, child) => sum + branchSize(child.id), 0);
		sizeCache.set(entryId, size);
		return size;
	};
	const rows: HistoryTreeRow[] = [];
	const visit = (entry: TranscriptEntry, depth: number, parentId: string | null, isBranchRoot: boolean) => {
		rows.push({
			entry,
			depth,
			isActive: activeLeafId === entry.id,
			isOnActivePath: activePath.has(entry.id),
			parentId,
			isBranchRoot,
			descendantCount: branchSize(entry.id) - 1
		});
		const entryChildren = children.get(entry.id) ?? [];
		const hasSplit = entryChildren.length > 1;
		const activeChild = entryChildren.find((child) => activePath.has(child.id));
		for (const child of entryChildren) {
			const isAlternateBranch = hasSplit && child.id !== activeChild?.id;
			visit(child, depth + (isAlternateBranch ? 1 : 0), entry.id, hasSplit);
		}
	};
	for (const root of children.get(null) ?? []) visit(root, 0, null, false);
	return rows;
}

export function historyEntryDisplay(entry: TranscriptEntry, entries: TranscriptEntry[]): HistoryEntryDisplay {
	const branch = branchEntriesFor(entries, entry.id);
	const index = branch.length - 1;
	const currentTurnId = turnIdAt(branch, index);
	const time = formatTimestamp(entry.timestamp_ms);
	const item = entry.item;
	if (item.type === "turn_started") {
		return {
			turnLabel: `t${item.turn_id}`,
			title: `Start of turn ${item.turn_id}`,
			preview: "Turn boundary opened.",
			meta: time
		};
	}
	const forkOption = forkOptionForEntry(entry, branch, index, currentTurnId, null);
	if (forkOption) {
		return {
			turnLabel: forkOption.turnLabel,
			title: forkOption.title,
			preview: forkOption.preview,
			meta: forkOption.meta
		};
	}
	if (item.type === "tool_call_started") {
		return {
			turnLabel: "tool",
			title: `Tool call: ${item.tool_call.tool_name}`,
			preview: truncate(item.tool_call.args_json, 96),
			meta: time
		};
	}
	return {
		turnLabel: currentTurnId ? `t${currentTurnId}` : "item",
		title: item.type.replaceAll("_", " "),
		preview: "Transcript entry.",
		meta: time
	};
}

export function branchEntriesFor(entries: TranscriptEntry[], leafId: string | null): TranscriptEntry[] {
	if (!leafId) return [];
	const byId = new Map(entries.map((entry) => [entry.id, entry]));
	const branch: TranscriptEntry[] = [];
	const seen = new Set<string>();
	let cursor: string | null = leafId;
	while (cursor && !seen.has(cursor)) {
		const entry = byId.get(cursor);
		if (!entry) break;
		branch.push(entry);
		seen.add(cursor);
		cursor = entry.parent_id;
	}
	return branch.reverse();
}

function turnIdAt(entries: TranscriptEntry[], index: number): number | null {
	const item = entries[index]?.item;
	if (item?.type === "turn_finished") return item.turn_id;
	if (item?.type === "compaction_summary") return item.last_turn_id;
	for (let cursor = index; cursor >= 0; cursor -= 1) {
		const candidate = entries[cursor].item;
		if (candidate.type === "turn_started") return candidate.turn_id;
		if (cursor !== index && candidate.type === "turn_finished") return null;
		if (candidate.type === "compaction_summary") return candidate.last_turn_id;
	}
	return null;
}

function forkOptionForEntry(
	entry: TranscriptEntry,
	entries: TranscriptEntry[],
	index: number,
	currentTurnId: number | null,
	activeLeafId: string | null
): HistoryTargetOption | null {
	const item = entry.item;
	const time = formatTimestamp(entry.timestamp_ms);
	const isActive = activeLeafId === entry.id;
	if (item.type === "user_message") {
		const text = contentBlocksToText(item.content);
		return {
			id: entry.id,
			actionLeafId: previousTurnBoundaryId(entries, index),
			sourceEntryId: entry.id,
			placement: "before",
			restoreText: text,
			turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
			title: currentTurnId ? `User message in turn ${currentTurnId}` : "User message",
			preview: truncate(text.trim() || "Empty user message.", 96),
			meta: time,
			isActive
		};
	}
	if (item.type === "assistant_message") {
		const toolNames = item.items
			.filter((assistantItem) => assistantItem.type === "tool_call")
			.map((assistantItem) => assistantItem.tool_name);
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: currentTurnId ? `a${currentTurnId}` : "asst",
			title: currentTurnId ? `Assistant message in turn ${currentTurnId}` : "Assistant message",
			preview: assistantPreview(item) || (toolNames.length ? `Tool call: ${toolNames.join(", ")}` : "Assistant message."),
			meta: time,
			isActive
		};
	}
	if (item.type === "tool_result") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: "tool",
			title: `Tool result: ${item.tool_name}`,
			preview: `${item.status.toLowerCase()}: ${truncate(firstLine(item.output) || "(empty)", 84)}`,
			meta: time,
			isActive
		};
	}
	if (item.type === "turn_finished") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: `t${item.turn_id}`,
			title: `End of turn ${item.turn_id}`,
			preview: `${item.outcome.toLowerCase()} turn boundary.`,
			meta: time,
			isActive
		};
	}
	if (item.type === "compaction_summary") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			placement: "at",
			turnLabel: `c${item.last_turn_id}`,
			title: "Compacted history",
			preview: truncate(item.summary, 96),
			meta: time,
			isActive
		};
	}
	return null;
}

function switchOptionForEntry(
	entry: TranscriptEntry,
	entries: TranscriptEntry[],
	index: number,
	currentTurnId: number | null,
	activeLeafId: string | null
): HistoryTargetOption | null {
	const item = entry.item;
	const time = formatTimestamp(entry.timestamp_ms);
	if (item.type === "user_message") {
		const text = contentBlocksToText(item.content);
		const actionLeafId = previousTurnBoundaryId(entries, index);
		return {
			id: entry.id,
			actionLeafId,
			expectedActiveLeafId: activeLeafId,
			sourceEntryId: entry.id,
			restoreText: text,
			turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
			title: "User message",
			preview: truncate(text.trim() || "Empty user message.", 96),
			meta: `edit · ${time}`,
			isActive: false
		};
	}
	if (item.type === "turn_finished") {
		const assistant = previousAssistantMessage(entries, index);
		return {
			id: entry.id,
			actionLeafId: entry.id,
			expectedActiveLeafId: activeLeafId,
			sourceEntryId: entry.id,
			turnLabel: `a${item.turn_id}`,
			title: "Assistant message",
			preview: assistant ? assistantPreview(assistant.item) || "Assistant message." : `${item.outcome.toLowerCase()} turn completed.`,
			meta: `switch · ${time}`,
			isActive: activeLeafId === entry.id
		};
	}
	if (item.type !== "compaction_summary") return null;
	const display = historyEntryDisplay(entry, entries);
	return {
		id: entry.id,
		actionLeafId: entry.id,
		expectedActiveLeafId: activeLeafId,
		sourceEntryId: entry.id,
		turnLabel: display.turnLabel,
		title: display.title,
		preview: display.preview,
		meta: display.meta,
		isActive: activeLeafId === entry.id
	};
}

function assistantPreview(item: Extract<TranscriptItem, { type: "assistant_message" }>): string {
	return truncate(
		item.items
			.map((assistantItem) => (assistantItem.type === "text" ? assistantItem.text : ""))
			.join(" ")
			.trim(),
		96
	);
}

function previousTurnBoundaryId(entries: TranscriptEntry[], beforeIndex: number): string | null {
	for (let index = beforeIndex - 1; index >= 0; index -= 1) {
		const entry = entries[index];
		if (entry.item.type === "turn_finished") return entry.id;
		if (entry.item.type === "compaction_summary") return entry.id;
	}
	return null;
}

function previousAssistantMessage(entries: TranscriptEntry[], beforeIndex: number): AssistantTranscriptEntry | null {
	for (let index = beforeIndex - 1; index >= 0; index -= 1) {
		const entry = entries[index];
		if (entry.item.type === "assistant_message") {
			return entry as AssistantTranscriptEntry;
		}
		if (entry.item.type === "turn_started" || entry.item.type === "turn_finished" || entry.item.type === "compaction_summary") return null;
	}
	return null;
}

function formatTimestamp(timestampMs: number): string {
	return new Date(timestampMs).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}
