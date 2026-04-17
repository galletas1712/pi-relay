import type { ExtensionFactory } from "@pi-relay/coding-agent";
import type { Orchestrator } from "./orchestrator.js";

export function createOrchestratorExtension(
	orchestratorRef: { current?: Orchestrator },
): ExtensionFactory {
	return (pi) => {
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
