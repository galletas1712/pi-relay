import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@mariozechner/pi-coding-agent";
import type { ReportToolRuntime } from "../types.js";

export function createReportTool(runtime: ReportToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "report",
		label: "Report Progress",
		description: "Send a progress report to your parent without stopping your own work.",
		promptSnippet: "Report a useful partial result to your parent agent.",
		promptGuidelines: [
			"Use report when your parent would benefit from an intermediate finding before you finish.",
		],
		parameters: Type.Object(
			{
				content: Type.String({ description: "Progress update or partial result to send to your parent." }),
			},
			{ additionalProperties: false },
		),
		async execute(_toolCallId, params) {
			await runtime.handleReport(callingAgentId, params.content);
			return {
				content: [{ type: "text", text: "Report delivered to parent." }],
				details: {},
			};
		},
	});
}
