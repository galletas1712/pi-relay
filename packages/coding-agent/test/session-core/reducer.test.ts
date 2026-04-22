import { describe, expect, test } from "vitest";
import { SessionCoreCommands } from "../../src/core/session-core/commands.js";
import {
	createSessionCoreInterpreter,
	getSessionCorePendingMessageCount,
	getSessionCoreQueue,
} from "../../src/core/session-core/interpreter.js";

describe("session-core queue reducer", () => {
	test("consume-user-message prefers steering entries before follow-up entries", () => {
		const core = createSessionCoreInterpreter();

		core.dispatch(SessionCoreCommands.enqueueFollowUp("same text"));
		core.dispatch(SessionCoreCommands.enqueueSteering("same text"));

		const transition = core.dispatch(SessionCoreCommands.consumeUserMessage("same text"));

		expect(getSessionCoreQueue(core)).toEqual({
			steering: [],
			followUp: ["same text"],
		});
		expect(transition.events).toContainEqual({
			type: "queue/message-consumed",
			queueKind: "steering",
			text: "same text",
		});
		expect(transition.effects).toContainEqual({
			type: "emit_queue_update",
			queue: {
				steering: [],
				followUp: ["same text"],
			},
			reason: "consume-user-message",
		});
	});

	test("pending message count tracks both queues across clear", () => {
		const core = createSessionCoreInterpreter();

		core.dispatch(SessionCoreCommands.enqueueSteering("first"));
		core.dispatch(SessionCoreCommands.enqueueFollowUp("second"));

		expect(getSessionCorePendingMessageCount(core)).toBe(2);

		const transition = core.dispatch(SessionCoreCommands.clearQueues());

		expect(getSessionCorePendingMessageCount(core)).toBe(0);
		expect(getSessionCoreQueue(core)).toEqual({ steering: [], followUp: [] });
		expect(transition.effects).toContainEqual({
			type: "emit_queue_update",
			queue: { steering: [], followUp: [] },
			reason: "clear",
		});
	});
});
