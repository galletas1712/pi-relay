import type { EventFrame } from "./types.ts";

export interface SessionEventRefreshPlan {
	refreshSession: boolean;
	refreshActiveBranch: boolean;
	refreshList: boolean;
}

const ACTIVE_BRANCH_REFRESH_EVENTS = new Set([
	"session.created",
	"session.recovered",
	"history.rewound",
	"history.compacted",
	"compaction.completed",
	"action.requested",
	"transcript.appended",
	"turn.started",
	"turn.finished",
	"assistant.message",
]);

const SELECTED_SESSION_REFRESH_EVENTS = new Set([
	"session.created",
	"session.configured",
	"session.idle",
	"session.recovered",
	"session.work_cancelled",
	"input.queued",
	"input.promoted",
	"input.consumed",
	"input.accepted",
	"input.ignored",
	"history.rewound",
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
	"transcript.appended",
	"turn.started",
	"turn.finished",
	"assistant.message",
]);

const SESSION_LIST_REFRESH_EVENTS = new Set([
	"session.created",
	"session.configured",
	"session.idle",
	"session.recovered",
	"session.work_cancelled",
	"input.queued",
	"input.promoted",
	"input.consumed",
	"input.accepted",
	"input.ignored",
	"history.forked",
	"history.rewound",
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
		refreshSession: SELECTED_SESSION_REFRESH_EVENTS.has(event.event),
		refreshActiveBranch: ACTIVE_BRANCH_REFRESH_EVENTS.has(event.event),
		refreshList: SESSION_LIST_REFRESH_EVENTS.has(event.event),
	};
}
