import type { SessionCoreRunState } from "./state.js";

export type SessionCoreCommand =
	| { type: "queue/enqueue-steering"; text: string }
	| { type: "queue/enqueue-follow-up"; text: string }
	| { type: "queue/consume-user-message"; text: string }
	| { type: "queue/clear" }
	| { type: "run-state/set"; runState: SessionCoreRunState };

export const SessionCoreCommands = {
	enqueueSteering(text: string): SessionCoreCommand {
		return { type: "queue/enqueue-steering", text };
	},
	enqueueFollowUp(text: string): SessionCoreCommand {
		return { type: "queue/enqueue-follow-up", text };
	},
	consumeUserMessage(text: string): SessionCoreCommand {
		return { type: "queue/consume-user-message", text };
	},
	clearQueues(): SessionCoreCommand {
		return { type: "queue/clear" };
	},
	setRunState(runState: SessionCoreRunState): SessionCoreCommand {
		return { type: "run-state/set", runState };
	},
};
