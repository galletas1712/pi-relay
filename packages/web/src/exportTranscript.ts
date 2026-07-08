import { firstLine, truncate } from "./text.ts";
import {
	assistantMessageText,
	buildTurnViews,
	modelStepPhaseLabel,
	userMessageExportText
} from "./turnView.ts";
import {
	activeBranchEntriesForExport,
	hasUsableSelectedSessionCache,
	turnCardsInOrder,
	turnDetailEntries,
	type SelectedSessionCache,
} from "./selectedSessionCache.ts";
import type { ModelStepPhase } from "./turnView.ts";
import type { TranscriptEntry } from "./types.ts";

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
			priorUserEntryIds: string[];
			phase: ModelStepPhase;
			turnLabel: string;
	  };

export type AssistantExportBlock = Extract<ExportBlock, { type: "assistant" }>;

export { assistantMessageText };

export function buildExportBlocks(entries: TranscriptEntry[]): ExportBlock[] {
	const blocks: ExportBlock[] = [];
	const turns = buildTurnViews(entries);

	for (const turn of turns) {
		const priorUserEntryIds = turn.userInputs.map((input) => input.id);
		for (const input of turn.userInputs) {
			const text = userMessageExportText(input);
			if (text) blocks.push({ type: "user", entryId: input.id, text });
		}

		for (const step of turn.modelSteps) {
			if (!step.text) continue;
			blocks.push({
				type: "assistant",
				entryId: step.entry.id,
				text: step.text,
				priorUserEntryIds,
				phase: step.phase,
				turnLabel: turn.turnId ? `turn ${turn.turnId}` : "session"
			});
		}
	}

	return blocks;
}

export function buildCachedExportBlocks(cache: SelectedSessionCache): ExportBlock[] {
	const entries = activeBranchEntriesForExport(cache);
	if (!hasUsableSelectedSessionCache(cache)) return buildExportBlocks(entries);

	const selectedIds = new Set(entries.map((entry) => entry.id));
	const blocks: ExportBlock[] = [];
	const representedIds = new Set<string>();
	const add = (block: ExportBlock) => {
		if (!selectedIds.has(block.entryId) || representedIds.has(block.entryId)) return;
		representedIds.add(block.entryId);
		blocks.push(block);
	};

	for (const card of turnCardsInOrder(cache)) {
		const detail = turnDetailEntries(cache, card.id);
		if (detail) {
			for (const block of buildExportBlocks(detail)) add(block);
			continue;
		}
		const summaryBlocks = buildExportBlocks([
			...card.user_messages,
			...(card.assistant_message ? [card.assistant_message] : []),
		]);
		for (const block of summaryBlocks) {
			add(block.type === "assistant"
				? {
						...block,
						phase: card.status === "open"
							? "running"
							: card.outcome === "Graceful"
								? "final_answer"
								: card.outcome === "Interrupted" || card.outcome === "Crashed"
									? "aborted"
									: "unknown",
						turnLabel: card.turn_id ? `turn ${card.turn_id}` : "session",
					}
				: block);
		}
	}

	const remainingEntries = entries.filter((entry) => !representedIds.has(entry.id));
	for (const block of buildExportBlocks(remainingEntries)) add(block);
	const orderById = new Map(entries.map((entry, index) => [entry.id, index]));
	return blocks.sort(
		(left, right) =>
			(orderById.get(left.entryId) ?? Number.MAX_SAFE_INTEGER) -
			(orderById.get(right.entryId) ?? Number.MAX_SAFE_INTEGER),
	);
}

export function assistantExportBlocks(blocks: ExportBlock[]): AssistantExportBlock[] {
	return blocks.filter((block): block is AssistantExportBlock => block.type === "assistant");
}

export function defaultSelectedAssistantIds(blocks: ExportBlock[]): Set<string> {
	const assistants = assistantExportBlocks(blocks);
	const finalAnswers = assistants.filter((block) => block.phase === "final_answer");
	return new Set((finalAnswers.length > 0 ? finalAnswers : assistants).map((assistant) => assistant.entryId));
}

export function formatExportMarkdown(blocks: ExportBlock[], selectedAssistantIds: Set<string>): string {
	const requiredUserIds = new Set<string>();
	for (const block of blocks) {
		if (block.type === "assistant" && selectedAssistantIds.has(block.entryId)) {
			for (const userEntryId of block.priorUserEntryIds) requiredUserIds.add(userEntryId);
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

export function exportTitle(block: AssistantExportBlock, index: number): string {
	return `${modelStepPhaseLabel(block.phase)} ${index + 1} · ${block.turnLabel}`;
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
