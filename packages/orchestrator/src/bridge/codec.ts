export const RELAY_CORE_BRIDGE_PROTOCOL_VERSION = 1 as const;

export interface BridgeAgentSummary {
	id: string;
	parentId: string | null;
	role: string;
	status: "running" | "idle" | "disposed";
	depth: number;
	childCount: number;
	sessionFile: string | undefined;
	lastOutput: string | undefined;
}

export type RelayCoreBridgeMode = "shadow";
export type RelayCoreBridgeSyncReason = "init" | "change";

export interface OrchestratorShadowSnapshot {
	protocolVersion: typeof RELAY_CORE_BRIDGE_PROTOCOL_VERSION;
	rootAgentId: string;
	generatedAt: string;
	agents: BridgeAgentSummary[];
}

export interface RelayCoreHelloCommand {
	kind: "hello";
	protocolVersion: typeof RELAY_CORE_BRIDGE_PROTOCOL_VERSION;
	mode: RelayCoreBridgeMode;
}

export interface RelayCoreSyncSnapshotCommand {
	kind: "sync_snapshot";
	reason: RelayCoreBridgeSyncReason;
	snapshot: OrchestratorShadowSnapshot;
}

export interface RelayCoreDisposeCommand {
	kind: "dispose";
}

export type RelayCoreBridgeCommand =
	| RelayCoreHelloCommand
	| RelayCoreSyncSnapshotCommand
	| RelayCoreDisposeCommand;

export interface RelayCoreBridgeAck {
	acceptedCommand: RelayCoreBridgeCommand["kind"];
	acceptedAt: string;
}

export interface RelayCoreBridgeError {
	message: string;
	data?: Record<string, unknown>;
}

export interface RelayCoreDiagnosticEvent {
	type: "diagnostic";
	level: "info" | "warn" | "error";
	message: string;
	details?: Record<string, unknown>;
}

export interface RelayCoreShadowDiffEvent {
	type: "shadow_diff";
	summary: string;
	mismatchCount: number;
	details?: Record<string, unknown>;
}

export type RelayCoreBridgeEvent = RelayCoreDiagnosticEvent | RelayCoreShadowDiffEvent;

export interface RelayCoreBridgeCallMessage {
	type: "call";
	id: number;
	command: RelayCoreBridgeCommand;
}

export interface RelayCoreBridgeResultMessage {
	type: "result";
	id: number;
	value: RelayCoreBridgeAck;
}

export interface RelayCoreBridgeErrorMessage {
	type: "error";
	id: number;
	error: RelayCoreBridgeError;
}

export interface RelayCoreBridgeEventMessage {
	type: "event";
	event: RelayCoreBridgeEvent;
}

export type RelayCoreBridgeMessage =
	| RelayCoreBridgeCallMessage
	| RelayCoreBridgeResultMessage
	| RelayCoreBridgeErrorMessage
	| RelayCoreBridgeEventMessage;

export function encodeRelayCoreBridgeMessage(message: RelayCoreBridgeMessage): string {
	return `${JSON.stringify(message)}\n`;
}

export function decodeRelayCoreBridgeMessage(line: string): RelayCoreBridgeMessage {
	const parsed = JSON.parse(line);
	if (typeof parsed !== "object" || parsed === null || typeof parsed.type !== "string") {
		throw new Error(`Invalid relay-core bridge frame: ${line.slice(0, 200)}`);
	}
	return parsed as RelayCoreBridgeMessage;
}
