import { describe, expect, it } from "vitest";
import {
	decodeSessionShadowBridgeMessage,
	encodeSessionShadowBridgeMessage,
	SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
	type SessionShadowBridgeCallMessage,
} from "../../src/core/session-shadow/codec.js";

describe("session-core shadow bridge codec", () => {
	it("round-trips sync_state frames", () => {
		const message: SessionShadowBridgeCallMessage = {
			type: "call",
			id: 9,
			command: {
				kind: "sync_state",
				reason: "init",
				snapshot: {
					protocolVersion: SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
					generatedAt: "2026-04-22T00:00:00.000Z",
					state: {
						runState: "idle",
						queue: {
							steering: ["urgent"],
							followUp: ["later"],
						},
					},
				},
			},
		};

		expect(decodeSessionShadowBridgeMessage(encodeSessionShadowBridgeMessage(message).trim())).toEqual(message);
	});

	it("round-trips dispatch frames", () => {
		const message: SessionShadowBridgeCallMessage = {
			type: "call",
			id: 10,
			command: {
				kind: "dispatch",
				command: {
					type: "queue/enqueue-follow-up",
					text: "after tools",
				},
			},
		};

		expect(decodeSessionShadowBridgeMessage(encodeSessionShadowBridgeMessage(message).trim())).toEqual(message);
	});
});
