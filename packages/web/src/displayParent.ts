import type { TranscriptEntry, TranscriptTreeNode } from "./types.ts";

export function displayParentIdForEntry(
	entry: TranscriptEntry,
	byId?: Map<string, TranscriptEntry>,
): string | null {
	const parentId = entry.item.type === "compaction_summary" ? entry.item.source_leaf_id : entry.parent_id;
	if (!parentId) return null;
	if (byId && !byId.has(parentId)) return null;
	return parentId;
}

export function displayParentIdForNode(
	node: TranscriptTreeNode,
	byId?: Map<string, TranscriptTreeNode>,
): string | null {
	const parentId = node.item_type === "compaction_summary" ? (node.source_leaf_id ?? null) : node.parent_id;
	if (!parentId) return null;
	if (byId && !byId.has(parentId)) return null;
	return parentId;
}
