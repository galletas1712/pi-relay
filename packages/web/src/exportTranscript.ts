import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { TranscriptEntry, TranscriptItem } from "./types.ts";

export type ExportBlock =
	| {
			type: "user";
			entryId: string;
			text: string;
	  }
	| {
			type: "assistant";
			entryId: string;
			text: string;
			priorUserEntryId: string | null;
	  };

export type AssistantExportBlock = Extract<ExportBlock, { type: "assistant" }>;

export function assistantMessageText(item: Extract<TranscriptItem, { type: "assistant_message" }>): string {
	return item.items
		.map((assistantItem) => (assistantItem.type === "text" ? assistantItem.text : ""))
		.filter(Boolean)
		.join("\n\n")
		.trim();
}

export function buildExportBlocks(entries: TranscriptEntry[]): ExportBlock[] {
	const blocks: ExportBlock[] = [];
	let currentUserEntryId: string | null = null;

	for (const entry of entries) {
		if (entry.item.type === "user_message") {
			const text = contentBlocksToText(entry.item.content).trim();
			currentUserEntryId = entry.id;
			if (text) blocks.push({ type: "user", entryId: entry.id, text });
			continue;
		}

		if (entry.item.type === "assistant_message") {
			const text = assistantMessageText(entry.item);
			if (!text) continue;
			blocks.push({
				type: "assistant",
				entryId: entry.id,
				text,
				priorUserEntryId: currentUserEntryId
			});
		}
	}

	return blocks;
}

export function assistantExportBlocks(blocks: ExportBlock[]): AssistantExportBlock[] {
	return blocks.filter((block): block is AssistantExportBlock => block.type === "assistant");
}

export function formatExportMarkdown(blocks: ExportBlock[], selectedAssistantIds: Set<string>): string {
	const requiredUserIds = new Set<string>();
	for (const block of blocks) {
		if (block.type === "assistant" && selectedAssistantIds.has(block.entryId) && block.priorUserEntryId) {
			requiredUserIds.add(block.priorUserEntryId);
		}
	}

	const parts: string[] = [];
	for (const block of blocks) {
		if (block.type === "user") {
			if (requiredUserIds.has(block.entryId)) parts.push(`## User\n\n${block.text}`);
			continue;
		}
		if (selectedAssistantIds.has(block.entryId)) parts.push(`## Assistant\n\n${block.text}`);
	}

	return parts.join("\n\n").trim();
}

export function exportPreview(text: string): string {
	return truncate(firstLine(text) || text.trim() || "Empty message", 120);
}

export function downloadMarkdown(filename: string, markdown: string) {
	const blob = new Blob([markdown], { type: "text/markdown;charset=utf-8" });
	const url = URL.createObjectURL(blob);
	try {
		const link = document.createElement("a");
		link.href = url;
		link.download = filename;
		link.click();
	} finally {
		window.setTimeout(() => URL.revokeObjectURL(url), 0);
	}
}
