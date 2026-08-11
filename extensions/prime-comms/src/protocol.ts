// Wire protocol for prime-comms (M2).
//
// PROVENANCE: message format ported from prime-agent's
//   packages/coding-agent/src/core/agent-messages.ts
// (createAgentSessionMessagePrompt / createAgentSessionMessage /
//  createAgentSessionMessageReceipt). The custom message type "agent_message"
// and the on-screen content format are identical to prime-agent's so rendered
// transcripts and parsing regexes stay interchangeable.
//
// M2 additions/divergences (documented): deliveryStatus gains "persisted"
// (written directly into a dead target's session file) and "recovered"
// (re-emitted into the sender's own context); the outbox record carries the
// resolved target (sessionId + sessionDir) so re-drive works after a host
// restart when the in-memory rlm-child registry is empty.

export const AGENT_MESSAGE_CUSTOM_TYPE = "agent_message";
export const AGENT_MESSAGE_SOURCE = "prime-comms";
export const OUTBOX_RECOVERY_CUSTOM_TYPE = "agent_message_outbox_recovery";

export type ReceiverRole = "parent" | "sibling" | "child";
export type FamilyRelationship = "parent" | "sibling" | "child";

export interface AgentRef {
	sessionId: string;
	name?: string;
}

export interface AgentMessageSender extends AgentRef {
	depth?: number;
}

/** Resolved delivery target persisted into the outbox at queue time so that
 * re-drive works after a host restart (registry is empty then). */
export interface ResolvedTarget {
	sessionId: string;
	sessionDir: string;
	childId?: string;
	name?: string;
	/** Session through which getLiveChild must be called for child/sibling
	 * targets (sender id for children; parent id for siblings). */
	parentSessionId?: string;
}

export interface AgentMessagePayload {
	id: string;
	source: string;
	message: string;
	from?: AgentMessageSender;
	/** Sender relationship from the receiver's point of view. */
	fromRelationship?: FamilyRelationship;
	target: AgentRef;
}

export interface AgentMessageDetails {
	id: string;
	message: string;
	from?: AgentMessageSender;
	fromRelationship?: FamilyRelationship;
	target?: AgentRef;
}

export type DeliveryStatus = "delivered" | "queued" | "failed" | "persisted" | "recovered";

export interface AgentMessageReceipt {
	id: string;
	source: string;
	target: AgentRef;
	from?: AgentMessageSender;
	message: string;
	/** Not named "status": the kernel host-bridge envelope reserves that key. */
	deliveryStatus: DeliveryStatus;
	deliveredAt?: string;
	queuedAt?: string;
	deliveryMode?: "steer" | "triggerTurn" | "deferred";
	/** True when the target ran a turn for this message and settled. */
	settled?: boolean;
	error?: string;
}

export type OutboxStatus = "queued" | "delivered" | "failed" | "persisted" | "recovered";

export interface OutboxRecord {
	id: string;
	ts: string;
	from: AgentRef;
	role: ReceiverRole;
	receiverName?: string;
	target?: ResolvedTarget;
	message: string;
	status: OutboxStatus;
	detail?: string;
}

function formatRef(ref: AgentRef | undefined): string {
	if (!ref) return "unknown";
	return ref.name ? `${ref.name} (${ref.sessionId})` : ref.sessionId;
}

/** createAgentSessionMessagePrompt: ported from prime-agent. */
export function createAgentMessagePrompt(payload: AgentMessagePayload): string {
	const relationshipLabel = payload.fromRelationship
		? `[from ${payload.fromRelationship}${
				payload.fromRelationship === "parent"
					? ""
					: `:${payload.from?.name ?? payload.from?.sessionId ?? "unknown"}`
			}]`
		: undefined;
	const lines = [
		...(relationshipLabel ? [relationshipLabel] : []),
		"Agent-to-agent message received.",
		`Source: ${payload.source}`,
	];
	if (payload.from) {
		lines.push(`From: ${formatRef(payload.from)}`);
	}
	lines.push(`To: ${formatRef(payload.target)}`);
	lines.push(`Message id: ${payload.id}`);
	lines.push("");
	lines.push(payload.message);
	return lines.join("\n");
}

/** createAgentSessionMessage: ported from prime-agent. */
export function createAgentMessage(
	payload: AgentMessagePayload,
	timestamp = Date.now(),
): {
	role: "custom";
	customType: typeof AGENT_MESSAGE_CUSTOM_TYPE;
	content: string;
	display: boolean;
	details: AgentMessageDetails;
	timestamp: number;
} {
	return {
		role: "custom",
		customType: AGENT_MESSAGE_CUSTOM_TYPE,
		content: createAgentMessagePrompt(payload),
		display: true,
		details: {
			id: payload.id,
			message: payload.message,
			from: payload.from,
			fromRelationship: payload.fromRelationship,
			target: payload.target,
		},
		timestamp,
	};
}

/** createAgentSessionMessageReceipt: ported from prime-agent (M2 status set). */
export function createAgentMessageReceipt(
	payload: AgentMessagePayload,
	status: DeliveryStatus,
	options: { deliveryMode?: "steer" | "triggerTurn" | "deferred"; settled?: boolean; error?: string } = {},
	at = new Date().toISOString(),
): AgentMessageReceipt {
	return {
		id: payload.id,
		source: payload.source,
		target: payload.target,
		from: payload.from,
		message: payload.message,
		deliveryStatus: status,
		...(status === "delivered" || status === "persisted" ? { deliveredAt: at } : { queuedAt: at }),
		...(options.deliveryMode ? { deliveryMode: options.deliveryMode } : {}),
		...(options.settled !== undefined ? { settled: options.settled } : {}),
		...(options.error ? { error: options.error } : {}),
	};
}

export function newAgentMessageId(): string {
	return `agentmsg_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}
