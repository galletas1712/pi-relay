import { PassThrough } from "node:stream";
import type { ImageContent, Model } from "@pi-relay/ai";
import type { ThinkingLevel } from "@pi-relay/agent-core";
import { describe, expect, test } from "vitest";
import { RpcClient } from "../../../src/client/rpc/client.js";
import { RpcServer } from "../../../src/client/rpc/server.js";
import type { ModelCycleResult, PromptOptions } from "../../../src/core/agent-session.js";
import type {
	AuthStatus,
	Client,
	OpenSessionOptions,
	ResumeOptions,
	SessionEvent,
	SessionHandle,
	SessionState,
	SessionSummary,
} from "../../../src/client/types.js";

/**
 * Fake session handle that records method calls and lets tests push events
 * into the iterator. Used as the server-side Client implementation so the
 * round-trip focuses on the wire transport rather than agent behavior.
 */
class FakeSessionHandle implements SessionHandle {
	readonly calls: Array<{ method: string; args: unknown[] }> = [];
	private pending: Array<(value: IteratorResult<SessionEvent>) => void> = [];
	private buffer: SessionEvent[] = [];
	private closed = false;
	readonly aborted: { called: boolean } = { called: false };

	constructor(readonly id: string) {}

	get events(): AsyncIterable<SessionEvent> {
		return { [Symbol.asyncIterator]: () => this.iterator() };
	}

	emit(event: SessionEvent): void {
		if (this.closed) return;
		const waiter = this.pending.shift();
		if (waiter) {
			waiter({ value: event, done: false });
			return;
		}
		this.buffer.push(event);
	}

	private iterator(): AsyncIterableIterator<SessionEvent> {
		return {
			[Symbol.asyncIterator]() {
				return this;
			},
			next: (): Promise<IteratorResult<SessionEvent>> => {
				if (this.buffer.length > 0) {
					return Promise.resolve({ value: this.buffer.shift() as SessionEvent, done: false });
				}
				if (this.closed) {
					return Promise.resolve({ value: undefined as unknown as SessionEvent, done: true });
				}
				return new Promise((resolve) => this.pending.push(resolve));
			},
			return: (): Promise<IteratorResult<SessionEvent>> => {
				this.closed = true;
				return Promise.resolve({ value: undefined as unknown as SessionEvent, done: true });
			},
		};
	}

	async prompt(text: string, opts?: PromptOptions): Promise<void> {
		this.calls.push({ method: "prompt", args: [text, opts] });
	}

	async steer(text: string, images?: ImageContent[]): Promise<void> {
		this.calls.push({ method: "steer", args: [text, images] });
	}

	async followUp(text: string, images?: ImageContent[]): Promise<void> {
		this.calls.push({ method: "followUp", args: [text, images] });
	}

	async abort(): Promise<void> {
		this.aborted.called = true;
		this.calls.push({ method: "abort", args: [] });
	}

	async switchModel(model: Model<any>): Promise<void> {
		this.calls.push({ method: "switchModel", args: [model] });
	}

	async cycleModel(direction: "forward" | "backward"): Promise<ModelCycleResult | undefined> {
		this.calls.push({ method: "cycleModel", args: [direction] });
		return undefined;
	}

	async cycleThinking(): Promise<ThinkingLevel> {
		this.calls.push({ method: "cycleThinking", args: [] });
		return "high";
	}

	async getState(): Promise<SessionState> {
		this.calls.push({ method: "getState", args: [] });
		return {
			id: this.id,
			cwd: "/tmp/fake",
			sessionFile: undefined,
			model: undefined,
			thinkingLevel: "medium",
			isStreaming: false,
			isCompacting: false,
			isBashRunning: false,
			autoCompactionEnabled: true,
			steeringMode: "all",
			followUpMode: "all",
			scopedModels: [],
			stats: {
				sessionFile: undefined,
				sessionId: this.id,
				userMessages: 0,
				assistantMessages: 0,
				toolCalls: 0,
				toolResults: 0,
				totalMessages: 0,
				tokens: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
				cost: 0,
			},
		};
	}

	async close(): Promise<void> {
		this.closed = true;
		while (this.pending.length > 0) {
			const waiter = this.pending.shift();
			waiter?.({ value: undefined as unknown as SessionEvent, done: true });
		}
	}
}

class FakeClient implements Client {
	readonly created: FakeSessionHandle[] = [];
	readonly disposed: { called: boolean } = { called: false };
	private counter = 0;
	private current: FakeSessionHandle | undefined;
	readonly authEntries: ReadonlyArray<{ provider: string; hasCredential: boolean }> = [
		{ provider: "anthropic", hasCredential: true },
	];

	readonly sessions = {
		open: async (_opts?: OpenSessionOptions): Promise<SessionHandle> => {
			const handle = new FakeSessionHandle(`fake-${++this.counter}`);
			this.created.push(handle);
			this.current = handle;
			return handle;
		},
		resume: async (_path: string, _opts?: ResumeOptions): Promise<SessionHandle> => {
			const handle = new FakeSessionHandle(`fake-resumed-${++this.counter}`);
			this.created.push(handle);
			this.current = handle;
			return handle;
		},
		list: async (): Promise<SessionSummary[]> => [
			{
				path: "/tmp/fake/session.jsonl",
				id: "fake-session-id",
				cwd: "/tmp/fake",
				name: "fake",
				created: new Date("2026-01-01T00:00:00Z"),
				modified: new Date("2026-01-02T00:00:00Z"),
				messageCount: 2,
				firstMessage: "hi",
				allMessagesText: "hi / hello",
			},
		],
	};

	session(): SessionHandle {
		if (!this.current) {
			throw new Error("no current session");
		}
		return this.current;
	}

	readonly models = {
		list: async (): Promise<Model<any>[]> => [],
		listAvailable: async (): Promise<Model<any>[]> => [],
	};

	readonly auth = {
		login: async (_provider: string): Promise<void> => {},
		logout: async (_provider: string): Promise<void> => {},
		status: async (): Promise<AuthStatus> => this.authEntries,
	};

	async dispose(): Promise<void> {
		this.disposed.called = true;
	}

	currentHandle(): FakeSessionHandle {
		if (!this.current) throw new Error("no current session");
		return this.current;
	}
}

function pair(): { serverIO: { input: PassThrough; output: PassThrough }; clientIO: { input: PassThrough; output: PassThrough } } {
	const clientToServer = new PassThrough();
	const serverToClient = new PassThrough();
	return {
		serverIO: { input: clientToServer, output: serverToClient },
		clientIO: { input: serverToClient, output: clientToServer },
	};
}

describe("RpcServer/RpcClient round-trip", () => {
	test("opens a session, sends a prompt, streams events, closes", async () => {
		const { serverIO, clientIO } = pair();
		const fakeClient = new FakeClient();
		const server = new RpcServer(fakeClient, serverIO);
		const listening = server.listen();
		const client = new RpcClient(clientIO);

		const handle = await client.sessions.open();
		expect(fakeClient.created).toHaveLength(1);
		expect(handle.id).toBe("fake-1");

		const collected: SessionEvent[] = [];
		const consumerDone = (async () => {
			for await (const event of handle.events) {
				collected.push(event);
				if (event.type === "agent_end") break;
			}
		})();

		await handle.prompt("hello there");
		expect(fakeClient.currentHandle().calls).toEqual([
			{ method: "prompt", args: ["hello there", undefined] },
		]);

		// Server-side emission flows through the wire back into the RpcClient iterator.
		fakeClient.currentHandle().emit({ type: "agent_start" });
		fakeClient.currentHandle().emit({ type: "agent_end", messages: [] });
		await consumerDone;

		expect(collected.map((e) => e.type)).toEqual(["agent_start", "agent_end"]);

		await handle.abort();
		expect(fakeClient.currentHandle().aborted.called).toBe(true);

		await handle.close();
		await client.dispose();
		expect(fakeClient.disposed.called).toBe(true);

		clientIO.output.end();
		serverIO.input.end();
		await listening;
	});

	test("sessions.list rehydrates Date fields", async () => {
		const { serverIO, clientIO } = pair();
		const fakeClient = new FakeClient();
		const server = new RpcServer(fakeClient, serverIO);
		const listening = server.listen();
		const client = new RpcClient(clientIO);

		const summaries = await client.sessions.list();
		expect(summaries).toHaveLength(1);
		expect(summaries[0].created).toBeInstanceOf(Date);
		expect(summaries[0].modified).toBeInstanceOf(Date);
		expect(summaries[0].created.toISOString()).toBe("2026-01-01T00:00:00.000Z");

		await client.dispose();
		clientIO.output.end();
		serverIO.input.end();
		await listening;
	});

	test("surfaces server-side errors as rejected promises", async () => {
		const { serverIO, clientIO } = pair();
		const fakeClient = new FakeClient();
		const server = new RpcServer(fakeClient, serverIO);
		const listening = server.listen();
		const client = new RpcClient(clientIO);

		// No session opened -> server throws, error frame round-trips.
		await expect(
			client.call("session.prompt", { sessionId: "missing", text: "hi" }),
		).rejects.toThrow(/session not open: missing/);

		await client.dispose();
		clientIO.output.end();
		serverIO.input.end();
		await listening;
	});

	test("auth.status round-trips", async () => {
		const { serverIO, clientIO } = pair();
		const fakeClient = new FakeClient();
		const server = new RpcServer(fakeClient, serverIO);
		const listening = server.listen();
		const client = new RpcClient(clientIO);

		const status = await client.auth.status();
		expect(status).toEqual([{ provider: "anthropic", hasCredential: true }]);

		await client.dispose();
		clientIO.output.end();
		serverIO.input.end();
		await listening;
	});

	test("cancel frames are ignored without crashing", async () => {
		const { serverIO, clientIO } = pair();
		const fakeClient = new FakeClient();
		const server = new RpcServer(fakeClient, serverIO);
		const listening = server.listen();
		const client = new RpcClient(clientIO);

		// Hand-craft a cancel frame; server must not blow up.
		clientIO.output.write(`${JSON.stringify({ type: "cancel", id: 9999 })}\n`);
		// Follow-up call still works.
		const handle = await client.sessions.open();
		expect(handle.id).toBe("fake-1");

		await client.dispose();
		clientIO.output.end();
		serverIO.input.end();
		await listening;
	});
});
