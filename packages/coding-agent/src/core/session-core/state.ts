export type SessionCoreRunState = "idle" | "running" | "retrying" | "compacting";

export type SessionCoreQueueKind = "steering" | "followUp";

export interface SessionCoreQueueState {
	steering: readonly string[];
	followUp: readonly string[];
}

export interface SessionCoreState {
	runState: SessionCoreRunState;
	queue: SessionCoreQueueState;
}

export function createEmptySessionCoreQueueState(): SessionCoreQueueState {
	return {
		steering: [],
		followUp: [],
	};
}

export function createSessionCoreState(overrides: Partial<SessionCoreState> = {}): SessionCoreState {
	return {
		runState: overrides.runState ?? "idle",
		queue: overrides.queue
			? {
					steering: [...overrides.queue.steering],
					followUp: [...overrides.queue.followUp],
			  }
			: createEmptySessionCoreQueueState(),
	};
}

export function getPendingSessionCoreMessageCount(state: Pick<SessionCoreState, "queue">): number {
	return state.queue.steering.length + state.queue.followUp.length;
}
