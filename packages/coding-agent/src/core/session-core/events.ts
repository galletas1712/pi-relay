import type { SessionCoreQueueKind, SessionCoreQueueState, SessionCoreRunState } from "./state.js";

export type SessionCoreQueueChangeReason =
	| "enqueue-steering"
	| "enqueue-follow-up"
	| "consume-user-message"
	| "clear";

export type SessionCoreEvent =
	| {
			type: "queue/updated";
			queue: SessionCoreQueueState;
			reason: SessionCoreQueueChangeReason;
	  }
	| {
			type: "queue/message-consumed";
			queueKind: SessionCoreQueueKind;
			text: string;
	  }
	| {
			type: "run-state/updated";
			runState: SessionCoreRunState;
	  };
