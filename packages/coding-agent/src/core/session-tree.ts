import type { AgentMessage } from "@pi-relay/agent-core";
import type { CompactionEntry, SessionContext, SessionEntry, SessionTreeNode } from "./session-manager.js";
import {
	createBranchSummaryMessage,
	createCompactionSummaryMessage,
	createCustomMessage,
} from "./messages.js";

type SessionInfoLikeEntry = {
	type: string;
	name?: string;
};

function createSessionEntryIndex(entries: readonly SessionEntry[]): Map<string, SessionEntry> {
	const byId = new Map<string, SessionEntry>();
	for (const entry of entries) {
		byId.set(entry.id, entry);
	}
	return byId;
}

export function getSessionChildren(byId: ReadonlyMap<string, SessionEntry>, parentId: string): SessionEntry[] {
	const children: SessionEntry[] = [];
	for (const entry of byId.values()) {
		if (entry.parentId === parentId) {
			children.push(entry);
		}
	}
	return children;
}

export function getSessionBranch(
	byId: ReadonlyMap<string, SessionEntry>,
	leafId?: string | null,
): SessionEntry[] {
	if (leafId === null) {
		return [];
	}

	const path: SessionEntry[] = [];
	let current = leafId ? byId.get(leafId) : undefined;
	while (current) {
		path.unshift(current);
		current = current.parentId ? byId.get(current.parentId) : undefined;
	}
	return path;
}

export function getLatestSessionName(entries: Iterable<SessionInfoLikeEntry>): string | undefined {
	let name: string | undefined;
	for (const entry of entries) {
		if (entry.type === "session_info") {
			name = entry.name?.trim() || undefined;
		}
	}
	return name;
}

function appendContextMessage(messages: AgentMessage[], entry: SessionEntry): void {
	if (entry.type === "message") {
		messages.push(entry.message);
	} else if (entry.type === "custom_message") {
		messages.push(
			createCustomMessage(entry.customType, entry.content, entry.display, entry.details, entry.timestamp),
		);
	} else if (entry.type === "branch_summary" && entry.summary) {
		messages.push(createBranchSummaryMessage(entry.summary, entry.fromId, entry.timestamp));
	}
}

/**
 * Build the session context from entries using tree traversal.
 * If leafId is provided, walks from that entry to root.
 * Handles compaction and branch summaries along the path.
 */
export function buildSessionContext(
	entries: readonly SessionEntry[],
	leafId?: string | null,
	byId?: ReadonlyMap<string, SessionEntry>,
): SessionContext {
	const entryIndex = byId ?? createSessionEntryIndex(entries);

	if (leafId === null) {
		return { messages: [], thinkingLevel: "off", model: null };
	}

	let leaf: SessionEntry | undefined;
	if (leafId) {
		leaf = entryIndex.get(leafId);
	}
	if (!leaf) {
		leaf = entries[entries.length - 1];
	}

	if (!leaf) {
		return { messages: [], thinkingLevel: "off", model: null };
	}

	const path = getSessionBranch(entryIndex, leaf.id);

	let thinkingLevel = "off";
	let model: { provider: string; modelId: string } | null = null;
	let compaction: CompactionEntry | null = null;

	for (const entry of path) {
		if (entry.type === "thinking_level_change") {
			thinkingLevel = entry.thinkingLevel;
		} else if (entry.type === "model_change") {
			model = { provider: entry.provider, modelId: entry.modelId };
		} else if (entry.type === "message" && entry.message.role === "assistant") {
			model = { provider: entry.message.provider, modelId: entry.message.model };
		} else if (entry.type === "compaction") {
			compaction = entry;
		}
	}

	const messages: AgentMessage[] = [];

	if (compaction) {
		messages.push(createCompactionSummaryMessage(compaction.summary, compaction.tokensBefore, compaction.timestamp));

		const compactionIdx = path.findIndex((entry) => entry.type === "compaction" && entry.id === compaction.id);
		if (compactionIdx === -1) {
			return { messages, thinkingLevel, model };
		}

		let foundFirstKept = false;
		for (let i = 0; i < compactionIdx; i++) {
			const entry = path[i];
			if (entry.id === compaction.firstKeptEntryId) {
				foundFirstKept = true;
			}
			if (foundFirstKept) {
				appendContextMessage(messages, entry);
			}
		}

		for (let i = compactionIdx + 1; i < path.length; i++) {
			appendContextMessage(messages, path[i]);
		}
	} else {
		for (const entry of path) {
			appendContextMessage(messages, entry);
		}
	}

	return { messages, thinkingLevel, model };
}

export function buildSessionTree(
	entries: readonly SessionEntry[],
	labelsById: ReadonlyMap<string, string>,
	labelTimestampsById: ReadonlyMap<string, string>,
): SessionTreeNode[] {
	const nodeMap = new Map<string, SessionTreeNode>();
	const roots: SessionTreeNode[] = [];

	for (const entry of entries) {
		nodeMap.set(entry.id, {
			entry,
			children: [],
			label: labelsById.get(entry.id),
			labelTimestamp: labelTimestampsById.get(entry.id),
		});
	}

	for (const entry of entries) {
		const node = nodeMap.get(entry.id)!;
		if (entry.parentId === null || entry.parentId === entry.id) {
			roots.push(node);
			continue;
		}

		const parent = nodeMap.get(entry.parentId);
		if (parent) {
			parent.children.push(node);
		} else {
			roots.push(node);
		}
	}

	const stack = [...roots];
	while (stack.length > 0) {
		const node = stack.pop()!;
		node.children.sort((a, b) => new Date(a.entry.timestamp).getTime() - new Date(b.entry.timestamp).getTime());
		stack.push(...node.children);
	}

	return roots;
}
