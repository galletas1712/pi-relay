import type { AgentMessageDetails, AgentWorklogDetails, SessionCustomMessage } from "./types.js";

function formatSender(fromAgentId: string, fromRole: string): string {
	return `Agent ${fromAgentId} (${fromRole})`;
}

function truncate(text: string | undefined, maxLength: number): string | undefined {
	if (!text) {
		return undefined;
	}

	const normalized = text.replace(/\s+/g, " ").trim();
	if (normalized.length <= maxLength) {
		return normalized;
	}
	return `${normalized.slice(0, maxLength - 3)}...`;
}

export function createAgentReportMessage(
	fromAgentId: string,
	fromRole: string,
	content: string,
): SessionCustomMessage<AgentMessageDetails> {
	return {
		customType: "agent_report",
		content: `[${formatSender(fromAgentId, fromRole)} REPORT]\n${content}`,
		display: true,
		details: {
			fromAgentId,
			fromRole,
		},
	};
}

export function createAgentDirectiveMessage(
	fromAgentId: string,
	fromRole: string,
	content: string,
): SessionCustomMessage<AgentMessageDetails> {
	return {
		customType: "agent_directive",
		content: `[${formatSender(fromAgentId, fromRole)} DIRECTIVE]\n${content}`,
		display: true,
		details: {
			fromAgentId,
			fromRole,
		},
	};
}

export function createAgentIdleMessage(
	fromAgentId: string,
	fromRole: string,
	lastOutput?: string,
	errorMessage?: string,
): SessionCustomMessage<AgentMessageDetails & { lastOutput?: string; errorMessage?: string }> {
	const lines = [`[${formatSender(fromAgentId, fromRole)} IDLE]`];
	const truncated = truncate(lastOutput, 300);
	if (truncated) {
		lines.push(`Last output: ${truncated}`);
	}
	if (errorMessage) {
		lines.push(`Error: ${errorMessage}`);
	}
	if (!truncated && !errorMessage) {
		lines.push("No final output was captured.");
	}
	return {
		customType: "agent_idle",
		content: lines.join("\n"),
		display: true,
		details: {
			fromAgentId,
			fromRole,
			lastOutput: truncated,
			errorMessage,
		},
	};
}

export function createAgentWorklogMessage(
	fromAgentId: string,
	fromRole: string,
	content: string,
	worklogFile: string,
	turn: number,
): SessionCustomMessage<AgentWorklogDetails> {
	return {
		customType: "agent_worklog",
		content: `[${formatSender(fromAgentId, fromRole)} WORKLOG]\n${content}`,
		display: true,
		details: {
			fromAgentId,
			fromRole,
			worklogFile,
			turn,
		},
	};
}

export function createRosterMessage(content: string): SessionCustomMessage {
	return {
		customType: "agent_roster",
		content,
		display: false,
	};
}
