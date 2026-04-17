import type { ImageContent, TextContent, ToolResultMessage, UserMessage } from "@pi-relay/ai";
import type { AgentMessage } from "./types.js";

export interface PendingToolResultDetails {
	pending: true;
	argsPreview: string;
}

export interface BackgroundToolCompletionDetails<T = unknown> {
	toolCallId: string;
	toolName: string;
	isError: boolean;
	resultDetails?: T;
	outputPath?: string;
}

export interface BackgroundToolCompletionMessage<T = unknown> {
	role: "custom";
	customType: "bg_tool_completion";
	content: string | (TextContent | ImageContent)[];
	display: boolean;
	details: BackgroundToolCompletionDetails<T>;
	timestamp: number;
}

declare module "./types.js" {
	interface CustomAgentMessages {
		bg_tool_completion: BackgroundToolCompletionMessage;
	}
}

function renderToolTarget(toolName: string, argsPreview: string): string {
	const preview = argsPreview.trim();
	return preview ? `${toolName}(${preview})` : toolName;
}

function toMessageContent(content: string | (TextContent | ImageContent)[]): (TextContent | ImageContent)[] {
	return typeof content === "string" ? [{ type: "text", text: content }] : content;
}

export function createPendingToolResult(toolCallId: string, toolName: string, argsPreview: string): ToolResultMessage {
	return {
		role: "toolResult",
		toolCallId,
		toolName,
		content: [
			{
				type: "text",
				text: `[PENDING] ${renderToolTarget(toolName, argsPreview)} is still running.`,
			},
		],
		details: {
			pending: true,
			argsPreview,
		} satisfies PendingToolResultDetails,
		isError: false,
		timestamp: Date.now(),
	};
}

export function formatBackgroundToolCompletion<T = unknown>(completion: {
	toolCallId: string;
	toolName: string;
	content: (TextContent | ImageContent)[];
	details?: T;
	isError: boolean;
	outputPath?: string;
}): BackgroundToolCompletionMessage<T> {
	const blocks: (TextContent | ImageContent)[] = [
		{
			type: "text",
			text: completion.isError
				? `[Background tool failed] ${completion.toolName} (${completion.toolCallId})`
				: `[Background tool completed] ${completion.toolName} (${completion.toolCallId})`,
		},
		...completion.content,
	];

	if (completion.outputPath) {
		blocks.push({
			type: "text",
			text: `Combined stdout/stderr: ${completion.outputPath}`,
		});
	}

	return {
		role: "custom",
		customType: "bg_tool_completion",
		content: blocks,
		display: true,
		details: {
			toolCallId: completion.toolCallId,
			toolName: completion.toolName,
			isError: completion.isError,
			resultDetails: completion.details,
			outputPath: completion.outputPath,
		},
		timestamp: Date.now(),
	};
}

export function bgCompletionToLlmMessage(message: BackgroundToolCompletionMessage): UserMessage {
	return {
		role: "user",
		content: toMessageContent(message.content),
		timestamp: message.timestamp,
	};
}

export function isBackgroundToolCompletionMessage(message: AgentMessage): message is BackgroundToolCompletionMessage {
	return (
		typeof message === "object" &&
		message !== null &&
		"role" in message &&
		message.role === "custom" &&
		"customType" in message &&
		message.customType === "bg_tool_completion"
	);
}

export function isPendingToolResult(message: AgentMessage): message is ToolResultMessage & { details: PendingToolResultDetails } {
	return (
		typeof message === "object" &&
		message !== null &&
		"role" in message &&
		message.role === "toolResult" &&
		typeof message.details === "object" &&
		message.details !== null &&
		"pending" in message.details &&
		message.details.pending === true
	);
}

export function annotateOrphanedPending(messages: AgentMessage[]): AgentMessage[] {
	const completedToolCalls = new Set(
		messages.flatMap((message) => {
			if (!isBackgroundToolCompletionMessage(message)) {
				return [];
			}
			return [message.details.toolCallId];
		}),
	);

	return messages.map((message) => {
		if (!isPendingToolResult(message) || completedToolCalls.has(message.toolCallId)) {
			return message;
		}

			return {
				...message,
				content: [
					{
						type: "text" as const,
						text: `[INTERRUPTED] ${renderToolTarget(message.toolName, message.details.argsPreview)} did not finish before the session ended. It may still be running if the process was killed abruptly. Inspect or re-run it if you still need the result.`,
					},
				],
			};
	});
}
