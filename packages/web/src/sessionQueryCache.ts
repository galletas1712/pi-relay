import type { QueryClient } from "@tanstack/react-query";
import { queryKeys } from "./queryKeys.ts";
import type { EventFrame, SessionSnapshot, SessionSummary, TranscriptEntry } from "./types.ts";

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

export function mergeMetadata(
	metadata: Record<string, unknown>,
	patch: Record<string, unknown>,
	remove: string[] = [],
): Record<string, unknown> {
	const next = { ...metadata, ...patch };
	for (const key of remove) delete next[key];
	return next;
}

export function patchSessionListMetadata(
	queryClient: QueryClient,
	projectId: string | null,
	sessionId: string,
	patch: Record<string, unknown>,
	remove: string[] = [],
	replace = false,
) {
	patchSessionList(queryClient, projectId, sessionId, (session) => ({
		...session,
		metadata: replace ? mergeMetadata(patch, {}, remove) : mergeMetadata(session.metadata, patch, remove),
	}));
}

export function patchSessionListProvider(queryClient: QueryClient, projectId: string | null, sessionId: string, provider: SessionSummary["provider"]) {
	patchSessionList(queryClient, projectId, sessionId, (session) => ({
		...session,
		provider,
	}));
}

export function patchSessionListActivity(
	queryClient: QueryClient,
	projectId: string | null,
	sessionId: string,
	activity: SessionSummary["activity"],
) {
	patchSessionList(queryClient, projectId, sessionId, (session) => ({
		...session,
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

export type SessionViewUpdateResult = "applied" | "ignored" | "reload_overview" | "reload_active_branch";

export function applyServerViewUpdate(snapshot: SessionSnapshot | undefined, event: EventFrame): SessionViewUpdateResult {
	if (!snapshot || snapshot.session_id !== event.session_id) return "reload_active_branch";
	if (event.event_id <= snapshot.last_event_id) return "ignored";
	const update = event.view_update;
	if (!update) return applyPersistedEventFallback(snapshot, event);
	if (update.active_branch?.kind === "reload_required") return "reload_active_branch";
	let next = snapshot;
	let changed = false;
	if (update.active_branch?.kind === "append_entry") {
		const entry = update.active_branch.entry;
		if (next.entries?.some((candidate) => candidate.id === entry.id)) {
			next = { ...next, last_event_id: event.event_id };
			changed = true;
		} else if (entry.parent_id !== next.active_leaf_id) {
			return "reload_active_branch";
		} else {
			next = {
				...next,
				entries: [...(next.entries ?? []), entry],
				active_leaf_id: entry.id,
				has_transcript_entries: true,
				last_event_id: event.event_id,
			};
			changed = true;
		}
	}
	if (update.overview) {
		next = applyOverviewPatch(next, update.overview);
		changed = true;
	}
	if (!changed) return fallbackRefreshForEvent(event);
	if (next.last_event_id < event.event_id) next = { ...next, last_event_id: event.event_id };
	Object.assign(snapshot, next);
	return "applied";
}

function applyPersistedEventFallback(snapshot: SessionSnapshot, event: EventFrame): SessionViewUpdateResult {
	if (event.event !== "transcript.appended") return fallbackRefreshForEvent(event);
	const entry = transcriptEntryFromEvent(event);
	if (!entry) return "reload_active_branch";
	if (snapshot.entries?.some((candidate) => candidate.id === entry.id)) {
		Object.assign(snapshot, { ...snapshot, last_event_id: event.event_id });
		return "applied";
	}
	if (entry.parent_id !== snapshot.active_leaf_id) return "reload_active_branch";
	Object.assign(snapshot, {
		...snapshot,
		entries: [...(snapshot.entries ?? []), entry],
		active_leaf_id: entry.id,
		has_transcript_entries: true,
		last_event_id: event.event_id,
	});
	return "applied";
}

function applyOverviewPatch(snapshot: SessionSnapshot, overview: Partial<Omit<SessionSnapshot, "entries">>): SessionSnapshot {
	return {
		...snapshot,
		...overview,
		entries: snapshot.entries,
		pending_actions: overview.pending_actions ?? snapshot.pending_actions,
		queued_inputs: overview.queued_inputs ?? snapshot.queued_inputs,
		metadata: overview.metadata ?? snapshot.metadata,
		provider: overview.provider ?? snapshot.provider,
	};
}

function fallbackRefreshForEvent(_event: Pick<EventFrame, "event" | "data">): SessionViewUpdateResult {
	return "reload_overview";
}

export function transcriptEntryFromEvent(event: Pick<EventFrame, "data">): TranscriptEntry | null {
	const entry = event.data.entry;
	if (!entry || typeof entry !== "object" || Array.isArray(entry)) return null as never;
	return entry as TranscriptEntry;
}
