import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@mariozechner/pi-coding-agent";
import type { MessageToolRuntime } from "../types.js";

export function createMessageTool(runtime: MessageToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "message",
		label: "Message Agent",
		description: "Send a directive to one or more child agents.",
		promptSnippet: "Send a follow-up directive to a child agent.",
		promptGuidelines: [
			"Use message to redirect or refine a child agent's work.",
			"Only message your own direct children.",
		],
		parameters: Type.Object(
			{
				to: Type.Union([
					Type.String({ description: "Target child agent ID." }),
					Type.Array(Type.String({ description: "Target child agent ID." })),
				]),
				content: Type.String({ description: "Directive to deliver to the child agent." }),
			},
			{ additionalProperties: false },
		),
		async execute(_toolCallId, params) {
			const targets = Array.isArray(params.to) ? params.to : [params.to];
			for (const target of targets) {
				await runtime.routeMessage(callingAgentId, target, params.content);
			}
			return {
				content: [{ type: "text", text: `Message delivered to ${targets.join(", ")}.` }],
				details: { targets },
			};
		},
	});
}
