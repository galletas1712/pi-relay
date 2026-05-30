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

export function refreshPlanForEvent(event: Pick<EventFrame, "event">): SessionEventRefreshPlan {
	return {
		syncSelected: true,
		refreshList: SESSION_LIST_REFRESH_EVENTS.has(event.event),
	};
}
