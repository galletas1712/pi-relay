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
};

export type ConnectionStatus = "connecting" | "open" | "closed" | "error";

type EventHandler = (event: EventFrame) => void;
type StatusHandler = (status: ConnectionStatus) => void;

export interface RpcClient {
	connect(): Promise<void>;
	close(): void;
	isOpen(): boolean;
	onEvent(handler: EventHandler): () => void;
	onStatus(handler: StatusHandler): () => void;
	request<T>(method: string, params?: Record<string, unknown>): Promise<T>;
}

export class AgentRpcClient implements RpcClient {
	private ws: WebSocket | null = null;
	private nextId = 1;
	private pending = new Map<string, Pending>();
	private eventHandlers = new Set<EventHandler>();
	private statusHandlers = new Set<StatusHandler>();
	private openPromise: Promise<void> | null = null;
	private closedByUser = false;
	private reconnectTimer: number | null = null;

	constructor(private readonly url: string) {}

	connect(): Promise<void> {
		if (this.ws?.readyState === WebSocket.OPEN) return Promise.resolve();
		if (this.openPromise) return this.openPromise;

		this.closedByUser = false;
		if (this.reconnectTimer !== null) {
			window.clearTimeout(this.reconnectTimer);
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
					reject(new Error(`failed to connect ${this.url}`));
				},
				{ once: true }
			);
		});

		ws.addEventListener("message", (message) => {
			if (this.ws === ws) this.handleMessage(message);
		});
		ws.addEventListener("close", () => {
			if (this.ws !== ws) return;
			this.emitStatus("closed");
			this.openPromise = null;
			this.ws = null;
			for (const pending of this.pending.values()) {
				pending.reject(new Error("websocket closed"));
			}
			this.pending.clear();
			if (!this.closedByUser) this.scheduleReconnect();
		});

		return this.openPromise;
	}

	close(): void {
		this.closedByUser = true;
		if (this.reconnectTimer !== null) {
			window.clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		for (const pending of this.pending.values()) {
			pending.reject(new Error("websocket closed"));
		}
		this.pending.clear();
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

	async request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
		await this.connect();
		const ws = this.ws;
		if (!ws || ws.readyState !== WebSocket.OPEN) {
			throw new Error("websocket is not open");
		}
		const id = `web_${this.nextId++}`;
		const promise = new Promise<T>((resolve, reject) => {
			this.pending.set(id, {
				resolve: (value) => resolve(value as T),
				reject
			});
		});
		ws.send(JSON.stringify({ id, method, params }));
		return promise;
	}

	private handleMessage(message: MessageEvent<string>): void {
		let data: RpcResponse<unknown> | EventFrame;
		try {
			data = JSON.parse(message.data) as RpcResponse<unknown> | EventFrame;
		} catch {
			this.emitStatus("error");
			return;
		}
		if ("ok" in data) {
			const pending = this.pending.get(data.id);
			if (!pending) return;
			this.pending.delete(data.id);
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

	private scheduleReconnect(): void {
		if (this.reconnectTimer !== null) return;
		this.reconnectTimer = window.setTimeout(() => {
			this.reconnectTimer = null;
			void this.connect().catch(() => {
				if (!this.closedByUser) this.scheduleReconnect();
			});
		}, 750);
	}
}

export function defaultWsUrl(): string {
	const configured = import.meta.env.VITE_PI_AGENT_WS as string | undefined;
	if (configured) return configured;
	return "ws://127.0.0.1:8787";
}
