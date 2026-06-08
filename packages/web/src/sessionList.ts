import type { Activity, Project, SessionSummary } from "./types.ts";

export type SessionListItem = SessionSummary;
export type SessionDisplayInfo = Pick<SessionSummary, "session_id" | "project_id" | "activity" | "active_leaf_id" | "provider" | "metadata">;
export type SessionDisplayActivity = "idle" | "running";

export function sessionTitle(session: SessionDisplayInfo): string {
	const title = session.metadata?.title;
	return typeof title === "string" && title.trim() ? title : session.session_id.slice(0, 13);
}

export function isArchivedSession(session: SessionDisplayInfo): boolean {
	return session.metadata?.archived === true;
}

export function sortSessionsByLastUserMessage(sessions: SessionListItem[]): SessionListItem[] {
	return [...sessions].sort(compareSessionsByLastUserMessage);
}

function compareSessionsByLastUserMessage(left: SessionListItem, right: SessionListItem): number {
	const archivedDifference = Number(isArchivedSession(left)) - Number(isArchivedSession(right));
	if (archivedDifference !== 0) return archivedDifference;

	const leftTimestamp = sortableLastUserMessageTimestamp(left);
	const rightTimestamp = sortableLastUserMessageTimestamp(right);
	if (leftTimestamp !== rightTimestamp) return rightTimestamp - leftTimestamp;

	const leftCreatedAt = Date.parse(left.created_at);
	const rightCreatedAt = Date.parse(right.created_at);
	if (Number.isFinite(leftCreatedAt) && Number.isFinite(rightCreatedAt) && leftCreatedAt !== rightCreatedAt) {
		return rightCreatedAt - leftCreatedAt;
	}

	if (left.session_id < right.session_id) return 1;
	if (left.session_id > right.session_id) return -1;
	return 0;
}

function sortableLastUserMessageTimestamp(session: SessionListItem): number {
	const timestamp = session.last_user_message_timestamp_ms;
	return typeof timestamp === "number" && Number.isFinite(timestamp) ? timestamp : -Infinity;
}

export function sessionDisplayActivity(session: SessionDisplayInfo): SessionDisplayActivity {
	return displayActivity(session.activity);
}

export function displayActivity(activity: Activity): SessionDisplayActivity {
	return activity === "idle" ? "idle" : "running";
}

export function tallyActivities(sessions: SessionListItem[]): Record<SessionDisplayActivity, number> {
	return sessions.reduce<Record<SessionDisplayActivity, number>>(
		(counts, session) => {
			counts[sessionDisplayActivity(session)] += 1;
			return counts;
		},
		{ idle: 0, running: 0 }
	);
}

export function projectTitle(project: Project): string {
	const name = project.name.trim();
	return name || project.project_id.slice(0, 8);
}
