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
	onDisconnect?: () => void;
}

interface PendingBridgeCall {
	resolve: (value: RelayCoreBridgeAck) => void;
	reject: (reason: unknown) => void;
}

export interface OrchestratorShadowBridgeController {
	start(): Promise<void>;
	flush(): Promise<void>;
	stop(): Promise<void>;
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

export class RelayCoreBridgeClient {
	private readonly pending = new Map<number, PendingBridgeCall>();
	private readonly io: RelayCoreBridgeIO;
	private readonly options: RelayCoreBridgeClientOptions;
	private readonly handleInputEnd = () => {
		this.close(new Error("relay-core bridge closed its input stream"));
	};
	private detachInput?: () => void;
	private closed = false;
	private nextId = 1;

	constructor(io: RelayCoreBridgeIO, options: RelayCoreBridgeClientOptions = {}) {
		this.io = io;
		this.options = options;
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

	close(reason?: Error): void {
		if (this.closed) {
			return;
		}
		this.closed = true;
		this.detachInput?.();
		this.detachInput = undefined;
		this.io.input.off("end", this.handleInputEnd);
		for (const pending of this.pending.values()) {
			pending.reject(reason ?? new Error("relay-core bridge client closed"));
		}
		this.pending.clear();
		this.options.onDisconnect?.();
	}

	private async call(command: RelayCoreBridgeCommand): Promise<RelayCoreBridgeAck> {
		if (this.closed) {
			throw new Error("relay-core bridge client is closed");
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
			this.options.onEvent?.(message.event);
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
): OrchestratorShadowBridgeController {
	let started = false;
	let stopped = false;
	let unsubscribe: (() => void) | undefined;
	let queue: Promise<void> = Promise.resolve();

	const enqueue = (operation: () => Promise<void>): Promise<void> => {
		queue = queue
			.catch(() => undefined)
			.then(async () => {
				if (stopped) {
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
			unsubscribe = orchestrator.subscribeToChanges(() => {
				void sync("change");
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
			unsubscribe?.();
			unsubscribe = undefined;
			await queue.catch(() => undefined);
			try {
				if (started) {
					await client.dispose();
				}
			} finally {
				client.close();
			}
		},
	};
}
