import type { ImageContent, Model } from "@pi-relay/ai";
import type { ThinkingLevel } from "@pi-relay/agent-core";
import type { ModelCycleResult, PromptOptions } from "../../core/agent-session.js";
import type {
	AuthStatus,
	Client,
	OpenSessionOptions,
	ResumeOptions,
	SessionEvent,
	SessionHandle,
	SessionState,
	SessionSummary,
} from "../types.js";
import { readLines } from "./framing.js";
import type { NodeIO } from "./server.js";
import {
	decodeMessage,
	encodeMessage,
	type MethodMap,
	type RpcCallId,
	type RpcMethod,
} from "./wire.js";

interface PendingCall {
	resolve: (value: unknown) => void;
	reject: (error: Error) => void;
}

/**
 * Single-consumer async event queue used by the RpcClient's session handles.
 *
 * Event frames arrive off the shared transport indexed by sessionId; this queue
 * lets each SessionHandle expose an AsyncIterable that blocks between events
 * and completes cleanly when the session is closed.
 */
class EventQueue {
	private buffered: SessionEvent[] = [];
	private waiters: Array<(result: IteratorResult<SessionEvent>) => void> = [];
	private closed = false;

	push(event: SessionEvent): void {
		if (this.closed) return;
		const waiter = this.waiters.shift();
		if (waiter) {
			waiter({ value: event, done: false });
			return;
		}
		this.buffered.push(event);
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		while (this.waiters.length > 0) {
			const waiter = this.waiters.shift();
			waiter?.({ value: undefined as unknown as SessionEvent, done: true });
		}
	}

	iterator(): AsyncIterableIterator<SessionEvent> {
		return {
			[Symbol.asyncIterator]() {
				return this;
			},
			next: (): Promise<IteratorResult<SessionEvent>> => {
				if (this.buffered.length > 0) {
					const value = this.buffered.shift() as SessionEvent;
					return Promise.resolve({ value, done: false });
				}
				if (this.closed) {
					return Promise.resolve({ value: undefined as unknown as SessionEvent, done: true });
				}
				return new Promise((resolve) => this.waiters.push(resolve));
			},
			return: (): Promise<IteratorResult<SessionEvent>> => {
				return Promise.resolve({ value: undefined as unknown as SessionEvent, done: true });
			},
		};
	}
}

class RpcSessionHandle implements SessionHandle {
	private queue = new EventQueue();

	constructor(
		readonly id: string,
		private readonly rpc: RpcClient,
	) {}

	get events(): AsyncIterable<SessionEvent> {
		return { [Symbol.asyncIterator]: () => this.queue.iterator() };
	}

	/** Called by RpcClient when an event frame for this session arrives. */
	pushEvent(event: SessionEvent): void {
		this.queue.push(event);
	}

	async prompt(text: string, opts?: PromptOptions): Promise<void> {
		await this.rpc.call("session.prompt", { sessionId: this.id, text, opts });
	}

	async steer(text: string, images?: ImageContent[]): Promise<void> {
		await this.rpc.call("session.steer", { sessionId: this.id, text, images });
	}

	async followUp(text: string, images?: ImageContent[]): Promise<void> {
		await this.rpc.call("session.followUp", { sessionId: this.id, text, images });
	}

	async abort(): Promise<void> {
		await this.rpc.call("session.abort", { sessionId: this.id });
	}

	async switchModel(model: Model<any>): Promise<void> {
		await this.rpc.call("session.switchModel", { sessionId: this.id, model });
	}

	async cycleModel(direction: "forward" | "backward"): Promise<ModelCycleResult | undefined> {
		const result = await this.rpc.call("session.cycleModel", { sessionId: this.id, direction });
		return result ?? undefined;
	}

	async cycleThinking(): Promise<ThinkingLevel> {
		const { level } = await this.rpc.call("session.cycleThinking", { sessionId: this.id });
		return level;
	}

	async getState(): Promise<SessionState> {
		return this.rpc.call("session.getState", { sessionId: this.id });
	}

	async close(): Promise<void> {
		await this.rpc.call("session.close", { sessionId: this.id });
		this.rpc.detachSession(this.id);
		this.queue.close();
	}
}

/**
 * Out-of-process Client implementation.
 *
 * Serializes every method on the `Client` surface as an RPC call over a duplex
 * byte stream. Session events are routed off the shared transport back into
 * per-session AsyncIterables.
 */
export class RpcClient implements Client {
	private readonly detachInput: () => void;
	private readonly pending = new Map<RpcCallId, PendingCall>();
	private readonly sessionHandles = new Map<string, RpcSessionHandle>();
	private nextCallId = 1;
	private currentSessionId: string | undefined;
	private disposed = false;

	constructor(private readonly io: NodeIO) {
		this.detachInput = readLines(io.input, (line) => {
			if (line.trim().length === 0) return;
			this.handleLine(line);
		});
	}

	session(): SessionHandle {
		if (!this.currentSessionId) {
			throw new Error("No active session; call sessions.open() or sessions.resume() first");
		}
		const handle = this.sessionHandles.get(this.currentSessionId);
		if (!handle) {
			throw new Error(`Session ${this.currentSessionId} is closed`);
		}
		return handle;
	}

	readonly sessions = {
		open: async (opts?: OpenSessionOptions): Promise<SessionHandle> => {
			const result = await this.call("sessions.open", {
				parentSession: opts?.parentSession,
			});
			return this.adoptSession(result.sessionId);
		},
		resume: async (sessionPath: string, opts?: ResumeOptions): Promise<SessionHandle> => {
			const result = await this.call("sessions.resume", {
				sessionPath,
				cwdOverride: opts?.cwdOverride,
			});
			return this.adoptSession(result.sessionId);
		},
		list: async (): Promise<SessionSummary[]> => {
			const { sessions } = await this.call("sessions.list", {});
			return sessions.map((s) => ({
				...s,
				created: new Date(s.created),
				modified: new Date(s.modified),
			}));
		},
	};

	readonly models = {
		list: async (): Promise<Model<any>[]> => {
			const { models } = await this.call("models.list", {});
			return models;
		},
		listAvailable: async (): Promise<Model<any>[]> => {
			const { models } = await this.call("models.listAvailable", {});
			return models;
		},
	};

	readonly auth = {
		login: async (provider: string): Promise<void> => {
			await this.call("auth.login", { provider });
		},
		logout: async (provider: string): Promise<void> => {
			await this.call("auth.logout", { provider });
		},
		status: async (): Promise<AuthStatus> => {
			const { entries } = await this.call("auth.status", {});
			return entries;
		},
	};

	async dispose(): Promise<void> {
		if (this.disposed) return;
		try {
			await this.call("dispose", {});
		} catch {
			// Transport may already be torn down; swallow.
		}
		this.disposed = true;
		for (const handle of this.sessionHandles.values()) {
			(handle as unknown as { queue: { close(): void } }).queue.close();
		}
		this.sessionHandles.clear();
		this.detachInput();
		for (const pending of this.pending.values()) {
			pending.reject(new Error("RpcClient disposed"));
		}
		this.pending.clear();
	}

	/**
	 * Low-level RPC call. Used by this class and by RpcSessionHandle.
	 */
	call<M extends RpcMethod>(method: M, params: MethodMap[M]["params"]): Promise<MethodMap[M]["result"]> {
		if (this.disposed) {
			return Promise.reject(new Error("RpcClient disposed"));
		}
		const id = this.nextCallId++;
		return new Promise<MethodMap[M]["result"]>((resolve, reject) => {
			this.pending.set(id, {
				resolve: (value) => resolve(value as MethodMap[M]["result"]),
				reject,
			});
			this.io.output.write(encodeMessage({ type: "call", id, method, params }));
		});
	}

	/** Invoked by RpcSessionHandle.close() to drop local tracking. */
	detachSession(sessionId: string): void {
		this.sessionHandles.delete(sessionId);
		if (this.currentSessionId === sessionId) {
			this.currentSessionId = undefined;
		}
	}

	private adoptSession(sessionId: string): RpcSessionHandle {
		let handle = this.sessionHandles.get(sessionId);
		if (!handle) {
			handle = new RpcSessionHandle(sessionId, this);
			this.sessionHandles.set(sessionId, handle);
		}
		this.currentSessionId = sessionId;
		return handle;
	}

	private handleLine(line: string): void {
		let message;
		try {
			message = decodeMessage(line);
		} catch {
			return;
		}
		if (message.type === "event") {
			const handle = this.sessionHandles.get(message.sessionId);
			handle?.pushEvent(message.event);
			return;
		}
		if (message.type === "result") {
			const pending = this.pending.get(message.id);
			if (!pending) return;
			this.pending.delete(message.id);
			pending.resolve(message.value);
			return;
		}
		if (message.type === "error") {
			const pending = this.pending.get(message.id);
			if (!pending) return;
			this.pending.delete(message.id);
			pending.reject(new Error(message.error.message));
			return;
		}
		// call/cancel frames are server-bound; ignore on the client.
	}
}
