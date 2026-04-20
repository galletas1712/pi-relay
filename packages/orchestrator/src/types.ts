import type {
	AgentMessage,
	AgentState,
	BackgroundToolEndContext,
	BackgroundToolStartContext,
	StreamFn,
	ThinkingLevel,
} from "@pi-relay/agent-core";
import type { ImageContent, Message, Model, SimpleStreamOptions, TextContent, ThinkingBudgets, Transport } from "@pi-relay/ai";
import type { AgentSessionEvent, PromptSource, SessionStats, ToolDefinition, ToolInfo } from "@pi-relay/coding-agent";

export type { SessionStats };

/**
 * Aggregated usage stats for an agent plus its subtree.
 *
 * `self` reflects only the agent identified by `agentId`; `tree` recursively
 * sums stats across `self` and every descendant in the orchestrator's record
 * graph. Both sides share the exact `SessionStats` shape so existing footer
 * and print-mode formatters can consume either.
 *
 * Session-identifying fields on the tree aggregate are carried from `self`:
 * `sessionId`, `sessionFile`, and `contextUsage` describe the attached agent
 * only. Counts (`userMessages`, `assistantMessages`, `toolCalls`, `toolResults`,
 * `totalMessages`), tokens, and `cost` are summed across the subtree.
 */
export interface SubtreeUsageStats {
	agentId: string;
	hasDescendants: boolean;
	self: SessionStats;
	tree: SessionStats;
}

export type AgentStatus = "running" | "idle" | "disposed";

export type AgentCustomType = "agent_report" | "agent_idle" | "agent_directive" | "agent_roster";

interface RelayCustomMessage<TType extends AgentCustomType, TDetails = unknown> {
	role: "custom";
	customType: TType;
	content: string | (TextContent | ImageContent)[];
	display: boolean;
	details?: TDetails;
	timestamp: number;
}

export interface SessionCustomMessage<T = unknown> {
	customType: string;
	content: string | (TextContent | ImageContent)[];
	display: boolean;
	details?: T;
}

export type AgentReportMessage = RelayCustomMessage<"agent_report", AgentMessageDetails>;
export type AgentDirectiveMessage = RelayCustomMessage<"agent_directive", AgentMessageDetails>;
export type AgentIdleMessage = RelayCustomMessage<
	"agent_idle",
	AgentMessageDetails & { errorMessage?: string; note?: string }
>;
export type AgentRosterMessage = RelayCustomMessage<"agent_roster">;

declare module "@pi-relay/agent-core" {
	interface CustomAgentMessages {
		agent_report: AgentReportMessage;
		agent_directive: AgentDirectiveMessage;
		agent_idle: AgentIdleMessage;
		agent_roster: AgentRosterMessage;
	}
}

export interface AgentHandle {
	state: AgentState;
	transformContext?: (messages: AgentMessage[], signal?: AbortSignal) => Promise<AgentMessage[]>;
	onBackgroundToolStart?: (context: BackgroundToolStartContext, signal?: AbortSignal) => Promise<void> | void;
	onBackgroundToolEnd?: (context: BackgroundToolEndContext, signal?: AbortSignal) => Promise<void> | void;
	convertToLlm(messages: AgentMessage[]): Message[] | Promise<Message[]>;
	getApiKey?: (provider: string) => Promise<string | undefined> | string | undefined;
	onPayload?: SimpleStreamOptions["onPayload"];
	sessionId?: string;
	thinkingBudgets?: ThinkingBudgets;
	transport: Transport;
	maxRetryDelayMs?: number;
	streamFn: StreamFn;
	hasQueuedMessages(): boolean;
	waitForIdle(): Promise<void>;
	mailbox?: {
		close(): void;
	};
}

export interface AgentSessionManagerHandle {
	getCwd(): string;
	getSessionDir(): string;
	getSessionId(): string;
	getSessionFile(): string | undefined;
	appendMessage(message: AgentMessage): string;
}

export interface AgentSessionHandle {
	agent: AgentHandle;
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
	getSessionStats(): SessionStats;
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
	addPromptSource(source: PromptSource): void;
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

export interface ReportToolRuntime {
	handleReport(agentId: string, content: string): Promise<void>;
}

export const DEFAULT_ORCHESTRATOR_CONFIG: OrchestratorConfig = {
	maxDepth: 4,
	maxChildren: 8,
	maxActiveAgents: 32,
};
