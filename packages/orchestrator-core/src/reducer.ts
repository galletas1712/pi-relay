import type { OrchestratorCoreCommand } from "./commands.js";
import type { OrchestratorCoreEffect } from "./effects.js";
import type { OrchestratorCoreEvent } from "./events.js";
import {
	createEmptyOrchestratorCoreState,
	createOrchestratorCoreState,
	type OrchestratorCoreState,
} from "./domain/state.js";
import { toTreeSnapshot } from "./selectors.js";

export interface OrchestratorCoreReduceResult<TSpawnConfig = unknown> {
	state: OrchestratorCoreState<TSpawnConfig>;
	events: OrchestratorCoreEvent<TSpawnConfig>[];
	effects: OrchestratorCoreEffect<TSpawnConfig>[];
}

function cloneState<TSpawnConfig>(state: OrchestratorCoreState<TSpawnConfig>): OrchestratorCoreState<TSpawnConfig> {
	return createOrchestratorCoreState(toTreeSnapshot(state), state.pendingSpawnDrafts);
}

function withEventAndPersist<TSpawnConfig>(
	state: OrchestratorCoreState<TSpawnConfig>,
	event: OrchestratorCoreEvent<TSpawnConfig>,
): OrchestratorCoreReduceResult<TSpawnConfig> {
	const snapshot = toTreeSnapshot(state);
	return {
		state,
		events: [event],
		effects: [
			{ type: "publish_event", event },
			{ type: "persist_tree", snapshot },
		],
	};
}

export function reduceOrchestratorState<TSpawnConfig = unknown>(
	state: OrchestratorCoreState<TSpawnConfig> = createEmptyOrchestratorCoreState<TSpawnConfig>(),
	command: OrchestratorCoreCommand<TSpawnConfig>,
): OrchestratorCoreReduceResult<TSpawnConfig> {
	switch (command.type) {
		case "hydrate": {
			const nextState = createOrchestratorCoreState(command.state.snapshot, command.state.pendingSpawnDrafts);
			return withEventAndPersist(nextState, {
				type: "state_hydrated",
				state: {
					snapshot: toTreeSnapshot(nextState),
					pendingSpawnDrafts: nextState.pendingSpawnDrafts,
				},
			});
		}
		case "register_pending_spawn": {
			const nextState = cloneState(state);
			const drafts = nextState.pendingSpawnDrafts[command.parentId] ?? [];
			nextState.pendingSpawnDrafts[command.parentId] = [...drafts, { ...command.draft }];
			return withEventAndPersist(nextState, {
				type: "pending_spawn_registered",
				parentId: command.parentId,
				draft: { ...command.draft },
			});
		}
		case "drop_pending_spawn": {
			const drafts = state.pendingSpawnDrafts[command.parentId] ?? [];
			if (drafts.length === 0) {
				return { state, events: [], effects: [] };
			}
			const nextState = cloneState(state);
			nextState.pendingSpawnDrafts[command.parentId] = drafts
				.filter((draft) => draft.id !== command.agentId)
				.map((draft) => ({ ...draft }));
			if (nextState.pendingSpawnDrafts[command.parentId]?.length === 0) {
				delete nextState.pendingSpawnDrafts[command.parentId];
			}
			return withEventAndPersist(nextState, {
				type: "pending_spawn_dropped",
				parentId: command.parentId,
				agentId: command.agentId,
			});
		}
		case "upsert_agent": {
			const nextState = cloneState(state);
			nextState.agents[command.agent.id] = {
				...command.agent,
				childIds: [...command.agent.childIds],
			};
			return withEventAndPersist(nextState, {
				type: "agent_upserted",
				agent: {
					...command.agent,
					childIds: [...command.agent.childIds],
				},
			});
		}
		case "replace_agent_children": {
			const existing = state.agents[command.agentId];
			if (!existing) {
				return { state, events: [], effects: [] };
			}
			const nextState = cloneState(state);
			nextState.agents[command.agentId] = {
				...existing,
				childIds: [...command.childIds],
			};
			return withEventAndPersist(nextState, {
				type: "agent_children_replaced",
				agentId: command.agentId,
				childIds: [...command.childIds],
			});
		}
		case "detach_child": {
			const existing = state.agents[command.parentId];
			if (!existing) {
				return { state, events: [], effects: [] };
			}
			const nextState = cloneState(state);
			nextState.agents[command.parentId] = {
				...existing,
				childIds: existing.childIds.filter((childId) => childId !== command.childId),
			};
			return withEventAndPersist(nextState, {
				type: "child_detached",
				parentId: command.parentId,
				childId: command.childId,
			});
		}
		case "set_agent_status": {
			const existing = state.agents[command.agentId];
			if (!existing) {
				return { state, events: [], effects: [] };
			}
			const nextState = cloneState(state);
			nextState.agents[command.agentId] = {
				...existing,
				status: command.status,
				lastStatusChange: command.changedAt,
			};
			return withEventAndPersist(nextState, {
				type: "agent_status_set",
				agentId: command.agentId,
				status: command.status,
			});
		}
		case "record_agent_turn": {
			const existing = state.agents[command.agentId];
			if (!existing) {
				return { state, events: [], effects: [] };
			}
			const nextState = cloneState(state);
			nextState.agents[command.agentId] = {
				...existing,
				turnCount: command.turnCount,
			};
			return withEventAndPersist(nextState, {
				type: "agent_turn_recorded",
				agentId: command.agentId,
				turnCount: command.turnCount,
			});
		}
		case "record_worklog_cursor": {
			const existing = state.agents[command.agentId];
			if (!existing) {
				return { state, events: [], effects: [] };
			}
			const nextState = cloneState(state);
			nextState.agents[command.agentId] = {
				...existing,
				lastWorklogTurn: command.lastWorklogTurn,
				lastWorklogMessageCount: command.lastWorklogMessageCount,
			};
			return withEventAndPersist(nextState, {
				type: "worklog_cursor_recorded",
				agentId: command.agentId,
				lastWorklogTurn: command.lastWorklogTurn,
				lastWorklogMessageCount: command.lastWorklogMessageCount,
			});
		}
	}
}
