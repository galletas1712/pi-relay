import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@pi-relay/coding-agent";
import type { SpawnConfig, SpawnToolRuntime } from "../types.js";

export function createSpawnTool(runtime: SpawnToolRuntime, callingAgentId: string): ToolDefinition {
	return defineTool({
		name: "spawn",
		label: "Spawn Agent",
		description: "Spawn a child agent to work on a subtask independently.",
		promptSnippet: "Spawn a child agent for an independent subtask.",
		promptGuidelines: [
			"Use spawn when a subtask can proceed independently from your current turn.",
			"Give each child a clear role and a concrete prompt.",
			"If you need several independent children, emit all spawn calls in the same assistant response.",
			"Pass `topics` when the child only needs task-relevant history — the system filters ancestor worklog entries by topic intersection (pinned and legacy entries bypass the filter).",
			"Pass `handoff` to hand off compressed context the child needs but that isn't yet in any worklog; it is injected before ancestor content in the child's initial prompt.",
		],
		parameters: Type.Object(
			{
				role: Type.String({ description: "Human-readable role label for the child agent." }),
				prompt: Type.String({ description: "Initial task description for the child agent." }),
				tools: Type.Optional(Type.Array(Type.String({ description: "Built-in tool name to keep enabled." }))),
				topics: Type.Optional(
					Type.Array(
						Type.String({ description: "Topic slug the child should focus on." }),
						{
							description:
								"Topic slugs the child should focus on. When provided, ancestor worklog entries whose `topics` intersect with this set are included; entries with no topics (legacy) are always included; pinned entries always included. Prefer slugs already present in the worklog vocabulary.",
						},
					),
				),
				handoff: Type.Optional(
					Type.String({
						description:
							"Optional parent-authored handoff context. When provided, prepended to the child's initial prompt inside a `<parent-handoff>` block so it appears before ancestor sections. Use this when the ancestor worklog doesn't cover the task's critical context. Empty strings are treated as absent.",
					}),
				),
			},
			{ additionalProperties: false },
		),
		async execute(_toolCallId, params) {
			const config: SpawnConfig = {
				role: params.role,
				prompt: params.prompt,
				tools: params.tools,
				topics: params.topics,
				handoff: params.handoff,
			};
			const agentId = await runtime.spawnAgent(callingAgentId, config);
			return {
				content: [{ type: "text", text: `Spawned agent ${agentId} (${params.role}).` }],
				details: { agentId },
			};
		},
	});
}
