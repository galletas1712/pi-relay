import type { SessionCoreCommand } from "../session-core/commands.js";
import { createSessionCoreState, type SessionCoreState } from "../session-core/state.js";

export const SESSION_CORE_BRIDGE_PROTOCOL_VERSION = 1 as const;

export type SessionShadowBridgeMode = "shadow";
export type SessionShadowSyncReason = "init" | "reset";

export interface SessionShadowSnapshot {
	protocolVersion: typeof SESSION_CORE_BRIDGE_PROTOCOL_VERSION;
	generatedAt: string;
	state: SessionCoreState;
}

export interface SessionShadowHelloCommand {
	kind: "hello";
	protocolVersion: typeof SESSION_CORE_BRIDGE_PROTOCOL_VERSION;
	mode: SessionShadowBridgeMode;
}

export interface SessionShadowSyncStateCommand {
	kind: "sync_state";
	reason: SessionShadowSyncReason;
	snapshot: SessionShadowSnapshot;
}

export interface SessionShadowDispatchCommand {
	kind: "dispatch";
	command: SessionCoreCommand;
}

export interface SessionShadowDisposeCommand {
	kind: "dispose";
}

export type SessionShadowBridgeCommand =
	| SessionShadowHelloCommand
	| SessionShadowSyncStateCommand
	| SessionShadowDispatchCommand
	| SessionShadowDisposeCommand;

export interface SessionShadowBridgeAck {
	acceptedCommand: SessionShadowBridgeCommand["kind"];
	acceptedAt: string;
}

export interface SessionShadowBridgeError {
	message: string;
	data?: Record<string, unknown>;
}

export interface SessionShadowDiagnosticEvent {
	type: "diagnostic";
	level: "info" | "warn" | "error";
	message: string;
	details?: Record<string, unknown>;
}

export interface SessionShadowStateSyncedEvent {
	type: "state_synced";
	reason: SessionShadowSyncReason;
	pendingMessageCount: number;
	runState: SessionCoreState["runState"];
}

export interface SessionShadowCommandAppliedEvent {
	type: "command_applied";
	commandType: SessionCoreCommand["type"];
	pendingMessageCount: number;
	runState: SessionCoreState["runState"];
}

export type SessionShadowBridgeEvent =
	| SessionShadowDiagnosticEvent
	| SessionShadowStateSyncedEvent
	| SessionShadowCommandAppliedEvent;

export interface SessionShadowBridgeCallMessage {
	type: "call";
	id: number;
	command: SessionShadowBridgeCommand;
}

export interface SessionShadowBridgeResultMessage {
	type: "result";
	id: number;
	value: SessionShadowBridgeAck;
}

export interface SessionShadowBridgeErrorMessage {
	type: "error";
	id: number;
	error: SessionShadowBridgeError;
}

export interface SessionShadowBridgeEventMessage {
	type: "event";
	event: SessionShadowBridgeEvent;
}

export type SessionShadowBridgeMessage =
	| SessionShadowBridgeCallMessage
	| SessionShadowBridgeResultMessage
	| SessionShadowBridgeErrorMessage
	| SessionShadowBridgeEventMessage;

export function createSessionShadowSnapshot(state: SessionCoreState): SessionShadowSnapshot {
	return {
		protocolVersion: SESSION_CORE_BRIDGE_PROTOCOL_VERSION,
		generatedAt: new Date().toISOString(),
		state: createSessionCoreState(state),
	};
}

export function encodeSessionShadowBridgeMessage(message: SessionShadowBridgeMessage): string {
	return `${JSON.stringify(message)}\n`;
}

export function decodeSessionShadowBridgeMessage(line: string): SessionShadowBridgeMessage {
	const parsed = JSON.parse(line);
	if (typeof parsed !== "object" || parsed === null || typeof parsed.type !== "string") {
		throw new Error(`Invalid session-core bridge frame: ${line.slice(0, 200)}`);
	}
	return parsed as SessionShadowBridgeMessage;
}
