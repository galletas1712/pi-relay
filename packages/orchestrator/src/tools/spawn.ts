import { Type } from "@sinclair/typebox";
import { defineTool, type ToolDefinition } from "@mariozechner/pi-coding-agent";
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
		],
		parameters: Type.Object(
			{
				role: Type.String({ description: "Human-readable role label for the child agent." }),
				prompt: Type.String({ description: "Initial task description for the child agent." }),
				tools: Type.Optional(Type.Array(Type.String({ description: "Built-in tool name to keep enabled." }))),
			},
			{ additionalProperties: false },
		),
		async execute(_toolCallId, params) {
			const config: SpawnConfig = {
				role: params.role,
				prompt: params.prompt,
				tools: params.tools,
			};
			const agentId = await runtime.spawnAgent(callingAgentId, config);
			return {
				content: [{ type: "text", text: `Spawned agent ${agentId} (${params.role}).` }],
				details: { agentId },
			};
		},
	});
}
