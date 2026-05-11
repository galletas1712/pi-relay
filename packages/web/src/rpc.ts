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

type EventHandler = (event: EventFrame) => void;
type StatusHandler = (status: "connecting" | "open" | "closed" | "error") => void;

export class AgentRpcClient {
	private ws: WebSocket | null = null;
	private nextId = 1;
	private pending = new Map<string, Pending>();
	private eventHandlers = new Set<EventHandler>();
	private statusHandlers = new Set<StatusHandler>();
	private openPromise: Promise<void> | null = null;

	constructor(private readonly url: string) {}

	connect(): Promise<void> {
		if (this.ws?.readyState === WebSocket.OPEN) return Promise.resolve();
		if (this.openPromise) return this.openPromise;

		this.emitStatus("connecting");
		this.ws = new WebSocket(this.url);
		this.openPromise = new Promise((resolve, reject) => {
			const ws = this.ws;
			if (!ws) return reject(new Error("websocket was not created"));

			ws.addEventListener(
				"open",
				() => {
					this.emitStatus("open");
					this.openPromise = null;
					resolve();
				},
				{ once: true }
			);
			ws.addEventListener(
				"error",
				() => {
					this.emitStatus("error");
					this.openPromise = null;
					reject(new Error(`failed to connect ${this.url}`));
				},
				{ once: true }
			);
		});

		this.ws.addEventListener("message", (message) => this.handleMessage(message));
		this.ws.addEventListener("close", () => {
			this.emitStatus("closed");
			this.openPromise = null;
			for (const pending of this.pending.values()) {
				pending.reject(new Error("websocket closed"));
			}
			this.pending.clear();
		});

		return this.openPromise;
	}

	close(): void {
		this.ws?.close();
		this.ws = null;
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
		const data = JSON.parse(message.data) as RpcResponse<unknown> | EventFrame;
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

	private emitStatus(status: "connecting" | "open" | "closed" | "error"): void {
		for (const handler of this.statusHandlers) handler(status);
	}
}

export function defaultWsUrl(): string {
	const configured = import.meta.env.VITE_PI_AGENT_WS as string | undefined;
	if (configured) return configured;
	return "ws://127.0.0.1:8787";
}
