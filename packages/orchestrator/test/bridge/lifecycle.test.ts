import { PassThrough } from "node:stream";
import { describe, expect, it, vi } from "vitest";
import {
	attachOrchestratorShadowBridge,
	RelayCoreBridgeClient,
} from "../../src/bridge/client.js";
import {
	decodeRelayCoreBridgeMessage,
	encodeRelayCoreBridgeMessage,
	RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
	type RelayCoreBridgeMessage,
} from "../../src/bridge/codec.js";

function createRecordingBridge() {
	const input = new PassThrough();
	const output = new PassThrough();
	const sent: RelayCoreBridgeMessage[] = [];

	output.on("data", (chunk) => {
		const lines = Buffer.from(chunk).toString("utf8").split("\n").filter(Boolean);
		for (const line of lines) {
			const message = decodeRelayCoreBridgeMessage(line);
			sent.push(message);
			if (message.type === "call") {
				input.write(
					encodeRelayCoreBridgeMessage({
						type: "result",
						id: message.id,
						value: {
							acceptedCommand: message.command.kind,
							acceptedAt: "2026-04-22T00:00:00.000Z",
						},
					}),
				);
			}
		}
	});

	return {
		client: new RelayCoreBridgeClient({ input, output }),
		sent,
	};
}

function createShadowSource() {
	const listeners = new Set<() => void>();
	const cleanup = vi.fn((listener: () => void) => {
		listeners.delete(listener);
	});

	return {
		source: {
			rootAgentId: "root",
			getAgentSummaries: () => [
				{
					id: "root",
					parentId: null,
					role: "root",
					status: "idle" as const,
					depth: 0,
					childCount: 0,
					sessionFile: undefined,
					lastOutput: undefined,
				},
			],
			subscribeToChanges(listener: () => void) {
				listeners.add(listener);
				return () => cleanup(listener);
			},
		},
		cleanup,
		emitChange() {
			for (const listener of listeners) {
				listener();
			}
		},
		listenerCount() {
			return listeners.size;
		},
	};
}

describe("relay-core bridge lifecycle", () => {
	it("rejects pending calls and reports disconnects when the input stream ends", async () => {
		const input = new PassThrough();
		const output = new PassThrough();
		const onDisconnect = vi.fn();
		const client = new RelayCoreBridgeClient({ input, output }, { onDisconnect });

		const pendingHello = client.hello();
		input.end();

		await expect(pendingHello).rejects.toThrow("relay-core bridge closed its input stream");
		expect(onDisconnect).toHaveBeenCalledTimes(1);
		await expect(
			client.syncSnapshot(
				{
					protocolVersion: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
					rootAgentId: "root",
					generatedAt: "2026-04-22T00:00:00.000Z",
					agents: [],
				},
				"init",
			),
		).rejects.toThrow("relay-core bridge client is closed");
	});

	it("cleans up source subscriptions and stops syncing changes after stop", async () => {
		const shadowSource = createShadowSource();
		const bridge = createRecordingBridge();
		const controller = attachOrchestratorShadowBridge(shadowSource.source, bridge.client);

		await controller.start();
		expect(shadowSource.listenerCount()).toBe(1);

		await controller.stop();
		expect(shadowSource.cleanup).toHaveBeenCalledTimes(1);
		expect(shadowSource.listenerCount()).toBe(0);

		const sentBeforeChange = bridge.sent.length;
		shadowSource.emitChange();
		await Promise.resolve();

		expect(bridge.sent).toHaveLength(sentBeforeChange);
		expect(bridge.sent.at(-1)).toMatchObject({
			type: "call",
			command: { kind: "dispose" },
		});
	});
});
