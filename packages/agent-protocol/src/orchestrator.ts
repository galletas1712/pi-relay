export type OrchestratorAgentStatus = "running" | "idle" | "disposed";

export interface OrchestratorPendingSpawnDraft {
	id: string;
	role: string;
	prompt: string;
}

export interface OrchestratorAgentSnapshot<TSpawnConfig = unknown> {
	id: string;
	parentId: string | null;
	childIds: string[];
	role: string;
	status: OrchestratorAgentStatus;
	spawnConfig: TSpawnConfig;
	sessionFile: string | undefined;
	worklogFile: string;
	createdAt: number;
	lastStatusChange: number;
	lastWorklogTurn: number;
	lastWorklogMessageCount: number;
	turnCount?: number;
}

export interface OrchestratorTreeSnapshot<TSpawnConfig = unknown> {
	sessionId: string;
	agents: Record<string, OrchestratorAgentSnapshot<TSpawnConfig>>;
}

export interface OrchestratorBoundaryState<TSpawnConfig = unknown> {
	snapshot: OrchestratorTreeSnapshot<TSpawnConfig>;
	pendingSpawnDrafts: Record<string, OrchestratorPendingSpawnDraft[]>;
}

export type OrchestratorBoundaryCommand<TSpawnConfig = unknown> =
	| {
			type: "hydrate";
			state: OrchestratorBoundaryState<TSpawnConfig>;
	  }
	| {
			type: "register_pending_spawn";
			parentId: string;
			draft: OrchestratorPendingSpawnDraft;
	  }
	| {
			type: "drop_pending_spawn";
			parentId: string;
			agentId: string;
	  }
	| {
			type: "upsert_agent";
			agent: OrchestratorAgentSnapshot<TSpawnConfig>;
	  }
	| {
			type: "replace_agent_children";
			agentId: string;
			childIds: string[];
	  }
	| {
			type: "detach_child";
			parentId: string;
			childId: string;
	  }
	| {
			type: "set_agent_status";
			agentId: string;
			status: OrchestratorAgentStatus;
			changedAt: number;
	  }
	| {
			type: "record_agent_turn";
			agentId: string;
			turnCount: number;
	  }
	| {
			type: "record_worklog_cursor";
			agentId: string;
			lastWorklogTurn: number;
			lastWorklogMessageCount: number;
	  };

export type OrchestratorBoundaryEvent<TSpawnConfig = unknown> =
	| {
			type: "state_hydrated";
			state: OrchestratorBoundaryState<TSpawnConfig>;
	  }
	| {
			type: "pending_spawn_registered";
			parentId: string;
			draft: OrchestratorPendingSpawnDraft;
	  }
	| {
			type: "pending_spawn_dropped";
			parentId: string;
			agentId: string;
	  }
	| {
			type: "agent_upserted";
			agent: OrchestratorAgentSnapshot<TSpawnConfig>;
	  }
	| {
			type: "agent_children_replaced";
			agentId: string;
			childIds: string[];
	  }
	| {
			type: "child_detached";
			parentId: string;
			childId: string;
	  }
	| {
			type: "agent_status_set";
			agentId: string;
			status: OrchestratorAgentStatus;
	  }
	| {
			type: "agent_turn_recorded";
			agentId: string;
			turnCount: number;
	  }
	| {
			type: "worklog_cursor_recorded";
			agentId: string;
			lastWorklogTurn: number;
			lastWorklogMessageCount: number;
	  };

export type OrchestratorBoundaryEffect<TSpawnConfig = unknown> =
	| {
			type: "persist_tree";
			snapshot: OrchestratorTreeSnapshot<TSpawnConfig>;
	  }
	| {
			type: "publish_event";
			event: OrchestratorBoundaryEvent<TSpawnConfig>;
	  };
