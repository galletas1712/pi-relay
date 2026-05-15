import type { QueryClient } from "@tanstack/react-query";
import { queryKeys } from "./queryKeys.ts";
import { queuedInputFromEvent } from "./pendingInputs.ts";
import type { EventFrame, ProviderConfig, QueuedInput, SessionSnapshot, SessionSummary } from "./types.ts";

export function patchSessionList(
	queryClient: QueryClient,
	projectId: string | null,
	sessionId: string,
	patcher: (session: SessionSummary) => SessionSummary,
) {
	queryClient.setQueryData<SessionSummary[]>(queryKeys.sessions(projectId), (current) => {
		if (!current) return current;
		let changed = false;
		const next = current.map((session) => {
			if (session.session_id !== sessionId) return session;
			changed = true;
			return patcher(session);
		});
		return changed ? next : current;
	});
}

export function patchSessionSnapshot(queryClient: QueryClient, sessionId: string, patcher: (snapshot: SessionSnapshot) => SessionSnapshot) {
	queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, "full_tree"), (current) => (current ? patcher(current) : current));
}

export function mergeMetadata(
	metadata: Record<string, unknown>,
	patch: Record<string, unknown>,
	remove: string[] = [],
): Record<string, unknown> {
	const next = { ...metadata, ...patch };
	for (const key of remove) delete next[key];
	return next;
}

export function patchSessionMetadataEverywhere(
	queryClient: QueryClient,
	projectId: string | null,
	sessionId: string,
	patch: Record<string, unknown>,
	remove: string[] = [],
) {
	patchSessionList(queryClient, projectId, sessionId, (session) => ({
		...session,
		metadata: mergeMetadata(session.metadata, patch, remove),
	}));
	patchSessionSnapshot(queryClient, sessionId, (snapshot) => ({
		...snapshot,
		metadata: mergeMetadata(snapshot.metadata, patch, remove),
	}));
}

export function patchSessionProviderEverywhere(
	queryClient: QueryClient,
	projectId: string | null,
	sessionId: string,
	provider: ProviderConfig,
) {
	patchSessionList(queryClient, projectId, sessionId, (session) => ({
		...session,
		provider,
	}));
	patchSessionSnapshot(queryClient, sessionId, (snapshot) => ({
		...snapshot,
		provider,
	}));
}

export function patchSessionActivityEverywhere(
	queryClient: QueryClient,
	projectId: string | null,
	sessionId: string,
	activity: SessionSummary["activity"],
) {
	patchSessionList(queryClient, projectId, sessionId, (session) => ({
		...session,
		activity,
	}));
	patchSessionSnapshot(queryClient, sessionId, (snapshot) => ({
		...snapshot,
		activity,
	}));
}

export function mergeSnapshotIntoSessionList(
	sessions: SessionSummary[] | undefined,
	snapshot: SessionSnapshot,
): SessionSummary[] | undefined {
	if (!sessions) return sessions;
	let found = false;
	const nextSessions = sessions.map((session) => {
		if (session.session_id !== snapshot.session_id) return session;
		found = true;
		return {
			...session,
			project_id: snapshot.project_id,
			starting_cwd: snapshot.starting_cwd,
			activity: snapshot.activity,
			active_leaf_id: snapshot.active_leaf_id,
			provider: snapshot.provider,
			metadata: snapshot.metadata,
		};
	});
	return found ? nextSessions : sessions;
}

export function patchQueuedInputsInSnapshot(queryClient: QueryClient, event: EventFrame) {
	patchSessionSnapshot(queryClient, event.session_id, (snapshot) => applyQueuedInputEventToSnapshot(event, snapshot));
}

export function applyQueuedInputEventToSnapshot<T extends Pick<SessionSnapshot, "session_id" | "queued_inputs">>(
	event: EventFrame,
	current: T,
): T {
	if (current.session_id !== event.session_id) return current;
	const inputId = typeof event.data.input_id === "string" ? event.data.input_id : null;
	if (!inputId) return current;
	if (event.event === "input.queued") {
		if (current.queued_inputs.some((input) => input.input_id === inputId)) return current;
		const queuedInput = queuedInputFromEvent(event);
		return queuedInput ? { ...current, queued_inputs: [...current.queued_inputs, queuedInput] } : current;
	}
	if (event.event === "input.consumed") {
		const queuedInputs = current.queued_inputs.filter((input) => input.input_id !== inputId);
		return queuedInputs.length === current.queued_inputs.length ? current : { ...current, queued_inputs: queuedInputs };
	}
	if (event.event !== "input.promoted") return current;
	const promotedAt = typeof event.data.promoted_at === "string" ? event.data.promoted_at : null;
	let changed = false;
	const queuedInputs: QueuedInput[] = current.queued_inputs.map((input) => {
		if (input.input_id !== inputId) return input;
		changed = true;
		return {
			...input,
			priority: "steer" as const,
			status: "queued" as const,
			promoted_at: promotedAt,
		};
	});
	return changed ? { ...current, queued_inputs: queuedInputs } : current;
}
