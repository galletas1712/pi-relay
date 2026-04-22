import { StringDecoder } from "node:string_decoder";
import type { ChildProcessWithoutNullStreams } from "node:child_process";
import type { SessionCoreCommand } from "../session-core/commands.js";
import type { SessionCoreState } from "../session-core/state.js";
import {
	decodeSessionShadowBridgeMessage,
	encodeSessionShadowBridgeMessage,
	createSessionShadowSnapshot,
	SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
	type SessionShadowBridgeAck,
	type SessionShadowBridgeCallMessage,
	type SessionShadowBridgeCommand,
	type SessionShadowBridgeEvent,
	type SessionShadowBridgeMessage,
	type SessionShadowSyncReason,
} from "./codec.js";

export interface SessionShadowBridgeIO {
	input: NodeJS.ReadableStream;
	output: NodeJS.WritableStream;
}

export interface SessionShadowBridgeClientOptions {
	onEvent?: (event: SessionShadowBridgeEvent) => void;
	onDisconnect?: () => void;
}

interface PendingBridgeCall {
	resolve: (value: SessionShadowBridgeAck) => void;
	reject: (reason: unknown) => void;
}

export interface SessionShadowBridgeController {
	start(initialState: SessionCoreState): Promise<void>;
	dispatch(command: SessionCoreCommand): Promise<void>;
	flush(): Promise<void>;
	stop(): Promise<void>;
}

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

function createRemoteError(message: SessionShadowBridgeMessage & { type: "error" }): Error {
	const error = new Error(message.error.message);
	if (message.error.data) {
		Object.assign(error, { data: message.error.data });
	}
	return error;
}

export class SessionShadowBridgeClient {
	private readonly pending = new Map<number, PendingBridgeCall>();
	private readonly io: SessionShadowBridgeIO;
	private readonly options: SessionShadowBridgeClientOptions;
	private readonly handleInputEnd = () => {
		this.close(new Error("session-core bridge closed its input stream"));
	};
	private detachInput?: () => void;
	private closed = false;
	private nextId = 1;

	constructor(io: SessionShadowBridgeIO, options: SessionShadowBridgeClientOptions = {}) {
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
		options?: SessionShadowBridgeClientOptions,
	): SessionShadowBridgeClient {
		return new SessionShadowBridgeClient(
			{
				input: child.stdout,
				output: child.stdin,
			},
			options,
		);
	}

	async hello(): Promise<SessionShadowBridgeAck> {
		return this.call({
			kind: "hello",
			protocolVersion: SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
			mode: "shadow",
		});
	}

	async syncState(snapshot: ReturnType<typeof createSessionShadowSnapshot>, reason: SessionShadowSyncReason) {
		return this.call({
			kind: "sync_state",
			reason,
			snapshot,
		});
	}

	async dispatch(command: SessionCoreCommand): Promise<SessionShadowBridgeAck> {
		return this.call({
			kind: "dispatch",
			command,
		});
	}

	async dispose(): Promise<SessionShadowBridgeAck> {
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
			pending.reject(reason ?? new Error("session-core bridge client closed"));
		}
		this.pending.clear();
		this.options.onDisconnect?.();
	}

	private async call(command: SessionShadowBridgeCommand): Promise<SessionShadowBridgeAck> {
		if (this.closed) {
			throw new Error("session-core bridge client is closed");
		}
		const id = this.nextId++;
		const message: SessionShadowBridgeCallMessage = {
			type: "call",
			id,
			command,
		};

		const result = new Promise<SessionShadowBridgeAck>((resolve, reject) => {
			this.pending.set(id, { resolve, reject });
		});

		this.io.output.write(encodeSessionShadowBridgeMessage(message));
		return result;
	}

	private handleLine(line: string): void {
		let message: SessionShadowBridgeMessage;
		try {
			message = decodeSessionShadowBridgeMessage(line);
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

export function attachSessionShadowBridge(client: SessionShadowBridgeClient): SessionShadowBridgeController {
	let started = false;
	let stopped = false;
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

	return {
		async start(initialState) {
			if (started) {
				await queue.catch(() => undefined);
				return;
			}
			started = true;
			await enqueue(async () => {
				await client.hello();
				await client.syncState(createSessionShadowSnapshot(initialState), "init");
			});
		},

		async dispatch(command) {
			if (!started) {
				throw new Error("session-core shadow bridge has not been started");
			}
			await enqueue(async () => {
				await client.dispatch(command);
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
