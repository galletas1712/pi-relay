import type { AgentMessage } from "@mariozechner/pi-agent-core";
import { isPendingToolResult } from "@mariozechner/pi-agent-core";
import type { Orchestrator } from "./orchestrator.js";

export function createAgentContextTransform(
	orchestrator: Orchestrator,
	agentId: string,
	innerTransform?: (messages: AgentMessage[], signal?: AbortSignal) => Promise<AgentMessage[]>,
): (messages: AgentMessage[], signal?: AbortSignal) => Promise<AgentMessage[]> {
	return async (messages, signal) => {
		const transformed = innerTransform ? await innerTransform(messages, signal) : messages;
		const record = orchestrator.getRecord(agentId);
		const orphanedPendingToolCallIds = new Set(record.orphanedPendingToolCallIds);
		const annotated =
			orphanedPendingToolCallIds.size === 0
				? transformed
				: transformed.map((message) => {
						if (!isPendingToolResult(message) || !orphanedPendingToolCallIds.has(message.toolCallId)) {
							return message;
						}

							return {
								...message,
								content: [
									{
										type: "text" as const,
										text: `[INTERRUPTED] ${message.toolName} did not finish before the session ended. It may still be running if the process was killed abruptly. Inspect or re-run it if you still need the result.`,
									},
								],
							};
					});
		return annotated;
	};
}
