import type { SelectedSessionCache } from "./selectedSessionCache.ts";
import type { Activity, DelegationSubagent, EventFrame } from "./types.ts";
import type { SessionListItem } from "./sessionList.ts";

export function sessionListRefreshKey(projectId: string | null): string {
	return projectId ?? "__host__";
}

export function projectIdFromEventData(event: EventFrame): string | null | undefined {
	const value = event.data.project_id;
	if (typeof value === "string") return value;
	if (value === null) return null;
	return undefined;
}

export function firstKnownProjectId(...projectIds: (string | null | undefined)[]): string | null | undefined {
	for (const projectId of projectIds) {
		if (projectId !== undefined) return projectId;
	}
	return undefined;
}

export function backgroundSessionNeedsWarm(
	session: SessionListItem,
	cache: SelectedSessionCache | null,
	warmedUpdatedAt: string | undefined,
): boolean {
	if (!cache?.snapshot) return true;
	if (warmedUpdatedAt !== session.updated_at) return true;
	if (cache.snapshot.activity !== session.activity) return true;
	if (cache.snapshot.active_leaf_id !== session.active_leaf_id) return true;
	if (session.has_transcript_entries && cache.turnOrder.length === 0) return true;
	return false;
}

export function canWarmBackgroundSession(session: SessionListItem): boolean {
	if (session.parent_session_id) return false;
	if (session.metadata?.hidden === true) return false;
	if (session.metadata?.archived === true) return false;
	if (session.metadata?.subagent === true) return false;
	return true;
}

export function subagentStatusNeedsWarm(status: DelegationSubagent["status"], activity?: Activity): boolean {
	return activity === "running" || activity === "queued" || status === "running" || status === "queued";
}
