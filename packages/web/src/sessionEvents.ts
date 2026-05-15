import type { EventFrame, ProviderConfig, SessionSummary } from "./types.ts";

export type SessionPatchOperation =
	| {
			type: "metadata";
			sessionId: string;
			patch: Record<string, unknown>;
			remove: string[];
	  }
	| { type: "provider"; sessionId: string; provider: ProviderConfig }
	| {
			type: "activity";
			sessionId: string;
			activity: SessionSummary["activity"];
	  }
	| { type: "queued_inputs"; sessionId: string; event: EventFrame }
	| { type: "invalidate_session"; sessionId: string; reason: string }
	| { type: "invalidate_list"; reason: string };

export function reduceSessionEvent(event: EventFrame): SessionPatchOperation[] {
	return [
		...metadataOperations(event),
		...activityOperations(event),
		...queuedInputOperations(event),
		...sessionInvalidationOperations(event),
		...listInvalidationOperations(event),
	];
}

function metadataOperations(event: EventFrame): SessionPatchOperation[] {
	if (event.event !== "session.configured") return [];
	const operations: SessionPatchOperation[] = [];
	const metadata = recordValue(event.data.metadata) ?? recordValue(event.data.metadata_patch) ?? titlePatchFromEvent(event);
	const remove = stringArrayValue(event.data.metadata_remove);
	const provider = providerValue(event.data.provider);
	if (metadata || remove.length > 0) {
		operations.push({
			type: "metadata",
			sessionId: event.session_id,
			patch: metadata ?? {},
			remove,
		});
	}
	if (provider)
		operations.push({
			type: "provider",
			sessionId: event.session_id,
			provider,
		});
	return operations;
}

function activityOperations(event: EventFrame): SessionPatchOperation[] {
	const activity = activityForEvent(event.event);
	return activity ? [{ type: "activity", sessionId: event.session_id, activity }] : [];
}

function queuedInputOperations(event: EventFrame): SessionPatchOperation[] {
	return isQueuedInputPatchEvent(event.event) ? [{ type: "queued_inputs", sessionId: event.session_id, event }] : [];
}

function sessionInvalidationOperations(event: EventFrame): SessionPatchOperation[] {
	const reason = sessionInvalidationReason(event.event);
	return reason ? [{ type: "invalidate_session", sessionId: event.session_id, reason }] : [];
}

function listInvalidationOperations(event: EventFrame): SessionPatchOperation[] {
	return listInvalidationReason(event.event) ? [{ type: "invalidate_list", reason: event.event }] : [];
}

function activityForEvent(event: string): SessionSummary["activity"] | null {
	if (event === "session.idle") return "idle";
	if (event === "input.queued") return "queued";
	if (
		event === "input.consumed" ||
		event === "input.accepted" ||
		event === "action.requested" ||
		event === "model.requested" ||
		event === "tool.requested" ||
		event === "compaction.requested" ||
		event === "tool.started"
	) {
		return "running";
	}
	return null;
}

function sessionInvalidationReason(event: string): string | null {
	if (event === "session.configured" || event === "input.consumed" || event === "input.promoted") return null;
	if (event === "transcript.appended") return "transcript append event lacks full entry data";
	if (event === "turn.started" || event === "turn.finished" || event === "assistant.message") return event;
	if (event === "history.rewound" || event === "history.compacted") return event;
	return isTerminalActivityEvent(event) ? event : null;
}

function listInvalidationReason(event: string): string | null {
	if (
		event === "session.created" ||
		event === "session.configured" ||
		event === "history.forked" ||
		event === "history.rewound" ||
		event === "history.compacted" ||
		event === "input.queued" ||
		event === "input.consumed" ||
		event === "input.promoted" ||
		isTerminalActivityEvent(event)
	) {
		return event;
	}
	return null;
}

function isTerminalActivityEvent(event: string): boolean {
	return (
		event === "session.idle" ||
		event === "model.completed" ||
		event === "model.error" ||
		event === "tool.completed" ||
		event === "tool.error" ||
		event === "compaction.completed" ||
		event === "compaction.error"
	);
}

function isQueuedInputPatchEvent(event: string): boolean {
	return event === "input.queued" || event === "input.consumed" || event === "input.promoted";
}

function titlePatchFromEvent(event: EventFrame): Record<string, unknown> | undefined {
	return typeof event.data.title === "string" ? { title: event.data.title } : undefined;
}

function recordValue(value: unknown): Record<string, unknown> | undefined {
	return value && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : undefined;
}

function stringArrayValue(value: unknown): string[] {
	return Array.isArray(value) ? value.filter((key): key is string => typeof key === "string") : [];
}

function providerValue(value: unknown): ProviderConfig | undefined {
	if (!value || typeof value !== "object" || Array.isArray(value)) return undefined;
	const candidate = value as Partial<ProviderConfig>;
	return candidate.kind && candidate.model ? (candidate as ProviderConfig) : undefined;
}
