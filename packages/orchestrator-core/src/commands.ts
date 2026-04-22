import type { OrchestratorBoundaryCommand } from "@pi-relay/agent-protocol";

export type OrchestratorCoreCommand<TSpawnConfig = unknown> = OrchestratorBoundaryCommand<TSpawnConfig>;
