/**
 * Canonical wire schema for the pi-relay Client RPC transport.
 *
 * Protocol: newline-delimited JSON over a duplex byte stream (stdio, socket, etc).
 * Framing: exactly one JSON object per line, LF-terminated.
 *
 * There is no version field. Both ends are built from the same source tree.
 *
 * Message types:
 * - call   — client invokes a method on the server's Client
 * - result — server's success return for a call
 * - error  — server's failure return for a call
 * - cancel — client asks to abort an in-flight call
 * - event  — server pushes a SessionEvent for a subscribed session
 */
import type { ImageContent, Model } from "@pi-relay/ai";
import type { ThinkingLevel } from "@pi-relay/agent-core";
import type { PromptOptions, SessionEvent, SessionState, SessionSummary } from "../types.js";

export type RpcCallId = number;

/**
 * Method names mirror the Client interface as dot-paths.
 * Each method's params / result types are declared in MethodMap below.
 */
export type RpcMethod =
	| "sessions.current"
	| "sessions.open"
	| "sessions.resume"
	| "sessions.list"
	| "session.prompt"
	| "session.steer"
	| "session.followUp"
	| "session.abort"
	| "session.switchModel"
	| "session.cycleModel"
	| "session.cycleThinking"
	| "session.getState"
	| "session.close"
	| "models.list"
	| "models.listAvailable"
	| "auth.login"
	| "auth.logout"
	| "auth.status"
	| "dispose";

/**
 * RPC-safe snapshot of SessionSummary.
 *
 * SessionSummary.created / .modified are Date on the Client surface. JSON flattens
 * them to strings, so the wire shape carries ISO strings and RpcClient rehydrates.
 */
export interface WireSessionSummary extends Omit<SessionSummary, "created" | "modified"> {
	created: string;
	modified: string;
}

export interface WireAuthStatusEntry {
	provider: string;
	hasCredential: boolean;
}

export type WireModel = Model<any>;

/** Parameters and result shape for every RPC method. */
export interface MethodMap {
	"sessions.current": {
		params: Record<string, never>;
		result: { sessionId: string };
	};
	"sessions.open": {
		params: { sessionId?: string; parentSession?: string };
		result: { sessionId: string };
	};
	"sessions.resume": {
		params: { sessionId?: string; sessionPath: string; cwdOverride?: string };
		result: { sessionId: string };
	};
	"sessions.list": {
		params: Record<string, never>;
		result: { sessions: WireSessionSummary[] };
	};
	"session.prompt": {
		params: { sessionId: string; text: string; opts?: PromptOptions };
		result: null;
	};
	"session.steer": {
		params: { sessionId: string; text: string; images?: ImageContent[] };
		result: null;
	};
	"session.followUp": {
		params: { sessionId: string; text: string; images?: ImageContent[] };
		result: null;
	};
	"session.abort": {
		params: { sessionId: string };
		result: null;
	};
	"session.switchModel": {
		params: { sessionId: string; model: WireModel };
		result: null;
	};
	"session.cycleModel": {
		params: { sessionId: string; direction: "forward" | "backward" };
		result: { model: WireModel; thinkingLevel: ThinkingLevel; isScoped: boolean } | null;
	};
	"session.cycleThinking": {
		params: { sessionId: string };
		result: { level: ThinkingLevel };
	};
	"session.getState": {
		params: { sessionId: string };
		result: SessionState;
	};
	"session.close": {
		params: { sessionId: string };
		result: null;
	};
	"models.list": {
		params: Record<string, never>;
		result: { models: WireModel[] };
	};
	"models.listAvailable": {
		params: Record<string, never>;
		result: { models: WireModel[] };
	};
	"auth.login": {
		params: { provider: string };
		result: null;
	};
	"auth.logout": {
		params: { provider: string };
		result: null;
	};
	"auth.status": {
		params: Record<string, never>;
		result: { entries: WireAuthStatusEntry[] };
	};
	dispose: {
		params: Record<string, never>;
		result: null;
	};
}

export type RpcParams<M extends RpcMethod> = MethodMap[M]["params"];
export type RpcResult<M extends RpcMethod> = MethodMap[M]["result"];

export interface RpcCallMessage<M extends RpcMethod = RpcMethod> {
	type: "call";
	id: RpcCallId;
	method: M;
	params: RpcParams<M>;
}

export interface RpcResultMessage<M extends RpcMethod = RpcMethod> {
	type: "result";
	id: RpcCallId;
	value: RpcResult<M>;
}

export interface RpcErrorPayload {
	message: string;
	data?: Record<string, unknown>;
}

export interface RpcErrorMessage {
	type: "error";
	id: RpcCallId;
	error: RpcErrorPayload;
}

export interface RpcCancelMessage {
	type: "cancel";
	id: RpcCallId;
}

export interface RpcEventMessage {
	type: "event";
	sessionId: string;
	event: SessionEvent;
}

export type RpcMessage =
	| RpcCallMessage
	| RpcResultMessage
	| RpcErrorMessage
	| RpcCancelMessage
	| RpcEventMessage;

/**
 * Serialize a wire message to a single NDJSON record (LF-terminated, LF-only).
 *
 * Payload strings may contain U+2028 / U+2029; framing is LF so they are passed
 * through untouched. Consumers must split only on "\n".
 */
export function encodeMessage(message: RpcMessage): string {
	return `${JSON.stringify(message)}\n`;
}

export function decodeMessage(line: string): RpcMessage {
	const parsed = JSON.parse(line);
	if (typeof parsed !== "object" || parsed === null || typeof parsed.type !== "string") {
		throw new Error(`Invalid RPC frame: ${line.slice(0, 200)}`);
	}
	return parsed as RpcMessage;
}
