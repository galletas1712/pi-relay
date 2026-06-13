import type { EventFrame } from "./types.ts";

export interface SessionEventRefreshPlan {
	syncSelected: boolean;
	refreshList: boolean;
}

const SESSION_LIST_REFRESH_EVENTS = new Set([
	"session.created",
	"session.configured",
	"session.idle",
	"session.recovered",
	"session.work_cancelled",
	"subagent.spawned",
	"subagent.running",
	"subagent.idle",
	"input.queued",
	"input.promoted",
	"input.updated",
	"input.cancelled",
	"input.reordered",
	"input.consumed",
	"input.accepted",
	"input.ignored",
	"history.switched",
	"history.compacted",
	"action.requested",
	"model.requested",
	"model.completed",
	"model.error",
	"tool.requested",
	"tool.started",
	"tool.completed",
	"tool.error",
	"compaction.requested",
	"compaction.completed",
	"compaction.error",
	"turn.finished",
]);

const SELECTED_SESSION_REFRESH_EVENTS = new Set([
	"session.configured",
	"session.recovered",
	"session.idle",
	"session.work_cancelled",
	"subagent.spawned",
	"subagent.running",
	"subagent.idle",
	"history.switched",
	"history.compacted",
	"compaction.requested",
	"compaction.completed",
	"compaction.error",
]);

const KNOWN_SESSION_EVENTS = new Set([
	...SESSION_LIST_REFRESH_EVENTS,
	"transcript.appended",
	"turn.started",
	"assistant.message",
]);

export function refreshPlanForEvent(event: Pick<EventFrame, "event">): SessionEventRefreshPlan {
	return {
		syncSelected: SELECTED_SESSION_REFRESH_EVENTS.has(event.event) || !KNOWN_SESSION_EVENTS.has(event.event),
		refreshList: SESSION_LIST_REFRESH_EVENTS.has(event.event),
	};
}
