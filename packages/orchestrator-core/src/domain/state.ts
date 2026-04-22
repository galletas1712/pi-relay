import type {
	OrchestratorAgentSnapshot,
	OrchestratorPendingSpawnDraft,
	OrchestratorTreeSnapshot,
} from "@pi-relay/agent-protocol";

export interface OrchestratorCoreState<TSpawnConfig = unknown> {
	sessionId: string;
	agents: Record<string, OrchestratorAgentSnapshot<TSpawnConfig>>;
	pendingSpawnDrafts: Record<string, OrchestratorPendingSpawnDraft[]>;
}

function cloneAgents<TSpawnConfig>(
	agents: Record<string, OrchestratorAgentSnapshot<TSpawnConfig>>,
): Record<string, OrchestratorAgentSnapshot<TSpawnConfig>> {
	const cloned: Record<string, OrchestratorAgentSnapshot<TSpawnConfig>> = {};
	for (const [agentId, agent] of Object.entries(agents)) {
		cloned[agentId] = {
			...agent,
			childIds: [...agent.childIds],
		};
	}
	return cloned;
}

function clonePendingSpawnDrafts(
	pendingSpawnDrafts: Record<string, OrchestratorPendingSpawnDraft[]>,
): Record<string, OrchestratorPendingSpawnDraft[]> {
	const cloned: Record<string, OrchestratorPendingSpawnDraft[]> = {};
	for (const [parentId, drafts] of Object.entries(pendingSpawnDrafts)) {
		cloned[parentId] = drafts.map((draft) => ({ ...draft }));
	}
	return cloned;
}

export function createEmptyOrchestratorCoreState<TSpawnConfig = unknown>(
	sessionId = "",
): OrchestratorCoreState<TSpawnConfig> {
	return {
		sessionId,
		agents: {},
		pendingSpawnDrafts: {},
	};
}

export function createOrchestratorCoreState<TSpawnConfig = unknown>(
	snapshot: OrchestratorTreeSnapshot<TSpawnConfig>,
	pendingSpawnDrafts: Record<string, OrchestratorPendingSpawnDraft[]> = {},
): OrchestratorCoreState<TSpawnConfig> {
	return {
		sessionId: snapshot.sessionId,
		agents: cloneAgents(snapshot.agents),
		pendingSpawnDrafts: clonePendingSpawnDrafts(pendingSpawnDrafts),
	};
}
