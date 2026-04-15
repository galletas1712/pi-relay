import type { AgentMessageDetails, SessionCustomMessage } from "./types.js";

function formatSender(fromAgentId: string, fromRole: string): string {
	return `Agent ${fromAgentId} (${fromRole})`;
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
	options: { errorMessage?: string; note?: string } = {},
): SessionCustomMessage<AgentMessageDetails & { errorMessage?: string; note?: string }> {
	const lines = [
		`[${formatSender(fromAgentId, fromRole)} IDLE]`,
		"The child is idle and can be reactivated with `message`.",
	];
	if (options.errorMessage) {
		lines.push(`Error: ${options.errorMessage}`);
	}
	if (options.note) {
		lines.push(`Note: ${options.note}`);
	}
	return {
		customType: "agent_idle",
		content: lines.join("\n"),
		display: true,
		details: {
			fromAgentId,
			fromRole,
			errorMessage: options.errorMessage,
			note: options.note,
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
