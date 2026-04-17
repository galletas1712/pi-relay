import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@pi-relay/coding-agent";
import type { ReportToolRuntime } from "../types.js";

export function createReportTool(runtime: ReportToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "report",
		label: "Report Progress",
		description: "Send a significant finding or result to your parent without stopping your own work.",
		promptSnippet: "Report a significant finding or completed result to your parent when it should influence coordination or be preserved before you go idle.",
		promptGuidelines: [
			"Use report when your parent would benefit from a solid intermediate finding or a completed result before or as you finish.",
			"Use it when the update should change what the parent or sibling agents do now, such as reprioritization, blocker handling, or avoiding duplicate work.",
			"If you are finishing and produced a substantive result your parent will need, send one concise report before going idle.",
			"If the update would not change current parent behavior and is not a substantive result your parent needs, hold it for later.",
			"Do not use report for routine status updates or tiny incremental findings.",
			"Prefer a single final report near the end unless the parent needs an update now.",
			"Do not rely on idle to carry your actual result to the parent.",
		],
		parameters: Type.Object(
			{
				content: Type.String({ description: "Intermediate finding or blocker to send to your parent." }),
			},
			{ additionalProperties: false },
		),
		async execute(_toolCallId, params) {
			await runtime.handleReport(callingAgentId, params.content);
			return {
				content: [{ type: "text", text: "Report queued for parent." }],
				details: {},
			};
		},
	});
}
