import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { AgentRpcClient, RPC_REQUEST_TIMEOUT_MS, resolveWsUrl } from "./rpc.ts";

class FakeWebSocket {
	static readonly CONNECTING = 0;
	static readonly OPEN = 1;
	static readonly CLOSING = 2;
	static readonly CLOSED = 3;
	static instances: FakeWebSocket[] = [];

	readonly sent: string[] = [];
	closed = false;
	readyState = FakeWebSocket.CONNECTING;
	private readonly listeners = new Map<string, { listener: EventListenerOrEventListenerObject; once: boolean }[]>();

	constructor(readonly url: string) {
		FakeWebSocket.instances.push(this);
	}

	addEventListener(type: string, listener: EventListenerOrEventListenerObject, options?: AddEventListenerOptions | boolean): void {
		const once = typeof options === "object" && options?.once === true;
		this.listeners.set(type, [...(this.listeners.get(type) ?? []), { listener, once }]);
	}

	send(data: string): void {
		this.sent.push(data);
	}

	close(): void {
		this.closed = true;
		this.readyState = FakeWebSocket.CLOSED;
		this.emit("close");
	}

	open(): void {
		this.readyState = FakeWebSocket.OPEN;
		this.emit("open");
	}

	private emit(type: string): void {
		const listeners = this.listeners.get(type) ?? [];
		for (const entry of [...listeners]) {
			if (typeof entry.listener === "function") {
				entry.listener.call(this, { type } as Event);
			} else {
				entry.listener.handleEvent({ type } as Event);
			}
			if (entry.once) {
				this.listeners.set(type, (this.listeners.get(type) ?? []).filter((candidate) => candidate !== entry));
			}
		}
	}
}

const originalWebSocket = globalThis.WebSocket;

beforeEach(() => {
	vi.useFakeTimers();
	FakeWebSocket.instances = [];
	globalThis.WebSocket = FakeWebSocket as unknown as typeof WebSocket;
});

afterEach(() => {
	globalThis.WebSocket = originalWebSocket;
	vi.useRealTimers();
});

describe("resolveWsUrl", () => {
	it("honors an explicit VITE_PI_AGENT_WS override", () => {
		expect(resolveWsUrl("wss://agent.example.test/ws", localLocation())).toBe("wss://agent.example.test/ws");
	});

	it("defaults local web clients to the daemon port", () => {
		expect(resolveWsUrl(undefined, localLocation())).toBe("ws://127.0.0.1:8787");
		expect(resolveWsUrl("", { protocol: "http:", hostname: "localhost", port: "8788" })).toBe("ws://127.0.0.1:8787");
	});

	it("uses same-origin /ws for non-local served clients", () => {
		expect(resolveWsUrl(undefined, { protocol: "https:", hostname: "odin.smelt-anaconda.ts.net", port: "" })).toBe(
			"wss://odin.smelt-anaconda.ts.net/ws",
		);
		expect(resolveWsUrl(undefined, { protocol: "http:", hostname: "example.test", port: "9000" })).toBe(
			"ws://example.test:9000/ws",
		);
	});
});

function localLocation(): Pick<Location, "hostname" | "port" | "protocol"> {
	return { protocol: "http:", hostname: "127.0.0.1", port: "8788" };
}

describe("AgentRpcClient reconnect hardening", () => {
	it("times out a hung request and starts reconnecting", async () => {
		const client = new AgentRpcClient("ws://agent.test/ws");
		const statuses: string[] = [];
		client.onStatus((status) => statuses.push(status));
		const connect = client.connect();
		const socket = FakeWebSocket.instances[0];
		socket.open();
		await connect;

		const request = client.request("session.list");
		const requestRejected = expect(request).rejects.toThrow("websocket request timed out");
		await Promise.resolve();
		expect(socket.sent).toHaveLength(1);

		await vi.advanceTimersByTimeAsync(RPC_REQUEST_TIMEOUT_MS);
		await requestRejected;

		expect(client.isOpen()).toBe(false);
		expect(statuses).toContain("closed");

		await vi.advanceTimersByTimeAsync(750);
		expect(FakeWebSocket.instances).toHaveLength(2);
		client.close();
	});

	it("force-reconnects an apparently open socket and rejects in-flight requests", async () => {
		const client = new AgentRpcClient("ws://agent.test/ws");
		const connect = client.connect();
		const firstSocket = FakeWebSocket.instances[0];
		firstSocket.open();
		await connect;

		const request = client.request("session.get");
		const requestRejected = expect(request).rejects.toThrow("websocket reconnecting");
		await Promise.resolve();
		expect(firstSocket.sent).toHaveLength(1);

		const reconnect = client.reconnect();
		expect(firstSocket.closed).toBe(true);
		expect(FakeWebSocket.instances).toHaveLength(2);
		const secondSocket = FakeWebSocket.instances[1];
		secondSocket.open();
		await reconnect;
		await requestRejected;

		expect(client.isOpen()).toBe(true);
		client.close();
	});
});
