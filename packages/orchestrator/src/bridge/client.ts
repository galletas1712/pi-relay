import { StringDecoder } from "node:string_decoder";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import {
	decodeRelayCoreBridgeMessage,
	encodeRelayCoreBridgeMessage,
	type BridgeAgentSummary,
	type OrchestratorShadowSnapshot,
	RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
	type RelayCoreBridgeAck,
	type RelayCoreBridgeCallMessage,
	type RelayCoreBridgeCommand,
	type RelayCoreBridgeEvent,
	type RelayCoreBridgeMessage,
	type RelayCoreBridgeSyncReason,
} from "./codec.js";

export interface RelayCoreBridgeIO {
	input: NodeJS.ReadableStream;
	output: NodeJS.WritableStream;
}

export interface RelayCoreBridgeClientOptions {
	onEvent?: (event: RelayCoreBridgeEvent) => void;
	onDisconnect?: (reason?: Error) => void;
}

interface PendingBridgeCall {
	resolve: (value: RelayCoreBridgeAck) => void;
	reject: (reason: unknown) => void;
}

class RelayCoreBridgeClosedError extends Error {
	constructor(message = "relay-core bridge client is closed") {
		super(message);
		this.name = "RelayCoreBridgeClosedError";
	}
}

export interface OrchestratorShadowBridgeController {
	start(): Promise<void>;
	flush(): Promise<void>;
	stop(): Promise<void>;
}

export interface OrchestratorShadowBridgeControllerOptions {
	onDisconnect?: (reason?: Error) => void;
}

type OrchestratorShadowSource = {
	rootAgentId: string;
	getAgentSummaries(): BridgeAgentSummary[];
	subscribeToChanges(listener: () => void): () => void;
};

function readLines(stream: NodeJS.ReadableStream, onLine: (line: string) => void): () => void {
	const decoder = new StringDecoder("utf8");
	let buffer = "";

	const emit = (line: string) => {
		onLine(line.endsWith("\r") ? line.slice(0, -1) : line);
	};

	const onData = (chunk: string | Buffer) => {
		buffer += typeof chunk === "string" ? chunk : decoder.write(chunk);
		while (true) {
			const newline = buffer.indexOf("\n");
			if (newline === -1) {
				return;
			}
			emit(buffer.slice(0, newline));
			buffer = buffer.slice(newline + 1);
		}
	};

	const onEnd = () => {
		buffer += decoder.end();
		if (buffer.length > 0) {
			emit(buffer);
			buffer = "";
		}
	};

	stream.on("data", onData);
	stream.on("end", onEnd);

	return () => {
		stream.off("data", onData);
		stream.off("end", onEnd);
	};
}

function createRemoteError(message: RelayCoreBridgeMessage & { type: "error" }): Error {
	const error = new Error(message.error.message);
	if (message.error.data) {
		Object.assign(error, { data: message.error.data });
	}
	return error;
}

function toError(reason: unknown, fallback: string): Error {
	if (reason instanceof Error) {
		return reason;
	}
	return new Error(reason === undefined ? fallback : String(reason));
}

export class RelayCoreBridgeClient {
	private readonly pending = new Map<number, PendingBridgeCall>();
	private readonly eventListeners = new Set<(event: RelayCoreBridgeEvent) => void>();
	private readonly disconnectListeners = new Set<(reason?: Error) => void>();
	private readonly io: RelayCoreBridgeIO;
	private readonly handleInputEnd = () => {
		this.close(new RelayCoreBridgeClosedError("relay-core bridge closed its input stream"));
	};
	private detachInput?: () => void;
	private closed = false;
	private disconnectReason: Error | undefined;
	private nextId = 1;

	constructor(io: RelayCoreBridgeIO, options: RelayCoreBridgeClientOptions = {}) {
		this.io = io;
		if (options.onEvent) {
			this.eventListeners.add(options.onEvent);
		}
		if (options.onDisconnect) {
			this.disconnectListeners.add(options.onDisconnect);
		}
		this.detachInput = readLines(this.io.input, (line) => {
			if (line.trim().length === 0) {
				return;
			}
			this.handleLine(line);
		});
		this.io.input.on("end", this.handleInputEnd);
	}

	static fromChildProcess(
		child: Pick<ChildProcessWithoutNullStreams, "stdin" | "stdout">,
		options?: RelayCoreBridgeClientOptions,
	): RelayCoreBridgeClient {
		return new RelayCoreBridgeClient(
			{
				input: child.stdout,
				output: child.stdin,
			},
			options,
		);
	}

	async hello(): Promise<RelayCoreBridgeAck> {
		return this.call({
			kind: "hello",
			protocolVersion: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
			mode: "shadow",
		});
	}

	async syncSnapshot(
		snapshot: OrchestratorShadowSnapshot,
		reason: RelayCoreBridgeSyncReason,
	): Promise<RelayCoreBridgeAck> {
		return this.call({
			kind: "sync_snapshot",
			reason,
			snapshot,
		});
	}

	async dispose(): Promise<RelayCoreBridgeAck> {
		return this.call({ kind: "dispose" });
	}

	get isClosed(): boolean {
		return this.closed;
	}

	subscribeToEvents(listener: (event: RelayCoreBridgeEvent) => void): () => void {
		this.eventListeners.add(listener);
		return () => {
			this.eventListeners.delete(listener);
		};
	}

	subscribeToDisconnect(listener: (reason?: Error) => void): () => void {
		this.disconnectListeners.add(listener);
		return () => {
			this.disconnectListeners.delete(listener);
		};
	}

	close(reason?: Error): void {
		if (this.closed) {
			return;
		}
		const closeReason = reason ?? new RelayCoreBridgeClosedError();
		this.closed = true;
		this.disconnectReason = closeReason;
		this.detachInput?.();
		this.detachInput = undefined;
		this.io.input.off("end", this.handleInputEnd);
		for (const pending of this.pending.values()) {
			pending.reject(closeReason);
		}
		this.pending.clear();
		for (const listener of this.disconnectListeners) {
			listener(closeReason);
		}
	}

	private async call(command: RelayCoreBridgeCommand): Promise<RelayCoreBridgeAck> {
		if (this.closed) {
			throw this.disconnectReason ?? new RelayCoreBridgeClosedError();
		}
		const id = this.nextId++;
		const message: RelayCoreBridgeCallMessage = {
			type: "call",
			id,
			command,
		};

		const result = new Promise<RelayCoreBridgeAck>((resolve, reject) => {
			this.pending.set(id, { resolve, reject });
		});

		this.io.output.write(encodeRelayCoreBridgeMessage(message));
		return result;
	}

	private handleLine(line: string): void {
		let message: RelayCoreBridgeMessage;
		try {
			message = decodeRelayCoreBridgeMessage(line);
		} catch (error) {
			this.close(error instanceof Error ? error : new Error(String(error)));
			return;
		}

		if (message.type === "event") {
			for (const listener of this.eventListeners) {
				listener(message.event);
			}
			return;
		}

		if (message.type !== "result" && message.type !== "error") {
			return;
		}

		const pending = this.pending.get(message.id);
		if (!pending) {
			return;
		}
		this.pending.delete(message.id);

		if (message.type === "result") {
			pending.resolve(message.value);
			return;
		}

		pending.reject(createRemoteError(message));
	}
}

export function createOrchestratorShadowSnapshot(
	orchestrator: Pick<OrchestratorShadowSource, "rootAgentId" | "getAgentSummaries">,
): OrchestratorShadowSnapshot {
	return {
		protocolVersion: RELAY_CORE_BRIDGE_PROTOCOL_VERSION,
		rootAgentId: orchestrator.rootAgentId,
		generatedAt: new Date().toISOString(),
		agents: orchestrator.getAgentSummaries(),
	};
}

export function attachOrchestratorShadowBridge(
	orchestrator: OrchestratorShadowSource,
	client: RelayCoreBridgeClient,
	options: OrchestratorShadowBridgeControllerOptions = {},
): OrchestratorShadowBridgeController {
	let started = false;
	let stopped = false;
	let disconnected = false;
	let unsubscribe: (() => void) | undefined;
	let detachDisconnectListener: (() => void) | undefined;
	let queue: Promise<void> = Promise.resolve();

	const handleDisconnect = (reason?: Error) => {
		if (disconnected) {
			return;
		}
		disconnected = true;
		unsubscribe?.();
		unsubscribe = undefined;
		options.onDisconnect?.(reason);
	};

	const closeBridge = (reason: unknown, fallbackMessage: string) => {
		const error = toError(reason, fallbackMessage);
		if (client.isClosed) {
			handleDisconnect(error);
			return;
		}
		client.close(error);
	};

	const enqueue = (operation: () => Promise<void>): Promise<void> => {
		queue = queue
			.catch(() => undefined)
			.then(async () => {
				if (stopped || disconnected) {
					return;
				}
				await operation();
			});
		return queue;
	};

	const sync = (reason: RelayCoreBridgeSyncReason) =>
		enqueue(async () => {
			await client.syncSnapshot(createOrchestratorShadowSnapshot(orchestrator), reason);
		});

	return {
		async start() {
			if (started) {
				return;
			}
			started = true;
			detachDisconnectListener = client.subscribeToDisconnect(handleDisconnect);
			if (client.isClosed) {
				closeBridge(undefined, "relay-core bridge client is closed");
				return;
			}
			unsubscribe = orchestrator.subscribeToChanges(() => {
				void sync("change").catch((error) => {
					closeBridge(error, "relay-core bridge change sync failed");
				});
			});

			await enqueue(async () => {
				await client.hello();
				await client.syncSnapshot(createOrchestratorShadowSnapshot(orchestrator), "init");
			});
		},

		async flush() {
			await queue.catch(() => undefined);
		},

		async stop() {
			if (stopped) {
				return;
			}
			stopped = true;
			detachDisconnectListener?.();
			detachDisconnectListener = undefined;
			unsubscribe?.();
			unsubscribe = undefined;
			await queue.catch(() => undefined);
			try {
				if (started && !client.isClosed) {
					try {
						await client.dispose();
					} catch (error) {
						closeBridge(error, "relay-core bridge dispose failed");
					}
				}
			} finally {
				client.close();
			}
		},
	};
}
