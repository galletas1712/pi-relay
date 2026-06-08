import type { QueryClient } from "@tanstack/react-query";
import { queryKeys, type EntryScope } from "./queryKeys.ts";
import { displayParentIdForEntry } from "./displayParent.ts";
import { sortSessionsByLastUserMessage } from "./sessionList.ts";
import type { ActiveBranchSyncResponse, EventFrame, SessionSnapshot, SessionSummary } from "./types.ts";

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
		return changed ? sortSessionsByLastUserMessage(next) : current;
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

export function patchSessionListEventSummary(
	queryClient: QueryClient,
	projectId: string | null,
	event: EventFrame,
	activity: SessionSummary["activity"] | null,
) {
	const metadata = recordValue(event.data.metadata);
	const provider = providerValue(event.data.provider);
	const activeLeafId = activeLeafIdValue(event.data);
	const lastUserMessageTimestampMs = lastUserMessageTimestampFromEvent(event);
	if (!metadata && !provider && !activity && activeLeafId === undefined && lastUserMessageTimestampMs === undefined) return;

	patchSessionList(queryClient, projectId, event.session_id, (session) => ({
		...session,
		metadata: metadata ?? session.metadata,
		provider: provider ?? session.provider,
		activity: activity ?? session.activity,
		active_leaf_id: activeLeafId === undefined ? session.active_leaf_id : activeLeafId,
		last_user_message_timestamp_ms:
			lastUserMessageTimestampMs === undefined
				? session.last_user_message_timestamp_ms
				: Math.max(session.last_user_message_timestamp_ms ?? Number.NEGATIVE_INFINITY, lastUserMessageTimestampMs),
		has_transcript_entries:
			activeLeafId === undefined ? session.has_transcript_entries : activeLeafId !== null || session.has_transcript_entries,
	}));
}

export function patchSessionSnapshot(
	queryClient: QueryClient,
	sessionId: string,
	scope: EntryScope,
	patcher: (snapshot: SessionSnapshot) => SessionSnapshot,
) {
	queryClient.setQueryData<SessionSnapshot>(queryKeys.session(sessionId, scope), (current) => {
		if (!current || current.session_id !== sessionId) return current;
		return patcher(current);
	});
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
			outer_cwd: snapshot.outer_cwd,
			workspaces: snapshot.workspaces,
			activity: snapshot.activity,
			active_leaf_id: snapshot.active_leaf_id,
			provider: snapshot.provider,
			metadata: snapshot.metadata,
			last_user_message_timestamp_ms:
				snapshot.last_user_message_timestamp_ms === undefined
					? session.last_user_message_timestamp_ms
					: snapshot.last_user_message_timestamp_ms,
		};
	});
	return found ? sortSessionsByLastUserMessage(nextSessions) : sessions;
}

function recordValue(value: unknown): Record<string, unknown> | null {
	return value !== null && typeof value === "object" && !Array.isArray(value) ? (value as Record<string, unknown>) : null;
}

function providerValue(value: unknown): SessionSummary["provider"] | null {
	const candidate = recordValue(value);
	if (!candidate) return null;
	if ((candidate.kind !== "openai" && candidate.kind !== "claude") || typeof candidate.model !== "string") return null;
	return {
		...candidate,
		kind: candidate.kind,
		model: candidate.model,
	} as SessionSummary["provider"];
}

function activeLeafIdValue(data: Record<string, unknown>): string | null | undefined {
	if (!Object.prototype.hasOwnProperty.call(data, "active_leaf_id")) return undefined;
	const value = data.active_leaf_id;
	if (value === null || typeof value === "string") return value;
	return undefined;
}

function lastUserMessageTimestampFromEvent(event: EventFrame): number | undefined {
	if (event.event !== "transcript.appended") return undefined;
	const entry = recordValue(event.data.entry);
	const item = entry ? recordValue(entry.item) : null;
	const timestamp = entry?.timestamp_ms;
	if (item?.type !== "user_message" || typeof timestamp !== "number" || !Number.isFinite(timestamp)) return undefined;
	return timestamp;
}

export type ActiveBranchSyncApplyResult = "applied" | "reload";

export function applyActiveBranchSync(snapshot: SessionSnapshot | undefined, sync: ActiveBranchSyncResponse): ActiveBranchSyncApplyResult {
	if (!snapshot || snapshot.session_id !== sync.session_id) return "reload";
	if (sync.status === "branch_changed") return "reload";
	const overview = sync.overview;
	const nextEntries = [...(snapshot.entries ?? [])];
	if (sync.status === "extended") {
		for (const entry of sync.entries) {
			if (nextEntries.some((candidate) => candidate.id === entry.id)) continue;
			const currentLeafId = nextEntries.at(-1)?.id ?? null;
			if (displayParentIdForEntry(entry) !== currentLeafId) return "reload";
			nextEntries.push(entry);
		}
	}
	if ((nextEntries.at(-1)?.id ?? null) !== sync.active_leaf_id) return "reload";
	Object.assign(snapshot, {
		...snapshot,
		...overview,
		active_leaf_id: sync.active_leaf_id,
		entries: nextEntries,
	});
	return "applied";
}
