import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@mariozechner/pi-coding-agent";
import type { ChildrenToolRuntime } from "../types.js";

export function createChildrenTool(runtime: ChildrenToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "children",
		label: "List Child Agents",
		description: "List your current direct child agents and their statuses.",
		promptSnippet: "Get a fresh list of your direct child agent IDs and statuses before messaging or terminating them.",
		promptGuidelines: [
			"Use children when you need the current direct-child IDs or statuses.",
			"Children lists only your own direct children.",
			"Call children before terminate if you are unsure which child ID is still active or idle.",
		],
		parameters: Type.Object({}, { additionalProperties: false }),
		async execute() {
			const content = await runtime.describeChildren(callingAgentId);
			return {
				content: [{ type: "text", text: content }],
				details: {},
			};
		},
	});
}
