import type { SessionCoreCommand } from "./commands.js";
import { reduceSessionCore, type SessionCoreTransition } from "./reducer.js";
import {
	createSessionCoreState,
	getPendingSessionCoreMessageCount,
	type SessionCoreQueueState,
	type SessionCoreState,
} from "./state.js";

export interface SessionCoreInterpreter {
	getState(): SessionCoreState;
	dispatch(command: SessionCoreCommand): SessionCoreTransition;
	reset(state?: SessionCoreState): void;
}

export function createSessionCoreInterpreter(initialState: SessionCoreState = createSessionCoreState()): SessionCoreInterpreter {
	let state = createSessionCoreState(initialState);

	return {
		getState() {
			return state;
		},
		dispatch(command) {
			const transition = reduceSessionCore(state, command);
			state = transition.state;
			return transition;
		},
		reset(nextState = createSessionCoreState()) {
			state = createSessionCoreState(nextState);
		},
	};
}

export function getSessionCoreQueue(interpreter: Pick<SessionCoreInterpreter, "getState">): SessionCoreQueueState {
	return interpreter.getState().queue;
}

export function getSessionCorePendingMessageCount(interpreter: Pick<SessionCoreInterpreter, "getState">): number {
	return getPendingSessionCoreMessageCount(interpreter.getState());
}
