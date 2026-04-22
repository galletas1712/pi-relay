import type { OrchestratorBoundaryEvent } from "@pi-relay/agent-protocol";

export type OrchestratorCoreEvent<TSpawnConfig = unknown> = OrchestratorBoundaryEvent<TSpawnConfig>;
