import { PassThrough } from "node:stream";
import { describe, expect, it, vi } from "vitest";
import {
	attachOrchestratorShadowBridge,
	RelayCoreBridgeClient,
} from "../../src/bridge/client.js";
import {
	decodeRelayCoreBridgeMessage,
	encodeRelayCoreBridgeMessage,
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
		input,
		sent,
	};
}

function createChangeErrorBridge() {
	const input = new PassThrough();
	const output = new PassThrough();
	const sent: RelayCoreBridgeMessage[] = [];

	output.on("data", (chunk) => {
		const lines = Buffer.from(chunk).toString("utf8").split("\n").filter(Boolean);
		for (const line of lines) {
			const message = decodeRelayCoreBridgeMessage(line);
			sent.push(message);
			if (message.type !== "call") {
				continue;
			}

			if (message.command.kind === "sync_snapshot" && message.command.reason === "change") {
				input.write(
					encodeRelayCoreBridgeMessage({
						type: "error",
						id: message.id,
						error: {
							message: "shadow sync failed",
						},
					}),
				);
				continue;
			}

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
	});

	return {
		client: new RelayCoreBridgeClient({ input, output }),
		input,
		sent,
	};
}

function createShadowSource() {
	const listeners = new Set<() => void>();
	let childCount = 0;

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
					childCount,
					sessionFile: undefined,
					lastOutput: undefined,
				},
			],
			subscribeToChanges(listener: () => void) {
				listeners.add(listener);
				return () => listeners.delete(listener);
			},
		},
		incrementChildren() {
			childCount += 1;
		},
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

describe("attachOrchestratorShadowBridge lifecycle", () => {
	it("unsubscribes from orchestrator changes after the bridge disconnects", async () => {
		const shadowSource = createShadowSource();
		const bridge = createRecordingBridge();
		const onDisconnect = vi.fn();
		const controller = attachOrchestratorShadowBridge(shadowSource.source, bridge.client, {
			onDisconnect,
		});

		await controller.start();
		expect(shadowSource.listenerCount()).toBe(1);
		const sentBeforeDisconnect = bridge.sent.length;

		bridge.client.close(new Error("stdio ended"));
		expect(onDisconnect).toHaveBeenCalledWith(expect.objectContaining({ message: "stdio ended" }));
		expect(shadowSource.listenerCount()).toBe(0);

		shadowSource.incrementChildren();
		shadowSource.emitChange();
		await controller.flush();

		expect(bridge.sent).toHaveLength(sentBeforeDisconnect);
		await expect(controller.stop()).resolves.toBeUndefined();
	});

	it("handles change-sync failures without unhandled rejections", async () => {
		const shadowSource = createShadowSource();
		const bridge = createChangeErrorBridge();
		const onDisconnect = vi.fn();
		const unhandledRejection = vi.fn();
		process.on("unhandledRejection", unhandledRejection);

		try {
			const controller = attachOrchestratorShadowBridge(shadowSource.source, bridge.client, {
				onDisconnect,
			});

			await controller.start();
			shadowSource.incrementChildren();
			shadowSource.emitChange();
			await controller.flush();
			await new Promise((resolve) => setTimeout(resolve, 0));

			expect(onDisconnect).toHaveBeenCalledWith(expect.objectContaining({ message: "shadow sync failed" }));
			expect(shadowSource.listenerCount()).toBe(0);
			expect(unhandledRejection).not.toHaveBeenCalled();
			await expect(controller.stop()).resolves.toBeUndefined();
		} finally {
			process.off("unhandledRejection", unhandledRejection);
		}
	});
});
