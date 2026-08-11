// Unit tests for BridgeClient (wire semantics, typed errors, reconnect) and
// BridgeSessionStore (attach/resume/rebuild) against a scripted fake socket.
// The real-stack equivalents live in bridge.integration.test.ts (env-gated).
import { beforeEach, describe, expect, it, vi } from "vitest";
import {
	BridgeClient,
	BridgeRequestError,
	BridgeTransportError,
	eventGapData,
	isBridgeErrorCode,
	type BridgeWebSocket,
} from "./client.ts";
import { BridgeSessionStore } from "./sessionStore.ts";
import type { ContractEventEnvelope } from "./types.ts";

class FakeWebSocket {
	static instances: FakeWebSocket[] = [];
	readonly sent: string[] = [];
	readyState = 0;
	closed = false;
	private readonly listeners = new Map<string, Array<(ev: unknown) => void>>();

	constructor(readonly url: string) {
		FakeWebSocket.instances.push(this);
	}

	addEventListener(type: string, listener: (ev: unknown) => void): void {
		this.listeners.set(type, [...(this.listeners.get(type) ?? []), listener]);
	}

	send(data: string): void {
		this.sent.push(data);
		const frame = JSON.parse(data) as { id: string; method: string; params: Record<string, unknown> };
		queueMicrotask(() => {
			serverHandler?.(this, frame);
		});
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		this.readyState = 3;
		this.emit("close", {});
	}

	open(): void {
		this.readyState = 1;
		this.emit("open", {});
	}

	recv(obj: unknown): void {
		this.emit("message", { data: JSON.stringify(obj) });
	}

	respond(id: string, result: unknown): void {
		this.recv({ id, result });
	}

	fail(id: string, code: string, message: string, data?: unknown): void {
		this.recv({ id, error: { code, message, ...(data ? { data } : {}) } });
	}

	private emit(type: string, event: unknown): void {
		for (const listener of this.listeners.get(type) ?? []) listener(event);
	}
}

type ServerHandler = (ws: FakeWebSocket, frame: { id: string; method: string; params: Record<string, unknown> }) => void;
let serverHandler: ServerHandler | null = null;

const originalWebSocket = globalThis.WebSocket;

beforeEach(() => {
	FakeWebSocket.instances = [];
	serverHandler = null;
});

function makeClient(options: { reconnectDelayMs?: number } = {}) {
	const client = new BridgeClient("ws://bridge.test/__bridge-ws", {
		webSocketFactory: (url) => new FakeWebSocket(url) as unknown as BridgeWebSocket,
		reconnectDelayMs: options.reconnectDelayMs ?? 5,
	});
	return client;
}

async function openClient(client: BridgeClient) {
	const connect = client.connect();
	FakeWebSocket.instances.at(-1)?.open();
	await connect;
	return FakeWebSocket.instances.at(-1)!;
}

function ev(sessionId: string, seq: number, event: string, data: Record<string, unknown>, replayed = false): ContractEventEnvelope {
	return { event, sessionId, seq, data, at: new Date().toISOString(), ...(replayed ? { replayed: true } : {}) };
}

describe("BridgeClient", () => {
	it("correlates responses by id and resolves results", async () => {
		const client = makeClient();
		serverHandler = (ws, frame) => {
			if (frame.method === "session.list") ws.respond(frame.id, { sessions: [{ sessionId: "s1" }] });
		};
		await openClient(client);
		const res = await client.listSessions();
		expect(res.sessions[0].sessionId).toBe("s1");
		const sent = JSON.parse(FakeWebSocket.instances[0].sent[0]) as { id: string; method: string };
		expect(sent.method).toBe("session.list");
		client.close();
	});

	it("maps server errors to typed BridgeRequestError with code + data", async () => {
		const client = makeClient();
		serverHandler = (ws, frame) => ws.fail(frame.id, "session_busy", "a turn is running", { seq: 7 });
		await openClient(client);
		const err = await client.sendPrompt("s1", "hi").catch((e: unknown) => e);
		expect(err).toBeInstanceOf(BridgeRequestError);
		expect((err as BridgeRequestError).code).toBe("session_busy");
		expect((err as BridgeRequestError).data).toEqual({ seq: 7 });
		expect(isBridgeErrorCode(err, "session_busy")).toBe(true);
		expect(isBridgeErrorCode(err, "session_not_found")).toBe(false);
		client.close();
	});

	it("parses event_gap data via eventGapData()", async () => {
		const client = makeClient();
		serverHandler = (ws, frame) => ws.fail(frame.id, "event_gap", "trimmed", { minAvailable: 31, headSeq: 88 });
		await openClient(client);
		const err = await client.attachSession("s1", 3).catch((e: unknown) => e);
		expect(eventGapData(err)).toEqual({ minAvailable: 31, headSeq: 88 });
		client.close();
	});

	it("dispatches event frames to subscribers", async () => {
		const client = makeClient();
		serverHandler = () => {};
		const ws = await openClient(client);
		const seen: ContractEventEnvelope[] = [];
		client.onEvent((e) => seen.push(e));
		ws.recv(ev("s1", 1, "session.state", { state: "idle" }));
		expect(seen).toHaveLength(1);
		expect(seen[0].event).toBe("session.state");
		client.close();
	});

	it("rejects pending requests with BridgeTransportError on socket close and reconnects", async () => {
		const client = makeClient();
		serverHandler = () => {}; // never responds
		const ws = await openClient(client);
		const req = client.listSessions();
		ws.close();
		await expect(req).rejects.toBeInstanceOf(BridgeTransportError);
		await vi.waitFor(() => {
			expect(FakeWebSocket.instances.length).toBeGreaterThan(1);
		});
		FakeWebSocket.instances[1].open();
		await vi.waitFor(() => expect(client.isOpen()).toBe(true));
		client.close();
	});

	it("sends idempotencyKey on session.create and prompt.send", async () => {
		const client = makeClient();
		serverHandler = (ws, frame) => ws.respond(frame.id, { sessionId: "s9", state: "starting", accepted: true, seq: 1, queued: false });
		await openClient(client);
		await client.createSession({ name: "x", idempotencyKey: "k-create" });
		await client.sendPrompt("s9", "hello", "k-prompt");
		const frames = FakeWebSocket.instances[0].sent.map((s) => JSON.parse(s) as { method: string; params: Record<string, unknown> });
		expect(frames[0].params.idempotencyKey).toBe("k-create");
		expect(frames[1].params.idempotencyKey).toBe("k-prompt");
		client.close();
	});
});

describe("BridgeSessionStore", () => {
	it("applies replay frames that arrive before the attach response", async () => {
		const client = makeClient();
		serverHandler = (ws, frame) => {
			if (frame.method === "session.attach") {
				// server: replay 1..3 synchronously, then respond
				for (let seq = 1; seq <= 3; seq += 1) {
					ws.recv(ev("s1", seq, "message.delta", seq === 1 ? { kind: "start", role: "assistant" } : seq === 2 ? { kind: "text", delta: "hi" } : { kind: "end", role: "assistant" }, true));
				}
				ws.respond(frame.id, { sessionId: "s1", headSeq: 3, replayed: 3 });
			}
		};
		await openClient(client);
		const store = new BridgeSessionStore(client);
		const projection = await store.attach("s1");
		expect(projection.watermark).toBe(3);
		expect(projection.blocks).toHaveLength(1);
		client.close();
	});

	it("resumes from the watermark on reconnect without dupes or gaps", async () => {
		const client = makeClient();
		const attaches: number[] = [];
		serverHandler = (ws, frame) => {
			if (frame.method === "session.attach") {
				const fromSeq = frame.params.fromSeq as number;
				attaches.push(fromSeq);
				// replay fromSeq+1..4
				for (let seq = fromSeq + 1; seq <= 4; seq += 1) {
					ws.recv(ev("s1", seq, "session.state", { state: seq === 4 ? "idle" : "running" }, true));
				}
				ws.respond(frame.id, { sessionId: "s1", headSeq: 4, replayed: 4 - fromSeq });
			}
		};
		const ws = await openClient(client);
		const store = new BridgeSessionStore(client);
		await store.attach("s1");
		expect(store.getSnapshot("s1")?.watermark).toBe(4);
		// live event
		ws.recv(ev("s1", 5, "session.state", { state: "running" }));
		expect(store.getSnapshot("s1")?.watermark).toBe(5);
		// drop the socket: client reconnects, store re-attaches fromSeq=5
		ws.close();
		await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(1));
		FakeWebSocket.instances[1].open();
		await vi.waitFor(() => expect(attaches.length).toBeGreaterThan(1));
		expect(attaches[1]).toBe(5);
		expect(store.getSnapshot("s1")?.gap).toBeNull();
		client.close();
	});

	it("rebuilds via session.getState and re-attaches at head on event_gap", async () => {
		const client = makeClient();
		const attachParams: Array<number | undefined> = [];
		let attachCount = 0;
		serverHandler = (ws, frame) => {
			if (frame.method === "session.attach") {
				attachCount += 1;
				const fromSeq = frame.params.fromSeq as number | undefined;
				attachParams.push(fromSeq);
				if (attachCount === 2) {
					ws.fail(frame.id, "event_gap", "trimmed", { minAvailable: 31, headSeq: 88 });
					return;
				}
				ws.respond(frame.id, { sessionId: "s1", headSeq: fromSeq ?? 0, replayed: 0 });
			}
			if (frame.method === "session.getState") {
				ws.respond(frame.id, { sessionId: "s1", state: "idle", headSeq: 88 });
			}
		};
		const ws = await openClient(client);
		const store = new BridgeSessionStore(client);
		await store.attach("s1");
		ws.close();
		await vi.waitFor(() => expect(FakeWebSocket.instances.length).toBeGreaterThan(1));
		FakeWebSocket.instances[1].open();
		await vi.waitFor(() => {
			const snap = store.getSnapshot("s1");
			expect(snap?.rebuiltAt).not.toBeNull();
		});
		const snap = store.getSnapshot("s1")!;
		expect(snap.watermark).toBe(88);
		expect(snap.state).toBe("idle");
		// re-attach after the gap used the fresh head, not the stale watermark
		expect(attachParams[2]).toBe(88);
		client.close();
	});

	it("appends an optimistic local user echo on prompt send", async () => {
		const client = makeClient();
		serverHandler = (ws, frame) => {
			if (frame.method === "session.attach") ws.respond(frame.id, { sessionId: "s1", headSeq: 0, replayed: 0 });
			if (frame.method === "prompt.send") ws.respond(frame.id, { accepted: true, seq: 1, queued: false });
		};
		await openClient(client);
		const store = new BridgeSessionStore(client);
		await store.attach("s1");
		const res = await store.sendPrompt("s1", "hello bridge");
		expect(res.seq).toBe(1);
		const blocks = store.getSnapshot("s1")!.blocks;
		expect(blocks).toHaveLength(1);
		expect(blocks[0]).toMatchObject({ kind: "message", role: "user", text: "hello bridge", local: true });
		client.close();
	});
});
