import type { ExtensionFactory } from "@mariozechner/pi-coding-agent";
import { buildAgentSystemPrompt } from "./system-prompt.js";
import type { Orchestrator } from "./orchestrator.js";

export function createOrchestratorExtension(orchestratorRef: { current?: Orchestrator }): ExtensionFactory {
	return (pi) => {
		pi.on("before_agent_start", async (event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (!orchestrator) {
				return;
			}

			const agentId = orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId());
			if (!agentId) {
				return;
			}

			const record = orchestrator.getRecord(agentId);
			return {
				systemPrompt: buildAgentSystemPrompt(event.systemPrompt, {
					role: record.role,
					hasParent: record.parentId !== null,
				}),
			};
		});

		pi.on("session_shutdown", async (_event, ctx) => {
			const orchestrator = orchestratorRef.current;
			if (!orchestrator || orchestrator.isDisposing) {
				return;
			}

			const agentId = orchestrator.getAgentIdBySessionId(ctx.sessionManager.getSessionId());
			if (agentId !== orchestrator.rootAgentId) {
				return;
			}

			await orchestrator.dispose();
		});
	};
}
