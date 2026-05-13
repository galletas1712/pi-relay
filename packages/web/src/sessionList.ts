import type { Activity, SessionSummary } from "./types.ts";

export type SessionListItem = SessionSummary;

export function sessionTitle(session: SessionListItem): string {
	const title = session.metadata?.title;
	return typeof title === "string" && title.trim() ? title : session.session_id.slice(0, 13);
}

export function isArchivedSession(session: SessionListItem): boolean {
	return session.metadata?.archived === true;
}

export function tallyActivities(sessions: SessionListItem[]): Record<Activity, number> {
	return sessions.reduce<Record<Activity, number>>(
		(counts, session) => {
			counts[session.activity] += 1;
			return counts;
		},
		{ idle: 0, queued: 0, running: 0 }
	);
}
