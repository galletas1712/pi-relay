import { describe, expect, it } from "vitest";
import {
	decodeRelayCoreBridgeMessage,
	encodeRelayCoreBridgeMessage,
	RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
	type RelayCoreBridgeCallMessage,
} from "../../src/bridge/codec.js";

describe("relay-core bridge codec", () => {
	it("round-trips sync snapshot frames", () => {
		const message: RelayCoreBridgeCallMessage = {
			type: "call",
			id: 7,
			command: {
				kind: "sync_snapshot",
				reason: "init",
				snapshot: {
					protocolVersion: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
					rootAgentId: "root",
					generatedAt: "2026-04-22T00:00:00.000Z",
					agents: [
						{
							id: "root",
							parentId: null,
							role: "root",
							status: "idle",
							depth: 0,
							childCount: 0,
							sessionFile: undefined,
							lastOutput: undefined,
						},
					],
				},
			},
		};

		expect(decodeRelayCoreBridgeMessage(encodeRelayCoreBridgeMessage(message).trim())).toEqual(message);
	});
});
