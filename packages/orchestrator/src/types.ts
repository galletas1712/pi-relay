import type { Agent, AgentMessage, ThinkingLevel } from "@mariozechner/pi-agent-core";
import type { ImageContent, Model, TextContent } from "@mariozechner/pi-ai";
import type { AgentSessionEvent, ToolDefinition, ToolInfo } from "@mariozechner/pi-coding-agent";

export type AgentStatus = "running" | "idle" | "disposed";
export type AgentDisplayStatus = "starting" | "running" | "waiting" | "idle";

export type AgentCustomType = "agent_report" | "agent_idle" | "agent_directive";

export interface SessionCustomMessage<T = unknown> {
	customType: string;
	content: string | (TextContent | ImageContent)[];
	display: boolean;
	details?: T;
}

export interface AgentSessionManagerHandle {
	getCwd(): string;
	getSessionDir(): string;
	getSessionId(): string;
	getSessionFile(): string | undefined;
	appendMessage(message: AgentMessage): string;
}

export interface AgentSessionHandle {
	agent: Agent;
	extensionRunner?: { emit(event: { type: "session_shutdown" }): Promise<void> };
	model: Model<any> | undefined;
	thinkingLevel: ThinkingLevel;
	isStreaming: boolean;
	isRetrying: boolean;
	isCompacting: boolean;
	sessionManager: AgentSessionManagerHandle;
	sessionId: string;
	sessionFile: string | undefined;
	getAllTools(): ToolInfo[];
	getLastAssistantText(): string | undefined;
	bindExtensions(bindings: object): Promise<void>;
	subscribe(listener: (event: AgentSessionEvent) => void): () => void;
	sendCustomMessage<T = unknown>(
		message: SessionCustomMessage<T>,
		options?: { triggerTurn?: boolean; deliverAs?: "steer" | "followUp" | "nextTurn" },
	): Promise<void>;
	prompt(message: string): Promise<void>;
	abort(): Promise<void>;
	abortCompaction?(): void;
	abortBranchSummary?(): void;
	dispose(): void;
}

export interface SpawnConfig {
	role: string;
	prompt: string;
	tools?: string[];
	model?: Model<any>;
	thinkingLevel?: ThinkingLevel;
}

export interface OrchestratorConfig {
	maxDepth: number;
	maxChildren: number;
	maxActiveAgents: number;
}

export interface AgentMessageDetails {
	fromAgentId: string;
	fromRole: string;
}

export interface ToolCallRecord {
	toolCallId: string;
	agentId: string;
	toolName: string;
	startedAt: number;
	abortController?: AbortController;
	status: "running" | "completed" | "aborted" | "timed_out";
}

export interface AgentTreeMetadataEntry {
	id: string;
	parentId: string | null;
	childIds: string[];
	role: string;
	status: AgentStatus;
	spawnConfig: SpawnConfig;
	sessionFile: string | undefined;
	worklogFile: string;
	createdAt: number;
	lastStatusChange: number;
	lastWorklogTurn: number;
	lastWorklogMessageCount: number;
	turnCount?: number;
}

export interface AgentTreeMetadata {
	sessionId: string;
	agents: Record<string, AgentTreeMetadataEntry>;
}

export interface AgentRecord {
	id: string;
	session: AgentSessionHandle;
	status: AgentStatus;
	parentId: string | null;
	childIds: string[];
	role: string;
	config: SpawnConfig;
	reactivating: boolean;
	worklogFile: string;
	createdAt: number;
	lastStatusChange: number;
	lastWorklogTurn: number;
	lastWorklogMessageCount: number;
	turnCount: number;
	pendingRestoreIdleNotice: boolean;
	orphanedPendingToolCallIds: string[];
	unsubscribe?: () => void;
}

export interface AgentSummary {
	id: string;
	parentId: string | null;
	role: string;
	status: AgentStatus;
	displayStatus: AgentDisplayStatus;
	depth: number;
	childCount: number;
	sessionFile: string | undefined;
	lastOutput: string | undefined;
}

export interface AgentSessionFactoryOptions {
	mode: "spawn" | "restore";
	agentId: string;
	parentId: string | null;
	config: SpawnConfig;
	sessionFile?: string;
	sessionDir?: string;
	customTools: ToolDefinition[];
	parentSession: AgentSessionHandle;
}

export interface CreatedAgentSession {
	session: AgentSessionHandle;
}

export type AgentSessionFactory = (options: AgentSessionFactoryOptions) => Promise<CreatedAgentSession>;

export interface OrchestratorOptions {
	rootSession: AgentSessionHandle;
	sessionFactory: AgentSessionFactory;
	config?: Partial<OrchestratorConfig>;
	rootAgentId?: string;
	rootRole?: string;
	workspaceDir?: string;
}

export interface SpawnToolRuntime {
	spawnAgent(parentId: string, config: SpawnConfig): Promise<string>;
}

export interface MessageToolRuntime {
	routeMessage(fromAgentId: string, targetAgentId: string, content: string): Promise<void>;
}

export interface ChildrenToolRuntime {
	describeChildren(agentId: string): Promise<string>;
}

export interface TerminateToolRuntime {
	terminateAgent(fromAgentId: string, targetAgentId: string): Promise<void>;
}

export interface ReportToolRuntime {
	handleReport(agentId: string, content: string): Promise<void>;
}

export const DEFAULT_ORCHESTRATOR_CONFIG: OrchestratorConfig = {
	maxDepth: 4,
	maxChildren: 8,
	maxActiveAgents: 32,
};
