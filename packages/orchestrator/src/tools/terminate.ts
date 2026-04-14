import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@mariozechner/pi-coding-agent";
import type { TerminateToolRuntime } from "../types.js";

export function createTerminateTool(runtime: TerminateToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "terminate",
		label: "Terminate Agent",
		description: "Permanently terminate one or more direct child agents and their descendant subtrees.",
		promptSnippet: "Terminate a direct child agent only when its work is hung, obsolete, or should be abandoned permanently.",
		promptGuidelines: [
			"Use terminate only for your own direct children.",
			"Termination is permanent for that child subtree. Terminated agents cannot be reactivated with message.",
			"If you only need to redirect a child, use message instead of terminate.",
		],
		parameters: Type.Object(
			{
				to: Type.Union([
					Type.String({ description: "Target child agent ID." }),
					Type.Array(Type.String({ description: "Target child agent ID." })),
				]),
			},
			{ additionalProperties: false },
		),
		async execute(_toolCallId, params) {
			const targets = Array.isArray(params.to) ? params.to : [params.to];
			for (const target of targets) {
				await runtime.terminateAgent(callingAgentId, target);
			}
			return {
				content: [{ type: "text", text: `Terminated agent ${targets.join(", ")}.` }],
				details: { targets },
			};
		},
	});
}
