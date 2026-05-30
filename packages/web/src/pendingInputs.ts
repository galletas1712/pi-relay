import type { ContentBlock, EventFrame, InputPriority, QueuedInput, SessionSnapshot } from "./types.ts";

export type PendingInputPlacement = "transcript" | "queue";
export type PendingInputStatus = "sending" | "accepted" | "queued" | "failed";

export interface PendingInput {
	id: string;
	sessionId: string;
	clientInputId: string;
	inputId?: string | null;
	content: ContentBlock[];
	placement: PendingInputPlacement;
	priority: InputPriority;
	status: PendingInputStatus;
	submittedAt: number;
	error?: string;
}

export interface PendingTranscriptInput {
	id: string;
	content: ContentBlock[];
	status: PendingInputStatus;
	error?: string;
}

export type QueuePaneInput = QueuedInput & {
	pending?: boolean;
	error?: string;
};

export function queuedInputFromPending(input: PendingInput): QueuePaneInput {
	return {
		input_id: input.inputId ?? input.clientInputId,
		priority: input.priority,
		status: input.status === "failed" ? "cancelled" : "queued",
		content: input.content,
		client_input_id: input.clientInputId,
		created_at: new Date(input.submittedAt).toISOString(),
		promoted_at: input.priority === "steer" ? new Date(input.submittedAt).toISOString() : null,
		pending: true,
		error: input.error,
	};
}

export function pendingInputIsReflected(input: PendingInput, snapshot: SessionSnapshot): boolean {
	if (snapshot.queued_inputs.some((queued) => queuedInputMatchesPending(queued, input))) return true;
	const fingerprint = contentFingerprint(input.content);
	return (snapshot.entries ?? []).some(
		(entry) =>
			entry.item.type === "user_message" &&
			entry.timestamp_ms >= input.submittedAt - 10_000 &&
			contentFingerprint(entry.item.content) === fingerprint,
	);
}

export function pendingInputMatchesEvent(input: PendingInput, event: EventFrame): boolean {
	const inputId = stringValue(event.data.input_id);
	const clientInputId = stringValue(event.data.client_input_id);
	return (!!inputId && input.inputId === inputId) || (!!clientInputId && input.clientInputId === clientInputId);
}

export function queuedInputMatchesPending(queued: QueuedInput, input: PendingInput): boolean {
	return (!!input.inputId && queued.input_id === input.inputId) || queued.client_input_id === input.clientInputId;
}

export function eventInputId(event: EventFrame): string | null {
	return stringValue(event.data.input_id);
}

export function eventClientInputId(event: EventFrame): string | null {
	return stringValue(event.data.client_input_id);
}

export function eventContentBlocks(event: EventFrame): ContentBlock[] | null {
	return contentBlocksFromUnknown(event.data.content);
}

function contentFingerprint(content: ContentBlock[]): string {
	return JSON.stringify(content);
}

function contentBlocksFromUnknown(value: unknown): ContentBlock[] | null {
	if (Array.isArray(value)) return value.filter(isContentBlock);
	if (value && typeof value === "object" && !Array.isArray(value)) {
		const content = (value as { content?: unknown }).content;
		if (Array.isArray(content)) return content.filter(isContentBlock);
	}
	return null;
}

function isContentBlock(value: unknown): value is ContentBlock {
	if (!value || typeof value !== "object" || Array.isArray(value)) return false;
	const block = value as Partial<ContentBlock>;
	if (block.type === "text") return typeof (block as { text?: unknown }).text === "string";
	if (block.type !== "image") return false;
	const image = (block as { image?: unknown }).image;
	return !!image && typeof image === "object" && !Array.isArray(image);
}

function stringValue(value: unknown): string | null {
	return typeof value === "string" && value.length > 0 ? value : null;
}
