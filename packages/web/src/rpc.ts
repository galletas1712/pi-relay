import { perfEnabled, perfLog, perfNow } from "./perf.ts";
import type { EventFrame } from "./types.ts";

interface RpcResponse<T> {
	id: string;
	ok: boolean;
	result?: T;
	error?: {
		code: string;
		message: string;
		data?: unknown;
	};
}

type Pending = {
	resolve: (value: unknown) => void;
	reject: (error: Error) => void;
	method: string;
	startedAt: number;
};

export type ConnectionStatus = "connecting" | "open" | "closed" | "error";

type EventHandler = (event: EventFrame) => void;
type StatusHandler = (status: ConnectionStatus) => void;

export interface RpcClient {
	connect(): Promise<void>;
	reconnect(): Promise<void>;
	close(): void;
	isOpen(): boolean;
	onEvent(handler: EventHandler): () => void;
	onStatus(handler: StatusHandler): () => void;
	request<T>(
		method: string,
		params?: Record<string, unknown>,
		options?: RpcRequestOptions,
	): Promise<T>;
}

export interface RpcRequestOptions {
	timeoutMs?: number;
}

export const RPC_REQUEST_TIMEOUT_MS = 15_000;
export const SESSION_START_REQUEST_TIMEOUT_MS = 300_000;

export class AgentRpcClient implements RpcClient {
	private ws: WebSocket | null = null;
	private nextId = 1;
	private pending = new Map<string, Pending>();
	private eventHandlers = new Set<EventHandler>();
	private statusHandlers = new Set<StatusHandler>();
	private openPromise: Promise<void> | null = null;
	private rejectOpenPromise: ((error: Error) => void) | null = null;
	private closedByUser = false;
	private reconnectTimer: ReturnType<typeof setTimeout> | null = null;

	constructor(private readonly url: string) {}

	connect(): Promise<void> {
		if (this.ws?.readyState === WebSocket.OPEN) return Promise.resolve();
		if (this.openPromise) return this.openPromise;

		this.closedByUser = false;
		if (this.reconnectTimer !== null) {
			globalThis.clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		this.emitStatus("connecting");
		const ws = new WebSocket(this.url);
		this.ws = ws;
		this.openPromise = new Promise((resolve, reject) => {
			ws.addEventListener(
				"open",
				() => {
					if (this.ws !== ws) return;
					this.emitStatus("open");
					this.openPromise = null;
					this.rejectOpenPromise = null;
					resolve();
				},
				{ once: true }
			);
			ws.addEventListener(
				"error",
				() => {
					if (this.ws !== ws) return;
					this.emitStatus("error");
					this.openPromise = null;
					this.rejectOpenPromise = null;
					reject(new Error(`failed to connect ${this.url}`));
				},
				{ once: true }
			);
			this.rejectOpenPromise = reject;
		});

		ws.addEventListener("message", (message) => {
			if (this.ws === ws) this.handleMessage(message);
		});
		ws.addEventListener("close", () => {
			if (this.ws !== ws) return;
			this.emitStatus("closed");
			this.openPromise = null;
			this.rejectOpenPromise = null;
			this.ws = null;
			this.rejectPending("websocket closed");
			if (!this.closedByUser) this.scheduleReconnect();
		});

		return this.openPromise;
	}

	reconnect(): Promise<void> {
		// Join an automatic or user-initiated connection attempt instead of
		// replacing its socket and creating a competing reconnect loop.
		if (this.openPromise) return this.openPromise;
		this.closedByUser = false;
		if (this.reconnectTimer !== null) {
			globalThis.clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		this.rejectConnecting("websocket reconnecting");
		this.rejectPending("websocket reconnecting");
		const ws = this.ws;
		this.ws = null;
		ws?.close();
		return this.connect();
	}

	close(): void {
		this.closedByUser = true;
		if (this.reconnectTimer !== null) {
			globalThis.clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		this.rejectConnecting("websocket closed");
		this.rejectPending("websocket closed");
		const ws = this.ws;
		this.ws = null;
		ws?.close();
	}

	isOpen(): boolean {
		return this.ws?.readyState === WebSocket.OPEN;
	}

	onEvent(handler: EventHandler): () => void {
		this.eventHandlers.add(handler);
		return () => this.eventHandlers.delete(handler);
	}

	onStatus(handler: StatusHandler): () => void {
		this.statusHandlers.add(handler);
		return () => this.statusHandlers.delete(handler);
	}

	async request<T>(
		method: string,
		params: Record<string, unknown> = {},
		options?: RpcRequestOptions,
	): Promise<T> {
		const timeoutMs = options?.timeoutMs ?? RPC_REQUEST_TIMEOUT_MS;
		if (!Number.isFinite(timeoutMs) || timeoutMs <= 0) {
			throw new Error("RPC request timeout must be a positive finite number");
		}
		await this.connect();
		const ws = this.ws;
		if (!ws || ws.readyState !== WebSocket.OPEN) {
			throw new Error("websocket is not open");
		}
		const id = `web_${this.nextId++}`;
		const promise = new Promise<T>((resolve, reject) => {
			this.pending.set(id, {
				resolve: (value) => resolve(value as T),
				reject,
				method,
				startedAt: perfNow()
			});
		});
		ws.send(JSON.stringify({ id, method, params }));
		return this.withRequestTimeout(id, promise, timeoutMs);
	}

	private handleMessage(message: MessageEvent<string>): void {
		let data: RpcResponse<unknown> | EventFrame;
		const shouldLogPerf = perfEnabled();
		const receivedAt = shouldLogPerf ? perfNow() : 0;
		try {
			data = JSON.parse(message.data) as RpcResponse<unknown> | EventFrame;
			if (shouldLogPerf) {
				perfLog("rpc message parsed", {
					bytes: message.data.length,
					parseMs: Math.round(perfNow() - receivedAt),
					kind: "ok" in data ? "response" : "event"
				});
			}
		} catch {
			this.emitStatus("error");
			return;
		}
		if ("ok" in data) {
			const pending = this.pending.get(data.id);
			if (!pending) return;
			this.pending.delete(data.id);
			if (shouldLogPerf) {
				perfLog("rpc response", {
					method: pending.method,
					ok: data.ok,
					roundtripMs: Math.round(perfNow() - pending.startedAt),
					pending: this.pending.size,
				});
			}
			if (data.ok) {
				pending.resolve(data.result);
			} else {
				const code = data.error?.code ?? "rpc_error";
				const detail = data.error?.message ?? "request failed";
				pending.reject(new Error(`${code}: ${detail}`));
			}
			return;
		}
		for (const handler of this.eventHandlers) handler(data);
	}

	private emitStatus(status: ConnectionStatus): void {
		for (const handler of this.statusHandlers) handler(status);
	}

	private rejectPending(message: string): void {
		for (const pending of this.pending.values()) {
			pending.reject(new Error(message));
		}
		this.pending.clear();
	}

	private rejectConnecting(message: string): void {
		const reject = this.rejectOpenPromise;
		this.openPromise = null;
		this.rejectOpenPromise = null;
		reject?.(new Error(message));
	}

	private withRequestTimeout<T>(
		id: string,
		promise: Promise<T>,
		timeoutMs: number,
	): Promise<T> {
		return new Promise<T>((resolve, reject) => {
			const timer = globalThis.setTimeout(() => {
				const pending = this.pending.get(id);
				if (!pending) return;
				this.pending.delete(id);
				const ws = this.ws;
				this.ws = null;
				ws?.close();
				const error = new Error("websocket request timed out");
				this.rejectPending("websocket closed");
				pending.reject(error);
				reject(error);
				this.emitStatus("closed");
				if (!this.closedByUser) this.scheduleReconnect();
			}, timeoutMs);
			promise.then(
				(value) => {
					globalThis.clearTimeout(timer);
					resolve(value);
				},
				(error) => {
					globalThis.clearTimeout(timer);
					reject(error);
				},
			);
		});
	}

	private scheduleReconnect(): void {
		if (this.reconnectTimer !== null) return;
		this.reconnectTimer = globalThis.setTimeout(() => {
			this.reconnectTimer = null;
			void this.connect().catch(() => {
				if (!this.closedByUser) this.scheduleReconnect();
			});
		}, 750);
	}
}

export function defaultWsUrl(): string {
	const configured = import.meta.env.VITE_PI_AGENT_WS as string | undefined;
	return resolveWsUrl(configured, window.location);
}

export function resolveWsUrl(
	configured: string | undefined,
	location: Pick<Location, "hostname" | "port" | "protocol">,
): string {
	if (configured?.trim()) return configured;
	if (isLoopbackHost(location.hostname)) return "ws://127.0.0.1:8787";
	const proto = location.protocol === "https:" ? "wss:" : "ws:";
	const port = location.port ? `:${location.port}` : "";
	return `${proto}//${location.hostname}${port}/ws`;
}

function isLoopbackHost(hostname: string): boolean {
	return hostname === "127.0.0.1" || hostname === "localhost" || hostname === "::1" || hostname === "[::1]";
}
