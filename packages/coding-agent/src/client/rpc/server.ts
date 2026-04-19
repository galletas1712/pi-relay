import { writeRawStdout } from "../../core/output-guard.js";
import type { Client, SessionEvent, SessionHandle } from "../types.js";
import { readLines } from "./framing.js";
import {
	decodeMessage,
	encodeMessage,
	type MethodMap,
	type RpcCallId,
	type RpcCallMessage,
	type RpcErrorMessage,
	type RpcErrorPayload,
	type RpcMessage,
	type RpcMethod,
	type RpcResultMessage,
	type WireSessionSummary,
} from "./wire.js";

export interface NodeIO {
	input: NodeJS.ReadableStream;
	output: NodeJS.WritableStream;
}

/**
 * Default server IO binds to process.stdin and routes outbound frames through
 * writeRawStdout so they survive the stdout takeover that main.ts applies when
 * stdout is reserved for structured output.
 */
const defaultServerIO: NodeIO = {
	input: process.stdin,
	output: {
		write(chunk: string | Uint8Array): boolean {
			writeRawStdout(typeof chunk === "string" ? chunk : Buffer.from(chunk).toString("utf8"));
			return true;
		},
	} as NodeJS.WritableStream,
};

/** Per-method dispatcher. Handlers receive typed params and return typed results. */
type MethodDispatcher = {
	[M in RpcMethod]: (params: MethodMap[M]["params"]) => Promise<MethodMap[M]["result"]>;
};

/**
 * RpcServer bridges a LocalClient to a newline-delimited JSON transport.
 *
 * It is not a Client itself. Each incoming `call` is dispatched to the wrapped
 * LocalClient; every opened SessionHandle's `events` iterable is drained into
 * outbound `event` frames addressed by sessionId.
 */
export class RpcServer {
	private readonly io: NodeIO;
	private readonly dispatch: MethodDispatcher;
	private readonly openSessions = new Map<string, SessionHandle>();
	private readonly eventPumps = new Map<string, Promise<void>>();
	private detachInput?: () => void;
	private done?: () => void;

	constructor(
		private readonly client: Client,
		io?: NodeIO,
	) {
		this.io = io ?? defaultServerIO;
		this.dispatch = this.buildDispatch();
	}

	/**
	 * Listen for wire messages on io.input until the input stream closes.
	 * Resolves once the stream ends.
	 */
	listen(): Promise<void> {
		return new Promise((resolve) => {
			this.done = resolve;
			this.detachInput = readLines(this.io.input, (line) => {
				if (line.trim().length === 0) return;
				this.handleLine(line);
			});
			const onEnd = () => {
				this.detachInput?.();
				this.detachInput = undefined;
				this.io.input.off("end", onEnd);
				this.done?.();
				this.done = undefined;
			};
			this.io.input.on("end", onEnd);
		});
	}

	private write(message: RpcMessage): void {
		this.io.output.write(encodeMessage(message));
	}

	private handleLine(line: string): void {
		let message: RpcMessage;
		try {
			message = decodeMessage(line);
		} catch (err) {
			// Inbound frame is malformed. Emit an error frame on id: -1 so the
			// other side has something to diagnose (client-side rejects all
			// pending calls on its own parse errors, so -1 is fine here).
			// Include a truncated snippet so wire corruption can be diagnosed.
			const snippet = line.length > 200 ? `${line.slice(0, 200)}…` : line;
			this.write({
				type: "error",
				id: -1,
				error: {
					message: `parse error: ${(err as Error).message}. Dropped frame: ${snippet}`,
				},
			});
			return;
		}
		if (message.type === "call") {
			void this.handleCall(message);
			return;
		}
		if (message.type === "cancel") {
			// Cancellation is not wired through AgentSession's structured prompts yet; swallow
			// the frame so clients can send it without crashing the server. Follow-up: route
			// cancel(id) to the session whose in-flight call owns that id.
			return;
		}
		// result/error/event frames are outbound-only on the server side; ignore inbound.
	}

	private async handleCall<M extends RpcMethod>(call: RpcCallMessage<M>): Promise<void> {
		try {
			const handler = this.dispatch[call.method] as (
				params: MethodMap[M]["params"],
			) => Promise<MethodMap[M]["result"]>;
			if (!handler) {
				this.sendError(call.id, { message: `unknown method: ${call.method}` });
				return;
			}
			const value = await handler(call.params);
			const result: RpcResultMessage<M> = { type: "result", id: call.id, value };
			this.write(result);
		} catch (err) {
			this.sendError(call.id, {
				message: err instanceof Error ? err.message : String(err),
			});
		}
	}

	private sendError(id: RpcCallId, error: RpcErrorPayload): void {
		const message: RpcErrorMessage = { type: "error", id, error };
		this.write(message);
	}

	private trackSession(handle: SessionHandle): void {
		if (this.openSessions.has(handle.id)) {
			return;
		}
		this.openSessions.set(handle.id, handle);
		this.eventPumps.set(handle.id, this.pumpEvents(handle));
	}

	private async pumpEvents(handle: SessionHandle): Promise<void> {
		const sessionId = handle.id;
		try {
			for await (const event of handle.events) {
				this.write({ type: "event", sessionId, event: event as SessionEvent });
			}
		} catch (err) {
			this.sendError(-1, {
				message: `event pump for ${sessionId} failed: ${err instanceof Error ? err.message : String(err)}`,
			});
		}
	}

	private buildDispatch(): MethodDispatcher {
		return {
			"sessions.current": async () => {
				const handle = this.client.session();
				this.trackSession(handle);
				return { sessionId: handle.id };
			},
			"sessions.open": async (params) => {
				const handle = await this.client.sessions.open(
					params.parentSession ? { parentSession: params.parentSession } : undefined,
				);
				this.trackSession(handle);
				return { sessionId: handle.id };
			},
			"sessions.resume": async (params) => {
				const handle = await this.client.sessions.resume(
					params.sessionPath,
					params.cwdOverride ? { cwdOverride: params.cwdOverride } : undefined,
				);
				this.trackSession(handle);
				return { sessionId: handle.id };
			},
			"sessions.list": async () => {
				const summaries = await this.client.sessions.list();
				const sessions: WireSessionSummary[] = summaries.map((s) => ({
					...s,
					created: s.created.toISOString(),
					modified: s.modified.toISOString(),
				}));
				return { sessions };
			},
			"session.prompt": async (params) => {
				await this.requireSession(params.sessionId).prompt(params.text, params.opts);
				return null;
			},
			"session.steer": async (params) => {
				await this.requireSession(params.sessionId).steer(params.text, params.images);
				return null;
			},
			"session.followUp": async (params) => {
				await this.requireSession(params.sessionId).followUp(params.text, params.images);
				return null;
			},
			"session.abort": async (params) => {
				await this.requireSession(params.sessionId).abort();
				return null;
			},
			"session.switchModel": async (params) => {
				await this.requireSession(params.sessionId).switchModel(params.model);
				return null;
			},
			"session.cycleModel": async (params) => {
				const result = await this.requireSession(params.sessionId).cycleModel(params.direction);
				return result ?? null;
			},
			"session.cycleThinking": async (params) => {
				const level = await this.requireSession(params.sessionId).cycleThinking();
				return { level };
			},
			"session.getState": async (params) => {
				return this.requireSession(params.sessionId).getState();
			},
			"session.close": async (params) => {
				const handle = this.openSessions.get(params.sessionId);
				if (!handle) return null;
				await handle.close();
				this.openSessions.delete(params.sessionId);
				await this.eventPumps.get(params.sessionId);
				this.eventPumps.delete(params.sessionId);
				return null;
			},
			"models.list": async () => {
				const models = await this.client.models.list();
				return { models };
			},
			"models.listAvailable": async () => {
				const models = await this.client.models.listAvailable();
				return { models };
			},
			"auth.login": async (params) => {
				await this.client.auth.login(params.provider);
				return null;
			},
			"auth.logout": async (params) => {
				await this.client.auth.logout(params.provider);
				return null;
			},
			"auth.status": async () => {
				const entries = await this.client.auth.status();
				return { entries: [...entries] };
			},
			dispose: async () => {
				for (const handle of this.openSessions.values()) {
					await handle.close();
				}
				await Promise.all(this.eventPumps.values());
				this.openSessions.clear();
				this.eventPumps.clear();
				await this.client.dispose();
				return null;
			},
		};
	}

	private requireSession(sessionId: string): SessionHandle {
		const handle = this.openSessions.get(sessionId);
		if (!handle) {
			throw new Error(`session not open: ${sessionId}`);
		}
		return handle;
	}
}
