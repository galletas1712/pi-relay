import type { Activity, SessionSummary } from "./types.ts";

export type SessionListItem = SessionSummary;
export type SessionDisplayActivity = "idle" | "running";

export function sessionTitle(session: SessionListItem): string {
	const title = session.metadata?.title;
	return typeof title === "string" && title.trim() ? title : session.session_id.slice(0, 13);
}

export function isArchivedSession(session: SessionListItem): boolean {
	return session.metadata?.archived === true;
}

export function sessionDisplayActivity(session: SessionListItem): SessionDisplayActivity {
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
