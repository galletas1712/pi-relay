import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import { perfEnabled, perfLog, perfNow } from "./perf.ts";
import {
	buildTurnViews,
	modelStepPhaseLabel,
	modelStepPreview,
	modelStepTitle,
	terminalModelStep
} from "./turnView.ts";
import { displayParentIdForEntry, displayParentIdForNode } from "./displayParent.ts";
import type { TranscriptEntry, TranscriptTreeNode, TurnOutcome } from "./types.ts";
import type { ModelStepView, TurnView } from "./turnView.ts";

export interface HistoryTargetOption {
	id: string | null;
	actionLeafId: string | null;
	expectedActiveLeafId?: string | null;
	sourceEntryId?: string;
	restoreEntryId?: string;
	restoreText?: string;
	turnLabel: string;
	title: string;
	preview: string;
	meta: string;
	isActive: boolean;
	outcome?: TurnOutcome;
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

export function historySwitchOptions(entries: TranscriptEntry[], activeLeafId: string | null): HistoryTargetOption[] {
	return measureHistoryDerivation("historySwitchOptions", entries, () => historyBranchPointOptions(entries, activeLeafId));
}

export function historySwitchOptionsFromNodes(nodes: TranscriptTreeNode[], activeLeafId: string | null): HistoryTargetOption[] {
	return measureHistoryDerivation("historySwitchOptionsFromNodes", nodes, () => historyBranchPointOptionsFromNodes(nodes, activeLeafId));
}

export function nodeBranchIds(nodes: TranscriptTreeNode[], leafId: string | null): string[] {
	if (!leafId) return [];
	const byId = new Map(nodes.map((node) => [node.id, node]));
	const branch: string[] = [];
	const seen = new Set<string>();
	let cursor: string | null = leafId;
	while (cursor && !seen.has(cursor)) {
		const node = byId.get(cursor);
		if (!node) break;
		branch.push(node.id);
		seen.add(cursor);
		cursor = displayParentIdForNode(node, byId);
	}
	return branch.reverse();
}

interface CompactEntryMeta {
	turnId: number | null;
	turnIdForChildren: number | null;
	previousBoundaryId: string | null;
}

function historyBranchPointOptionsFromNodes(
	nodes: TranscriptTreeNode[],
	activeLeafId: string | null
): HistoryTargetOption[] {
	const byId = new Map(nodes.map((node) => [node.id, node]));
	const metaById = compactMetaById(nodes, byId);
	const options: HistoryTargetOption[] = [];
	for (const node of nodes) {
		const option = branchPointOptionForNode(byId, node, metaById.get(node.id), activeLeafId);
		if (option) options.push(option);
	}
	return options.reverse();
}

function compactMetaById(nodes: TranscriptTreeNode[], byId: Map<string, TranscriptTreeNode>): Map<string, CompactEntryMeta> {
	const metaById = new Map<string, CompactEntryMeta>();
	for (const node of nodes) {
		const parentId = displayParentIdForNode(node, byId);
		const parent = parentId ? byId.get(parentId) : null;
		const parentMeta = parentId ? metaById.get(parentId) : null;
		const previousBoundaryId = parent && isHistoryBoundaryNode(parent) ? parent.id : (parentMeta?.previousBoundaryId ?? null);
		const turnId = turnIdForNode(node, parentMeta?.turnIdForChildren ?? null);
		const turnIdForChildren = turnIdForChildNodes(node, turnId);
		metaById.set(node.id, { turnId, turnIdForChildren, previousBoundaryId });
	}
	return metaById;
}

function isHistoryBoundaryNode(node: TranscriptTreeNode): boolean {
	return node.item_type === "turn_finished" || node.item_type === "compaction_summary";
}

function turnIdForNode(node: TranscriptTreeNode, inheritedTurnId: number | null): number | null {
	if (node.item_type === "turn_finished") return node.turn_id ?? inheritedTurnId;
	if (node.item_type === "compaction_summary") return node.turn_id ?? inheritedTurnId;
	if (node.item_type === "turn_started") return node.turn_id ?? inheritedTurnId;
	return inheritedTurnId;
}

function turnIdForChildNodes(node: TranscriptTreeNode, currentTurnId: number | null): number | null {
	if (node.item_type === "turn_finished") return null;
	if (node.item_type === "compaction_summary") return node.turn_id ?? currentTurnId;
	if (node.item_type === "turn_started") return node.turn_id ?? currentTurnId;
	return currentTurnId;
}

function previousNodeBoundaryId(byId: Map<string, TranscriptTreeNode>, meta: CompactEntryMeta | undefined): string | null {
	const boundaryId = meta?.previousBoundaryId ?? null;
	if (!boundaryId || byId.has(boundaryId)) return boundaryId;
	return null;
}

function branchPointOptionForNode(
	byId: Map<string, TranscriptTreeNode>,
	node: TranscriptTreeNode,
	meta: CompactEntryMeta | undefined,
	activeLeafId: string | null
): HistoryTargetOption | null {
	const time = formatTimestamp(node.timestamp_ms);
	const currentTurnId = meta?.turnId ?? null;
	const preview = truncate((node.display_hint ?? "").trim() || node.item_type.replaceAll("_", " "), 96);
	if (node.item_type === "user_message") {
		return {
			id: node.id,
			actionLeafId: previousNodeBoundaryId(byId, meta),
			expectedActiveLeafId: activeLeafId,
			sourceEntryId: node.id,
			restoreEntryId: node.id,
			turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
			title: "User message",
			preview,
			meta: `edit · ${time}`,
			isActive: false
		};
	}
	if (node.item_type === "turn_finished") {
		return {
			id: node.id,
			actionLeafId: node.id,
			expectedActiveLeafId: activeLeafId,
			sourceEntryId: node.id,
			turnLabel: node.turn_id ? `t${node.turn_id}` : "turn",
			title: node.turn_id ? `End of turn ${node.turn_id}` : "End of turn",
			preview,
			meta: `switch · ${time}`,
			isActive: activeLeafId === node.id,
			outcome: node.outcome ?? undefined
		};
	}
	if (node.item_type !== "compaction_summary") return null;
	return {
		id: node.id,
		actionLeafId: node.id,
		expectedActiveLeafId: activeLeafId,
		sourceEntryId: node.id,
		turnLabel: node.turn_id ? `c${node.turn_id}` : "comp",
		title: "Compacted history",
		preview,
		meta: `switch · ${time}`,
		isActive: activeLeafId === node.id
	};
}

interface HistoryIndex {
	byId: Map<string, TranscriptEntry>;
	children: Map<string | null, TranscriptEntry[]>;
	metaById: Map<string, EntryMeta>;
	modelStepByEntryId: Map<string, ModelStepView>;
	turnByBoundaryEntryId: Map<string, TurnView>;
	branchFor: (leafId: string | null) => TranscriptEntry[];
}

interface EntryMeta {
	turnId: number | null;
	turnIdForChildren: number | null;
	previousBoundaryId: string | null;
}

const indexCache = new WeakMap<TranscriptEntry[], HistoryIndex>();

function historyBranchPointOptions(
	entries: TranscriptEntry[],
	activeLeafId: string | null
): HistoryTargetOption[] {
	const index = createHistoryIndex(entries);
	const options: HistoryTargetOption[] = [];
	for (const entry of entries) {
		const option = branchPointOptionForEntry(index, entry, index.metaById.get(entry.id), activeLeafId);
		if (option) options.push(option);
	}
	return options.reverse();
}

export function historyTreeRows(entries: TranscriptEntry[], activeLeafId: string | null): HistoryTreeRow[] {
	return measureHistoryDerivation("historyTreeRows", entries, () => historyTreeRowsIndexed(createHistoryIndex(entries), activeLeafId));
}

function historyTreeRowsIndexed(index: HistoryIndex, activeLeafId: string | null): HistoryTreeRow[] {
	const activePath = new Set(index.branchFor(activeLeafId).map((entry) => entry.id));
	const sizeCache = new Map<string, number>();
	const branchSize = (entryId: string): number => {
		const cached = sizeCache.get(entryId);
		if (cached !== undefined) return cached;
		const size = 1 + (index.children.get(entryId) ?? []).reduce((sum, child) => sum + branchSize(child.id), 0);
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
		const entryChildren = index.children.get(entry.id) ?? [];
		const hasSplit = entryChildren.length > 1;
		const activeChild = entryChildren.find((child) => activePath.has(child.id));
		for (const child of entryChildren) {
			const isAlternateBranch = hasSplit && child.id !== activeChild?.id;
			visit(child, depth + (isAlternateBranch ? 1 : 0), entry.id, hasSplit);
		}
	};
	for (const root of index.children.get(null) ?? []) visit(root, 0, null, false);
	return rows;
}

function historyEntryDisplayIndexed(index: HistoryIndex, entry: TranscriptEntry): HistoryEntryDisplay {
	const meta = index.metaById.get(entry.id);
	const currentTurnId = meta?.turnId ?? null;
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
	const option = displayOptionForEntry(index, entry, meta, null);
	if (option) {
		return {
			turnLabel: option.turnLabel,
			title: option.title,
			preview: option.preview,
			meta: option.meta
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
	return createHistoryIndex(entries).branchFor(leafId);
}

function createHistoryIndex(entries: TranscriptEntry[]): HistoryIndex {
	const cached = indexCache.get(entries);
	if (cached) return cached;
	const byId = new Map(entries.map((entry) => [entry.id, entry]));
	const children = new Map<string | null, TranscriptEntry[]>();
	const metaById = new Map<string, EntryMeta>();
	for (const entry of entries) {
		const parentId = displayParentIdForEntry(entry, byId);
		const siblings = children.get(parentId) ?? [];
		siblings.push(entry);
		children.set(parentId, siblings);
		const parent = parentId ? byId.get(parentId) : null;
		const parentMeta = parentId ? metaById.get(parentId) : null;
		const previousBoundaryId = parent && isHistoryBoundary(parent) ? parent.id : (parentMeta?.previousBoundaryId ?? null);
		const turnId = turnIdForEntry(entry, parentMeta?.turnIdForChildren ?? null);
		const turnIdForChildren = turnIdForChildEntries(entry, turnId);
		metaById.set(entry.id, { turnId, turnIdForChildren, previousBoundaryId });
	}
	const branchCache = new Map<string | null, TranscriptEntry[]>();
	const branchFor = (leafId: string | null): TranscriptEntry[] => {
		if (!leafId) return [];
		const cached = branchCache.get(leafId);
		if (cached) return cached;
		const branch: TranscriptEntry[] = [];
		const seen = new Set<string>();
		let cursor: string | null = leafId;
		while (cursor && !seen.has(cursor)) {
			const entry = byId.get(cursor);
			if (!entry) break;
			branch.push(entry);
			seen.add(cursor);
			cursor = displayParentIdForEntry(entry, byId);
		}
		branch.reverse();
		branchCache.set(leafId, branch);
		return branch;
	};
	const turnViews = buildTurnViews(entries);
	const modelStepByEntryId = new Map<string, ModelStepView>();
	const turnByBoundaryEntryId = new Map<string, TurnView>();
	for (const turn of turnViews) {
		if (turn.boundaryEntry) turnByBoundaryEntryId.set(turn.boundaryEntry.id, turn);
		for (const step of turn.modelSteps) modelStepByEntryId.set(step.entry.id, step);
	}
	const index = { byId, children, metaById, modelStepByEntryId, turnByBoundaryEntryId, branchFor };
	indexCache.set(entries, index);
	return index;
}

function isHistoryBoundary(entry: TranscriptEntry): boolean {
	return entry.item.type === "turn_finished" || entry.item.type === "compaction_summary";
}

function turnIdForEntry(entry: TranscriptEntry, inheritedTurnId: number | null): number | null {
	const item = entry.item;
	if (item.type === "turn_finished") return item.turn_id;
	if (item.type === "compaction_summary") return item.last_turn_id;
	if (item.type === "turn_started") return item.turn_id;
	return inheritedTurnId;
}

function turnIdForChildEntries(entry: TranscriptEntry, currentTurnId: number | null): number | null {
	const item = entry.item;
	if (item.type === "turn_finished") return null;
	if (item.type === "compaction_summary") return item.last_turn_id;
	if (item.type === "turn_started") return item.turn_id;
	return currentTurnId;
}

function previousTurnBoundaryId(index: HistoryIndex, meta: EntryMeta | undefined): string | null {
	const boundaryId = meta?.previousBoundaryId ?? null;
	if (!boundaryId || index.byId.has(boundaryId)) return boundaryId;
	return null;
}

function displayOptionForEntry(
	index: HistoryIndex,
	entry: TranscriptEntry,
	meta: EntryMeta | undefined,
	activeLeafId: string | null
): HistoryTargetOption | null {
	const item = entry.item;
	const time = formatTimestamp(entry.timestamp_ms);
	const isActive = activeLeafId === entry.id;
	const currentTurnId = meta?.turnId ?? null;
	if (item.type === "user_message") {
		const text = contentBlocksToText(item.content);
		return {
			id: entry.id,
			actionLeafId: previousTurnBoundaryId(index, meta),
			sourceEntryId: entry.id,
			restoreEntryId: entry.id,
			restoreText: text,
			turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
			title: currentTurnId ? `User message in turn ${currentTurnId}` : "User message",
			preview: truncate(text.trim() || "Empty user message.", 96),
			meta: time,
			isActive
		};
	}
	if (item.type === "assistant_message") {
		const step = index.modelStepByEntryId.get(entry.id) ?? null;
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			turnLabel: currentTurnId ? `a${currentTurnId}` : "asst",
			title: step ? modelStepTitle(step) : currentTurnId ? `Assistant message in turn ${currentTurnId}` : "Assistant message",
			preview: step ? modelStepPreview(step) : "Assistant message.",
			meta: step ? `${modelStepPhaseLabel(step.phase)} · ${time}` : time,
			isActive
		};
	}
	if (item.type === "tool_result") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
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
			turnLabel: `t${item.turn_id}`,
			title: `End of turn ${item.turn_id}`,
			preview: `${item.outcome.toLowerCase()} turn boundary.`,
			meta: time,
			isActive,
			outcome: item.outcome
		};
	}
	if (item.type === "compaction_summary") {
		return {
			id: entry.id,
			actionLeafId: entry.id,
			sourceEntryId: entry.id,
			turnLabel: `c${item.last_turn_id}`,
			title: "Compacted history",
			preview: truncate(item.summary, 96),
			meta: time,
			isActive
		};
	}
	return null;
}

function branchPointOptionForEntry(
	index: HistoryIndex,
	entry: TranscriptEntry,
	meta: EntryMeta | undefined,
	activeLeafId: string | null
): HistoryTargetOption | null {
	const item = entry.item;
	const time = formatTimestamp(entry.timestamp_ms);
	const currentTurnId = meta?.turnId ?? null;
	if (item.type === "user_message") {
		const text = contentBlocksToText(item.content);
		const actionLeafId = previousTurnBoundaryId(index, meta);
		return {
			id: entry.id,
			actionLeafId,
			expectedActiveLeafId: activeLeafId,
			sourceEntryId: entry.id,
			restoreEntryId: entry.id,
			restoreText: text,
			turnLabel: currentTurnId ? `u${currentTurnId}` : "user",
			title: "User message",
			preview: truncate(text.trim() || "Empty user message.", 96),
			meta: `edit · ${time}`,
			isActive: false
		};
	}
	if (item.type === "turn_finished") {
		const turn = index.turnByBoundaryEntryId.get(entry.id) ?? null;
		const step = turn ? terminalModelStep(turn) : null;
		return {
			id: entry.id,
			actionLeafId: entry.id,
			expectedActiveLeafId: activeLeafId,
			sourceEntryId: entry.id,
			turnLabel: `t${item.turn_id}`,
			title: step ? modelStepTitle(step) : `End of turn ${item.turn_id}`,
			preview: step ? modelStepPreview(step) : `${item.outcome.toLowerCase()} turn completed.`,
			meta: `switch · ${time}`,
			isActive: activeLeafId === entry.id,
			outcome: item.outcome
		};
	}
	if (item.type !== "compaction_summary") return null;
	const display = historyEntryDisplayIndexed(index, entry);
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

function measureHistoryDerivation<T>(label: string, entries: { id: string }[], derive: () => T): T {
	if (!perfEnabled()) return derive();
	const startedAt = perfNow();
	const result = derive();
	const count = Array.isArray(result) ? result.length : undefined;
	perfLog(label, {
		entries: entries.length,
		resultCount: count,
		deriveMs: Math.round(perfNow() - startedAt)
	});
	return result;
}

function formatTimestamp(timestampMs: number): string {
	return new Date(timestampMs).toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
}
