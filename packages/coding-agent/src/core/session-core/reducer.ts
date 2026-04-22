import type { SessionCoreCommand } from "./commands.js";
import type { SessionCoreEffect } from "./effects.js";
import type { SessionCoreEvent, SessionCoreQueueChangeReason } from "./events.js";
import { createSessionCoreState, type SessionCoreQueueState, type SessionCoreState } from "./state.js";

export interface SessionCoreTransition {
	state: SessionCoreState;
	events: SessionCoreEvent[];
	effects: SessionCoreEffect[];
}

function createTransition(
	state: SessionCoreState,
	events: SessionCoreEvent[] = [],
	effects: SessionCoreEffect[] = [],
): SessionCoreTransition {
	return { state, events, effects };
}

function removeFirstMatch(values: readonly string[], text: string): string[] | undefined {
	const index = values.indexOf(text);
	if (index === -1) {
		return undefined;
	}

	return [...values.slice(0, index), ...values.slice(index + 1)];
}

function withQueueUpdate(
	state: SessionCoreState,
	queue: SessionCoreQueueState,
	reason: SessionCoreQueueChangeReason,
	extraEvents: SessionCoreEvent[] = [],
): SessionCoreTransition {
	const nextState = createSessionCoreState({
		...state,
		queue,
	});

	return createTransition(
		nextState,
		[
			...extraEvents,
			{
				type: "queue/updated",
				queue: nextState.queue,
				reason,
			},
		],
		[
			{
				type: "emit_queue_update",
				queue: nextState.queue,
				reason,
			},
		],
	);
}

export function reduceSessionCore(state: SessionCoreState, command: SessionCoreCommand): SessionCoreTransition {
	switch (command.type) {
		case "queue/enqueue-steering":
			return withQueueUpdate(
				state,
				{
					steering: [...state.queue.steering, command.text],
					followUp: [...state.queue.followUp],
				},
				"enqueue-steering",
			);

		case "queue/enqueue-follow-up":
			return withQueueUpdate(
				state,
				{
					steering: [...state.queue.steering],
					followUp: [...state.queue.followUp, command.text],
				},
				"enqueue-follow-up",
			);

		case "queue/consume-user-message": {
			const steering = removeFirstMatch(state.queue.steering, command.text);
			if (steering) {
				return withQueueUpdate(
					state,
					{
						steering,
						followUp: [...state.queue.followUp],
					},
					"consume-user-message",
					[{ type: "queue/message-consumed", queueKind: "steering", text: command.text }],
				);
			}

			const followUp = removeFirstMatch(state.queue.followUp, command.text);
			if (followUp) {
				return withQueueUpdate(
					state,
					{
						steering: [...state.queue.steering],
						followUp,
					},
					"consume-user-message",
					[{ type: "queue/message-consumed", queueKind: "followUp", text: command.text }],
				);
			}

			return createTransition(state);
		}

		case "queue/clear":
			return withQueueUpdate(
				state,
				{
					steering: [],
					followUp: [],
				},
				"clear",
			);

		case "run-state/set": {
			if (state.runState === command.runState) {
				return createTransition(state);
			}

			const nextState = createSessionCoreState({
				...state,
				runState: command.runState,
			});

			return createTransition(
				nextState,
				[{ type: "run-state/updated", runState: nextState.runState }],
				[{ type: "run-state/updated", runState: nextState.runState }],
			);
		}
	}
}
