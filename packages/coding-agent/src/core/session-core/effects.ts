import type { SessionCoreQueueChangeReason } from "./events.js";
import type { SessionCoreQueueState, SessionCoreRunState } from "./state.js";

export type SessionCoreEffect =
	| {
			type: "emit_queue_update";
			queue: SessionCoreQueueState;
			reason: SessionCoreQueueChangeReason;
	  }
	| {
			type: "run-state/updated";
			runState: SessionCoreRunState;
	  };
