import { PassThrough } from "node:stream";
import { describe, expect, it, vi } from "vitest";
import { attachSessionShadowBridge, SessionShadowBridgeClient } from "../../src/core/session-shadow/client.js";
import {
	decodeSessionShadowBridgeMessage,
	encodeSessionShadowBridgeMessage,
	SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
	type SessionShadowBridgeMessage,
} from "../../src/core/session-shadow/codec.js";

function createRecordingBridge(
	onCall?: (message: Extract<SessionShadowBridgeMessage, { type: "call" }>) => SessionShadowBridgeMessage | undefined,
) {
	const input = new PassThrough();
	const output = new PassThrough();
	const sent: SessionShadowBridgeMessage[] = [];
	const onEvent = vi.fn();

	output.on("data", (chunk) => {
		const lines = Buffer.from(chunk)
			.toString("utf8")
			.split("\n")
			.filter(Boolean);
		for (const line of lines) {
			const message = decodeSessionShadowBridgeMessage(line);
			sent.push(message);
			if (message.type === "call") {
				const response = onCall?.(message);
				input.write(
					encodeSessionShadowBridgeMessage(
						response ?? {
							type: "result",
							id: message.id,
							value: {
								acceptedCommand: message.command.kind,
								acceptedAt: "2026-04-22T00:00:00.000Z",
							},
						},
					),
				);
			}
		}
	});

	return {
		client: new SessionShadowBridgeClient({ input, output }, { onEvent }),
		input,
		output,
		sent,
		onEvent,
	};
}

describe("session-core shadow bridge client", () => {
	it("streams init snapshots and dispatch commands without changing local authority", async () => {
		const bridge = createRecordingBridge();
		const controller = attachSessionShadowBridge(bridge.client);

		await controller.start({
			runState: "idle",
			queue: {
				steering: [],
				followUp: [],
			},
		});
		await controller.dispatch({
			type: "queue/enqueue-follow-up",
			text: "after tools",
		});
		await controller.dispatch({
			type: "run-state/set",
			runState: "retrying",
		});
		await controller.flush();

		expect(bridge.sent[0]).toMatchObject({
			type: "call",
			command: {
				kind: "hello",
				mode: "shadow",
				protocolVersion: SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
			},
		});
		expect(bridge.sent[1]).toMatchObject({
			type: "call",
			command: {
				kind: "sync_state",
				reason: "init",
				snapshot: {
					state: {
						runState: "idle",
					},
				},
			},
		});
		expect(bridge.sent[2]).toMatchObject({
			type: "call",
			command: {
				kind: "dispatch",
				command: {
					type: "queue/enqueue-follow-up",
					text: "after tools",
				},
			},
		});
		expect(bridge.sent[3]).toMatchObject({
			type: "call",
			command: {
				kind: "dispatch",
				command: {
					type: "run-state/set",
					runState: "retrying",
				},
			},
		});

		await controller.stop();

		expect(bridge.sent.at(-1)).toMatchObject({
			type: "call",
			command: {
				kind: "dispose",
			},
		});
	});

	it("forwards bridge events to observers", async () => {
		const bridge = createRecordingBridge();

		bridge.input.write(
			encodeSessionShadowBridgeMessage({
				type: "event",
				event: {
					type: "diagnostic",
					level: "info",
					message: "session shadow ready",
				},
			}),
		);

		await vi.waitFor(() => {
			expect(bridge.onEvent).toHaveBeenCalledWith({
				type: "diagnostic",
				level: "info",
				message: "session shadow ready",
			});
		});

		bridge.client.close();
	});

	it("allows init retry after a failed handshake and blocks dispatch until sync succeeds", async () => {
		let failHello = true;
		const bridge = createRecordingBridge((message) => {
			if (failHello && message.command.kind === "hello") {
				failHello = false;
				return {
					type: "error",
					id: message.id,
					error: {
						message: "hello failed",
					},
				};
			}
			return undefined;
		});
		const controller = attachSessionShadowBridge(bridge.client);

		await expect(
			controller.start({
				runState: "idle",
				queue: {
					steering: [],
					followUp: [],
				},
			}),
		).rejects.toThrow("hello failed");

		await expect(
			controller.dispatch({
				type: "queue/enqueue-follow-up",
				text: "after tools",
			}),
		).rejects.toThrow("has not completed initial sync");

		await controller.start({
			runState: "idle",
			queue: {
				steering: [],
				followUp: [],
			},
		});
		await controller.dispatch({
			type: "queue/enqueue-follow-up",
			text: "after tools",
		});
		await controller.flush();

		expect(
			bridge.sent.filter(
				(message) => message.type === "call" && message.command.kind === "hello",
			),
		).toHaveLength(2);
		expect(
			bridge.sent.filter(
				(message) => message.type === "call" && message.command.kind === "dispatch",
			),
		).toHaveLength(1);

		await controller.stop();
	});
});
