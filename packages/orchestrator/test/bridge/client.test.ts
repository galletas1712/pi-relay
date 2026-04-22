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
	const onEvent = vi.fn();
	const onDisconnect = vi.fn();

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
		client: new RelayCoreBridgeClient({ input, output }, { onEvent, onDisconnect }),
		input,
		output,
		sent,
		onEvent,
		onDisconnect,
	};
}

type ShadowAgent = {
	id: string;
	parentId: string | null;
	role: string;
	status: "running" | "idle" | "disposed";
	depth: number;
	childCount: number;
	sessionFile: string | undefined;
	lastOutput: string | undefined;
};

function createShadowSource() {
	const listeners = new Set<() => void>();
	let agents: ShadowAgent[] = [
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
	];

	return {
		source: {
			rootAgentId: "root",
			getAgentSummaries: () => agents,
			subscribeToChanges(listener: () => void) {
				listeners.add(listener);
				return () => listeners.delete(listener);
			},
		},
		setAgents(nextAgents: typeof agents) {
			agents = nextAgents;
		},
		emitChange() {
			for (const listener of listeners) {
				listener();
			}
		},
	};
}

describe("relay-core bridge client", () => {
	it("streams orchestrator snapshots in shadow mode without changing authority", async () => {
		const shadowSource = createShadowSource();
		const bridge = createRecordingBridge();
		const controller = attachOrchestratorShadowBridge(shadowSource.source, bridge.client);

		await controller.start();

		expect(bridge.sent[0]).toMatchObject({
			type: "call",
			command: {
				kind: "hello",
				mode: "shadow",
				protocolVersion: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
			},
		});
		expect(bridge.sent[1]).toMatchObject({
			type: "call",
			command: {
				kind: "sync_snapshot",
				reason: "init",
				snapshot: {
					rootAgentId: "root",
				},
			},
		});

		shadowSource.setAgents([
			{
				id: "root",
				parentId: null,
				role: "root",
				status: "idle",
				depth: 0,
				childCount: 1,
				sessionFile: undefined,
				lastOutput: undefined,
			},
			{
				id: "explore-12345678",
				parentId: "root",
				role: "explore",
				status: "running",
				depth: 1,
				childCount: 0,
				sessionFile: "/tmp/explore.jsonl",
				lastOutput: "Scanning files",
			},
		]);
		shadowSource.emitChange();
		await controller.flush();

		expect(
			bridge.sent.some(
				(message) =>
					message.type === "call" &&
					message.command.kind === "sync_snapshot" &&
					message.command.reason === "change" &&
					message.command.snapshot.agents.some((agent) => agent.role === "explore"),
			),
		).toBe(true);

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
			encodeRelayCoreBridgeMessage({
				type: "event",
				event: {
					type: "diagnostic",
					level: "info",
					message: "shadow host ready",
				},
			}),
		);

		await vi.waitFor(() => {
			expect(bridge.onEvent).toHaveBeenCalledWith({
				type: "diagnostic",
				level: "info",
				message: "shadow host ready",
			});
		});

		bridge.client.close();
	});

	it("stops cleanly after the remote bridge disconnects", async () => {
		const shadowSource = createShadowSource();
		const bridge = createRecordingBridge();
		const controller = attachOrchestratorShadowBridge(shadowSource.source, bridge.client);

		await controller.start();
		bridge.input.end();

		await vi.waitFor(() => {
			expect(bridge.onDisconnect).toHaveBeenCalledTimes(1);
		});

		await expect(controller.stop()).resolves.toBeUndefined();
		await expect(controller.stop()).resolves.toBeUndefined();
	});

	it("stops cleanly after the client is already closed locally", async () => {
		const shadowSource = createShadowSource();
		const bridge = createRecordingBridge();
		const controller = attachOrchestratorShadowBridge(shadowSource.source, bridge.client);

		await controller.start();
		bridge.client.close();

		await expect(controller.stop()).resolves.toBeUndefined();
	});
});
