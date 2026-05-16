import type { QueryClient } from "@tanstack/react-query";
import { queryKeys } from "./queryKeys.ts";
import type { SessionSnapshot, SessionSummary } from "./types.ts";

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
