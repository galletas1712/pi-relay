import { contentBlocksToText, firstLine, truncate } from "./text.ts";
import type { AssistantItem, ToolCall, TranscriptEntry, TranscriptItem, TurnOutcome } from "./types.ts";

export type AssistantTranscriptEntry = TranscriptEntry & {
	item: Extract<TranscriptItem, { type: "assistant_message" }>;
};

export type UserTranscriptEntry = TranscriptEntry & {
	item: Extract<TranscriptItem, { type: "user_message" }>;
};

export type ToolResultTranscriptEntry = TranscriptEntry & {
	item: Extract<TranscriptItem, { type: "tool_result" }>;
};

export type TurnFinishedTranscriptEntry = TranscriptEntry & {
	item: Extract<TranscriptItem, { type: "turn_finished" }>;
};

export type ModelStepPhase = "tool_request" | "final_answer" | "running" | "aborted" | "unknown";

export interface ModelStepView {
	entry: AssistantTranscriptEntry;
	turnId: number | null;
	phase: ModelStepPhase;
	text: string;
	toolCalls: ToolCall[];
	toolResults: ToolResultTranscriptEntry[];
}

export interface TurnView {
	turnId: number | null;
	startEntry: TranscriptEntry | null;
	boundaryEntry: TurnFinishedTranscriptEntry | null;
	outcome: TurnOutcome | null;
	entries: TranscriptEntry[];
	userInputs: UserTranscriptEntry[];
	modelSteps: ModelStepView[];
	toolResults: ToolResultTranscriptEntry[];
}

type MutableModelStep = Omit<ModelStepView, "phase" | "toolResults">;

interface MutableTurn {
	turnId: number | null;
	startEntry: TranscriptEntry | null;
	boundaryEntry: TurnFinishedTranscriptEntry | null;
	outcome: TurnOutcome | null;
	entries: TranscriptEntry[];
	userInputs: UserTranscriptEntry[];
	modelSteps: MutableModelStep[];
	toolResults: ToolResultTranscriptEntry[];
}

export function assistantMessageText(item: Extract<TranscriptItem, { type: "assistant_message" }>): string {
	return item.items
		.map((assistantItem) => (assistantItem.type === "text" ? assistantItem.text : ""))
		.filter(Boolean)
		.join("")
		.trim();
}

export function assistantToolCalls(items: AssistantItem[]): ToolCall[] {
	return items.filter((item): item is Extract<AssistantItem, { type: "tool_call" }> => item.type === "tool_call");
}

export function buildTurnViews(entries: TranscriptEntry[]): TurnView[] {
	const turns: MutableTurn[] = [];
	let current: MutableTurn | null = null;

	const ensureTurn = (turnId: number | null = current?.turnId ?? null): MutableTurn => {
		if (!current) current = emptyTurn(turnId, null);
		if (current.turnId == null && turnId != null) current.turnId = turnId;
		return current;
	};

	const closeCurrent = () => {
		if (current && current.entries.length > 0) turns.push(current);
		current = null;
	};

	for (const entry of entries) {
		const item = entry.item;
		if (item.type === "turn_started") {
			closeCurrent();
			current = emptyTurn(item.turn_id, entry);
			current.entries.push(entry);
			continue;
		}
		if (item.type === "compaction_summary") {
			closeCurrent();
			continue;
		}

		const turn = ensureTurn(turnIdForItem(item));
		turn.entries.push(entry);

		if (item.type === "user_message") {
			turn.userInputs.push(entry as UserTranscriptEntry);
			continue;
		}
		if (item.type === "assistant_message") {
			turn.modelSteps.push({
				entry: entry as AssistantTranscriptEntry,
				turnId: turn.turnId,
				text: assistantMessageText(item),
				toolCalls: assistantToolCalls(item.items)
			});
			continue;
		}
		if (item.type === "tool_result") {
			turn.toolResults.push(entry as ToolResultTranscriptEntry);
			continue;
		}
		if (item.type === "turn_finished") {
			turn.turnId = item.turn_id;
			turn.boundaryEntry = entry as TurnFinishedTranscriptEntry;
			turn.outcome = item.outcome;
			closeCurrent();
		}
	}
	closeCurrent();

	return turns.map(finalizeTurn);
}

export function modelStepPhaseLabel(phase: ModelStepPhase): string {
	switch (phase) {
		case "tool_request":
			return "progress";
		case "final_answer":
			return "final answer";
		case "running":
			return "running";
		case "aborted":
			return "aborted";
		case "unknown":
			return "model step";
	}
}

export function modelStepPreview(step: Pick<ModelStepView, "text" | "toolCalls">): string {
	if (step.text.trim()) return truncate(firstLine(step.text) || step.text.trim(), 96);
	if (step.toolCalls.length > 0) return `Tool call: ${step.toolCalls.map((call) => call.tool_name).join(", ")}`;
	return "Assistant model step.";
}

export function modelStepTitle(step: Pick<ModelStepView, "phase" | "turnId">): string {
	const suffix = step.turnId ? ` in turn ${step.turnId}` : "";
	switch (step.phase) {
		case "tool_request":
			return `Progress step${suffix}`;
		case "final_answer":
			return `Final answer${suffix}`;
		case "running":
			return `Running model step${suffix}`;
		case "aborted":
			return `Aborted model step${suffix}`;
		case "unknown":
			return `Model step${suffix}`;
	}
}

export function terminalModelStep(turn: TurnView): ModelStepView | null {
	for (let index = turn.modelSteps.length - 1; index >= 0; index -= 1) {
		const step = turn.modelSteps[index];
		if (step.phase === "final_answer") return step;
	}
	return turn.modelSteps.at(-1) ?? null;
}

export function turnForBoundaryEntry(turns: TurnView[], entryId: string): TurnView | null {
	return turns.find((turn) => turn.boundaryEntry?.id === entryId) ?? null;
}

export function modelStepForEntry(turns: TurnView[], entryId: string): ModelStepView | null {
	for (const turn of turns) {
		const match = turn.modelSteps.find((step) => step.entry.id === entryId);
		if (match) return match;
	}
	return null;
}

export function userMessageExportText(entry: UserTranscriptEntry): string {
	return contentBlocksToText(entry.item.content).trim();
}

function finalizeTurn(turn: MutableTurn): TurnView {
	const resultsById = new Map<string, ToolResultTranscriptEntry[]>();
	for (const result of turn.toolResults) {
		const list = resultsById.get(result.item.tool_call_id) ?? [];
		list.push(result);
		resultsById.set(result.item.tool_call_id, list);
	}
	const terminalAssistantId = terminalAssistantEntryId(turn);
	const modelSteps = turn.modelSteps.map((step) => {
		const toolResults = step.toolCalls.flatMap((call) => resultsById.get(call.id) ?? []);
		return {
			...step,
			toolResults,
			phase: deriveModelStepPhase(step, turn, terminalAssistantId)
		};
	});
	return {
		...turn,
		modelSteps
	};
}

function deriveModelStepPhase(step: MutableModelStep, turn: MutableTurn, terminalAssistantId: string | null): ModelStepPhase {
	if (step.toolCalls.length > 0) return "tool_request";
	if (!turn.boundaryEntry) {
		return step.entry.id === turn.modelSteps.at(-1)?.entry.id ? "running" : "unknown";
	}
	if (turn.outcome === "Graceful" && step.entry.id === terminalAssistantId) return "final_answer";
	if ((turn.outcome === "Interrupted" || turn.outcome === "Crashed") && step.entry.id === terminalAssistantId) return "aborted";
	return "unknown";
}

function terminalAssistantEntryId(turn: MutableTurn): string | null {
	for (let index = turn.modelSteps.length - 1; index >= 0; index -= 1) {
		const step = turn.modelSteps[index];
		if (step.text || step.toolCalls.length > 0) return step.entry.id;
	}
	return null;
}

function emptyTurn(turnId: number | null, startEntry: TranscriptEntry | null): MutableTurn {
	return {
		turnId,
		startEntry,
		boundaryEntry: null,
		outcome: null,
		entries: [],
		userInputs: [],
		modelSteps: [],
		toolResults: []
	};
}

function turnIdForItem(item: TranscriptItem): number | null {
	if (item.type === "turn_started" || item.type === "turn_finished" || item.type === "tool_call_started") return item.turn_id;
	if (item.type === "compaction_summary") return item.last_turn_id;
	return null;
}
