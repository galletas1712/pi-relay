import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@mariozechner/pi-coding-agent";
import type { ReportToolRuntime } from "../types.js";

export function createReportTool(runtime: ReportToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "report",
		label: "Report Progress",
		description: "Send a significant progress report to your parent without stopping your own work.",
		promptSnippet: "Report a significant finding to your parent agent when it cannot wait until the end.",
		promptGuidelines: [
			"Use report only when your parent would benefit from an intermediate finding before you finish.",
			"Do not use report for routine status updates or tiny incremental findings.",
			"Prefer a single final report near the end unless the parent needs an update now.",
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
