// Typed WebSocket client for bridge contract v0 (packages/bridge/README.md).
// Mirrors the disciplines of the legacy rpc.ts layer — per-request timeouts,
// typed errors, pending-request rejection on reconnect — against the slimmer
// bridge wire: {id, method, params} -> {id, result} | {id, error:{code,...}},
// plus server->client event envelopes carrying a per-session monotonic seq.
//
// The WebSocket constructor is injectable: browsers use the native class
// (same-origin via the vite dev proxy, which injects the upgrade headers a
// browser cannot set); Node tests inject the `ws` package with the bearer
// token and Origin the bridge demands at upgrade time.
import type {
	CommandAckResult,
	CommsListResult,
	ContractEventEnvelope,
	EventGapErrorData,
	ModelsListResult,
	PromptSendResult,
	ReplExecuteResult,
	SessionAttachResult,
	SessionCreateParams,
	SessionCreateResult,
	SessionListResult,
	ProjectListResult,
	SessionState,
	SetModelResult,
	SetThinkingLevelResult,
	SubagentTranscriptResult,
	SubagentTreeResult,
} from "./types.ts";

export interface BridgeWebSocket {
	readonly readyState: number;
	send(data: string): void;
	close(): void;
	addEventListener(
		type: "open" | "message" | "close" | "error",
		listener: (event: never) => void,
		options?: { once?: boolean },
	): void;
}

export type BridgeWebSocketFactory = (url: string) => BridgeWebSocket;

const defaultWebSocketFactory: BridgeWebSocketFactory = (url) =>
	new WebSocket(url) as unknown as BridgeWebSocket;

/** Definite, server-returned failure: the operation did not happen (typed code). */
export class BridgeRequestError extends Error {
	constructor(
		readonly code: string,
		readonly detail: string,
		readonly data?: unknown,
	) {
		super(`${code}: ${detail}`);
		this.name = "BridgeRequestError";
	}
}

/**
 * The socket dropped or timed out after send, so the request may have been
 * applied. Callers retry with the same idempotency key (session.create /
 * prompt.send) or reconcile via session.getState before retrying.
 */
export class BridgeTransportError extends Error {
	constructor(message: string) {
		super(message);
		this.name = "BridgeTransportError";
	}
}

export function isBridgeErrorCode(error: unknown, code: string): boolean {
	return error instanceof BridgeRequestError && error.code === code;
}

export function eventGapData(error: unknown): EventGapErrorData | null {
	if (!isBridgeErrorCode(error, "event_gap")) return null;
	const data = (error as BridgeRequestError).data as Partial<EventGapErrorData> | undefined;
	if (typeof data?.minAvailable !== "number" || typeof data?.headSeq !== "number") return null;
	return { minAvailable: data.minAvailable, headSeq: data.headSeq };
}

export type BridgeConnectionStatus = "connecting" | "open" | "closed" | "error";

type EventHandler = (event: ContractEventEnvelope) => void;
type StatusHandler = (status: BridgeConnectionStatus) => void;

interface Pending {
	resolve: (value: unknown) => void;
	reject: (error: Error) => void;
	method: string;
}

export interface BridgeClientOptions {
	webSocketFactory?: BridgeWebSocketFactory;
	/** Per-request timeout. Prompt acks are fast (the bridge journals before
	 * send); 15 s matches the legacy layer's RPC budget. */
	requestTimeoutMs?: number;
	reconnectDelayMs?: number;
}

const WS_OPEN = 1;

export class BridgeClient {
	private ws: BridgeWebSocket | null = null;
	private nextId = 1;
	private readonly pending = new Map<string, Pending>();
	private readonly eventHandlers = new Set<EventHandler>();
	private readonly statusHandlers = new Set<StatusHandler>();
	private openPromise: Promise<void> | null = null;
	private rejectOpenPromise: ((error: Error) => void) | null = null;
	private disposed = false;
	private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
	private readonly wsFactory: BridgeWebSocketFactory;
	private readonly requestTimeoutMs: number;
	private readonly reconnectDelayMs: number;

	constructor(
		readonly url: string,
		options: BridgeClientOptions = {},
	) {
		this.wsFactory = options.webSocketFactory ?? defaultWebSocketFactory;
		this.requestTimeoutMs = options.requestTimeoutMs ?? 15_000;
		this.reconnectDelayMs = options.reconnectDelayMs ?? 750;
	}

	connect(): Promise<void> {
		if (this.disposed) return Promise.reject(new Error("bridge client is disposed"));
		if (this.ws?.readyState === WS_OPEN) return Promise.resolve();
		if (this.openPromise) return this.openPromise;

		if (this.reconnectTimer !== null) {
			globalThis.clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		this.emitStatus("connecting");
		const ws = this.wsFactory(this.url);
		this.ws = ws;
		this.openPromise = new Promise<void>((resolve, reject) => {
			ws.addEventListener("open", () => {
				if (this.ws !== ws) return;
				this.emitStatus("open");
				this.openPromise = null;
				this.rejectOpenPromise = null;
				resolve();
			}, { once: true });
			ws.addEventListener("error", () => {
				if (this.ws !== ws) return;
				this.emitStatus("error");
			}, { once: true });
			this.rejectOpenPromise = reject;
		});

		ws.addEventListener("message", (event) => {
			if (this.ws === ws) this.handleMessage((event as MessageEvent<string>).data);
		});
		ws.addEventListener("close", () => {
			if (this.ws !== ws) return;
			this.emitStatus("closed");
			this.rejectConnecting(new BridgeTransportError("bridge websocket closed"));
			this.ws = null;
			this.rejectPending(new BridgeTransportError("bridge websocket closed"));
			if (!this.disposed) this.scheduleReconnect();
		});

		return this.openPromise;
	}

	close(): void {
		if (this.disposed) return;
		this.disposed = true;
		if (this.reconnectTimer !== null) {
			globalThis.clearTimeout(this.reconnectTimer);
			this.reconnectTimer = null;
		}
		this.rejectConnecting(new BridgeTransportError("bridge client closed"));
		this.rejectPending(new BridgeTransportError("bridge client closed"));
		const ws = this.ws;
		this.ws = null;
		ws?.close();
	}

	isOpen(): boolean {
		return this.ws?.readyState === WS_OPEN;
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
		if (!ws || ws.readyState !== WS_OPEN) {
			throw new BridgeTransportError("bridge websocket is not open");
		}
		const id = `b${this.nextId++}`;
		const promise = new Promise<T>((resolve, reject) => {
			this.pending.set(id, { resolve: (value) => resolve(value as T), reject, method });
		});
		ws.send(JSON.stringify({ id, method, params }));
		return this.withRequestTimeout(id, promise);
	}

	private handleMessage(raw: string): void {
		let frame: { id?: unknown; result?: unknown; error?: { code?: unknown; message?: unknown; data?: unknown } } & Partial<ContractEventEnvelope>;
		try {
			frame = JSON.parse(raw);
		} catch {
			this.emitStatus("error");
			return;
		}
		if (typeof frame.event === "string") {
			const envelope = frame as ContractEventEnvelope;
			for (const handler of this.eventHandlers) handler(envelope);
			return;
		}
		const id = typeof frame.id === "string" ? frame.id : null;
		if (!id) return;
		const pending = this.pending.get(id);
		if (!pending) return;
		this.pending.delete(id);
		if (frame.error) {
			const code = typeof frame.error.code === "string" ? frame.error.code : "internal";
			const message = typeof frame.error.message === "string" ? frame.error.message : "request failed";
			pending.reject(new BridgeRequestError(code, message, frame.error.data));
			return;
		}
		pending.resolve(frame.result);
	}

	private emitStatus(status: BridgeConnectionStatus): void {
		for (const handler of this.statusHandlers) handler(status);
	}

	private rejectPending(error: BridgeTransportError): void {
		for (const pending of this.pending.values()) pending.reject(error);
		this.pending.clear();
	}

	private rejectConnecting(error: BridgeTransportError): void {
		const reject = this.rejectOpenPromise;
		this.openPromise = null;
		this.rejectOpenPromise = null;
		reject?.(error);
	}

	private withRequestTimeout<T>(id: string, promise: Promise<T>): Promise<T> {
		return new Promise<T>((resolve, reject) => {
			const timer = globalThis.setTimeout(() => {
				const pending = this.pending.get(id);
				if (!pending) return;
				this.pending.delete(id);
				pending.reject(new BridgeTransportError("bridge request timed out"));
			}, this.requestTimeoutMs);
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
				if (!this.disposed) this.scheduleReconnect();
			});
		}, this.reconnectDelayMs);
	}

	// ---- contract v0 facade -----------------------------------------------------

	listSessions(): Promise<SessionListResult> {
		return this.request("session.list");
	}

	/** M8 (G1): project catalog for sidebar grouping. */
	listProjects(): Promise<ProjectListResult> {
		return this.request("project.list");
	}

	createSession(params: SessionCreateParams = {}): Promise<SessionCreateResult> {
		return this.request("session.create", { ...params });
	}

	attachSession(id: string, fromSeq?: number): Promise<SessionAttachResult> {
		return this.request("session.attach", fromSeq === undefined ? { id } : { id, fromSeq });
	}

	detachSession(id: string): Promise<{ detached: boolean }> {
		return this.request("session.detach", { id });
	}

	sendPrompt(sessionId: string, text: string, idempotencyKey?: string): Promise<PromptSendResult> {
		return this.request("prompt.send", { sessionId, text, ...(idempotencyKey ? { idempotencyKey } : {}) });
	}

	steer(sessionId: string, text: string): Promise<CommandAckResult> {
		return this.request("session.steer", { sessionId, text });
	}

	followUp(sessionId: string, text: string): Promise<CommandAckResult> {
		return this.request("session.followUp", { sessionId, text });
	}

	abort(sessionId: string): Promise<CommandAckResult> {
		return this.request("session.abort", { sessionId });
	}

	getState(sessionId: string): Promise<SessionState> {
		return this.request("session.getState", { sessionId });
	}

	subagentTree(sessionId: string): Promise<SubagentTreeResult> {
		return this.request("subagent.tree", { sessionId });
	}

	/** M9 (contract v0.1): run a console cell on the session's kernel. The ack
	 * carries the deterministic cell_id; lifecycle streams via repl.* events. */
	replExecute(sessionId: string, code: string, clientCellId?: string): Promise<ReplExecuteResult> {
		return this.request("repl.execute", { sessionId, code, ...(clientCellId ? { client_cell_id: clientCellId } : {}) });
	}

	/** M11a (contract v0.2): model catalog + availability + auth status. */
	listModels(refresh?: boolean): Promise<ModelsListResult> {
		return this.request("models.list", refresh ? { refresh: true } : {});
	}

	/** M11a: switch the session's model (rpc set_model passthrough). */
	setModel(sessionId: string, provider: string, modelId: string): Promise<SetModelResult> {
		return this.request("session.setModel", { sessionId, provider, modelId });
	}

	/** M11a: set the session's thinking level (effective level reconciled). */
	setThinkingLevel(sessionId: string, level: string): Promise<SetThinkingLevelResult> {
		return this.request("session.setThinkingLevel", { sessionId, level });
	}

	/** M11a: child transcript + repl cells for drill-down (load-on-open). */
	subagentTranscript(sessionId: string, childId: string): Promise<SubagentTranscriptResult> {
		return this.request("subagent.transcript", { sessionId, childId });
	}

	/** M11a: inter-agent messages for a session (outbox + inbound entries). */
	commsList(sessionId: string): Promise<CommsListResult> {
		return this.request("comms.list", { sessionId });
	}
}

export function newIdempotencyKey(prefix: string): string {
	const uuid = typeof crypto?.randomUUID === "function" ? crypto.randomUUID() : `${Date.now()}-${Math.random()}`;
	return `${prefix}:${uuid}`;
}
