import type {
	OrchestratorAgentSnapshot,
	OrchestratorAgentStatus,
	OrchestratorPendingSpawnDraft,
	OrchestratorTreeSnapshot,
} from "@pi-relay/agent-protocol";
import type { OrchestratorCoreState } from "./domain/state.js";

export interface OrchestratorCoreAgentSummary {
	id: string;
	parentId: string | null;
	role: string;
	status: OrchestratorAgentStatus;
	depth: number;
	childCount: number;
}

export interface SpawnAllowanceConfig {
	maxDepth: number;
	maxChildren: number;
	maxActiveAgents: number;
}

export type SpawnAllowanceReason = "missing-parent" | "max-children" | "max-depth" | "max-active-agents";

export interface SpawnAllowance {
	allowed: boolean;
	reason?: SpawnAllowanceReason;
	activeChildCount: number;
	pendingDirectChildren: number;
	activeAgentCount: number;
	pendingSpawnCount: number;
	depth: number;
}

export interface SiblingBatchEntry {
	id: string;
	role: string;
	prompt: string;
	status: OrchestratorAgentStatus | "spawning";
}

export interface UsageSnapshotLike {
	sessionFile: string | undefined;
	sessionId: string;
	userMessages: number;
	assistantMessages: number;
	toolCalls: number;
	toolResults: number;
	totalMessages: number;
	tokens: {
		input: number;
		output: number;
		cacheRead: number;
		cacheWrite: number;
		total: number;
	};
	cost: number;
	contextUsage?: unknown;
}

export interface UsageTotals {
	userMessages: number;
	assistantMessages: number;
	toolCalls: number;
	toolResults: number;
	totalMessages: number;
	input: number;
	output: number;
	cacheRead: number;
	cacheWrite: number;
	totalTokens: number;
	cost: number;
}

export interface UsageAggregation<TStats extends UsageSnapshotLike> {
	agentId: string;
	self: TStats;
	totals: UsageTotals;
	descendantCount: number;
}

function cloneAgent<TSpawnConfig>(agent: OrchestratorAgentSnapshot<TSpawnConfig>): OrchestratorAgentSnapshot<TSpawnConfig> {
	return {
		...agent,
		childIds: [...agent.childIds],
	};
}

function clonePendingDrafts(drafts: OrchestratorPendingSpawnDraft[]): OrchestratorPendingSpawnDraft[] {
	return drafts.map((draft) => ({ ...draft }));
}

export function toTreeSnapshot<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
): OrchestratorTreeSnapshot<TSpawnConfig> {
	const agents: Record<string, OrchestratorAgentSnapshot<TSpawnConfig>> = {};
	for (const [agentId, agent] of Object.entries(state.agents)) {
		agents[agentId] = cloneAgent(agent);
	}
	return {
		sessionId: state.sessionId,
		agents,
	};
}

export function getAgent<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
	agentId: string,
): OrchestratorAgentSnapshot<TSpawnConfig> | undefined {
	return state.agents[agentId];
}

export function getPendingSpawnDrafts(
	state: OrchestratorCoreState<unknown>,
	parentId: string,
): OrchestratorPendingSpawnDraft[] {
	return clonePendingDrafts(state.pendingSpawnDrafts[parentId] ?? []);
}

export function getAgentDepth<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
	agentId: string,
): number {
	let depth = 0;
	let current: OrchestratorAgentSnapshot<TSpawnConfig> | undefined = state.agents[agentId];
	while (current) {
		depth += 1;
		current = current.parentId ? state.agents[current.parentId] : undefined;
	}
	return depth;
}

export function countRunningChildren<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
	agentId: string,
	excludingAgentId?: string,
): number {
	const agent = state.agents[agentId];
	if (!agent) {
		return 0;
	}
	let count = 0;
	for (const childId of agent.childIds) {
		if (childId === excludingAgentId) {
			continue;
		}
		const child = state.agents[childId];
		if (!child || child.status !== "running") {
			continue;
		}
		count += 1;
	}
	return count;
}

export function hasRunningChildren<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
	agentId: string,
	excludingAgentId?: string,
): boolean {
	return countRunningChildren(state, agentId, excludingAgentId) > 0;
}

export function evaluateSpawnAllowance<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
	parentId: string,
	config: SpawnAllowanceConfig,
): SpawnAllowance {
	const parent = state.agents[parentId];
	const activeChildCount = countRunningChildren(state, parentId);
	const pendingDirectChildren = state.pendingSpawnDrafts[parentId]?.length ?? 0;
	const pendingSpawnCount = Object.values(state.pendingSpawnDrafts).reduce((total, drafts) => total + drafts.length, 0);
	const activeAgentCount = Object.values(state.agents).filter((agent) => agent.status === "running").length;
	const depth = parent ? getAgentDepth(state, parentId) : 0;

	if (!parent) {
		return {
			allowed: false,
			reason: "missing-parent",
			activeChildCount,
			pendingDirectChildren,
			activeAgentCount,
			pendingSpawnCount,
			depth,
		};
	}

	if (activeChildCount + pendingDirectChildren >= config.maxChildren) {
		return {
			allowed: false,
			reason: "max-children",
			activeChildCount,
			pendingDirectChildren,
			activeAgentCount,
			pendingSpawnCount,
			depth,
		};
	}

	if (depth >= config.maxDepth) {
		return {
			allowed: false,
			reason: "max-depth",
			activeChildCount,
			pendingDirectChildren,
			activeAgentCount,
			pendingSpawnCount,
			depth,
		};
	}

	if (activeAgentCount + pendingSpawnCount >= config.maxActiveAgents) {
		return {
			allowed: false,
			reason: "max-active-agents",
			activeChildCount,
			pendingDirectChildren,
			activeAgentCount,
			pendingSpawnCount,
			depth,
		};
	}

	return {
		allowed: true,
		activeChildCount,
		pendingDirectChildren,
		activeAgentCount,
		pendingSpawnCount,
		depth,
	};
}

export function buildAgentSummaries<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig>,
	rootAgentId: string,
): OrchestratorCoreAgentSummary[] {
	const summaries: OrchestratorCoreAgentSummary[] = [];
	const visit = (agentId: string, depth: number) => {
		const agent = state.agents[agentId];
		if (!agent || agent.status === "disposed") {
			return;
		}
		summaries.push({
			id: agent.id,
			parentId: agent.parentId,
			role: agent.role,
			status: agent.status,
			depth,
			childCount: agent.childIds.length,
		});
		for (const childId of agent.childIds) {
			visit(childId, depth + 1);
		}
	};

	visit(rootAgentId, 0);
	return summaries;
}

export function buildSiblingBatchEntries<TSpawnConfig extends { role: string; prompt: string }>(
	state: OrchestratorCoreState<TSpawnConfig>,
	parentId: string,
	agentId: string,
): SiblingBatchEntry[] {
	const entries: SiblingBatchEntry[] = [];
	const parent = state.agents[parentId];
	for (const childId of parent?.childIds ?? []) {
		if (childId === agentId) {
			continue;
		}
		const child = state.agents[childId];
		if (!child || child.status === "disposed") {
			continue;
		}
		entries.push({
			id: child.id,
			role: child.role,
			prompt: child.spawnConfig.prompt,
			status: child.status,
		});
	}

	for (const draft of state.pendingSpawnDrafts[parentId] ?? []) {
		if (draft.id === agentId || entries.some((entry) => entry.id === draft.id)) {
			continue;
		}
		entries.push({
			id: draft.id,
			role: draft.role,
			prompt: draft.prompt,
			status: "spawning",
		});
	}

	return entries;
}

export function aggregateUsageTotals<TSpawnConfig = unknown, TStats extends UsageSnapshotLike = UsageSnapshotLike>(
	state: OrchestratorCoreState<TSpawnConfig>,
	agentId: string,
	statsByAgentId: Record<string, TStats>,
): UsageAggregation<TStats> | undefined {
	const root = state.agents[agentId];
	const self = statsByAgentId[agentId];
	if (!root || !self) {
		return undefined;
	}

	const visited = new Set<string>([agentId]);
	const totals: UsageTotals = {
		userMessages: self.userMessages,
		assistantMessages: self.assistantMessages,
		toolCalls: self.toolCalls,
		toolResults: self.toolResults,
		totalMessages: self.totalMessages,
		input: self.tokens.input,
		output: self.tokens.output,
		cacheRead: self.tokens.cacheRead,
		cacheWrite: self.tokens.cacheWrite,
		totalTokens: self.tokens.total,
		cost: self.cost,
	};
	let descendantCount = 0;

	const visit = (currentId: string) => {
		const current = state.agents[currentId];
		if (!current) {
			return;
		}
		for (const childId of current.childIds) {
			if (visited.has(childId)) {
				continue;
			}
			const child = state.agents[childId];
			if (!child || child.status === "disposed") {
				continue;
			}
			const childStats = statsByAgentId[childId];
			if (!childStats) {
				continue;
			}
			visited.add(childId);
			descendantCount += 1;
			totals.userMessages += childStats.userMessages;
			totals.assistantMessages += childStats.assistantMessages;
			totals.toolCalls += childStats.toolCalls;
			totals.toolResults += childStats.toolResults;
			totals.totalMessages += childStats.totalMessages;
			totals.input += childStats.tokens.input;
			totals.output += childStats.tokens.output;
			totals.cacheRead += childStats.tokens.cacheRead;
			totals.cacheWrite += childStats.tokens.cacheWrite;
			totals.totalTokens += childStats.tokens.total;
			totals.cost += childStats.cost;
			visit(childId);
		}
	};

	visit(agentId);
	return {
		agentId,
		self,
		totals,
		descendantCount,
	};
}
