import type {
	AgentMessage,
	AgentState,
	BackgroundToolEndContext,
	BackgroundToolStartContext,
	StreamFn,
	ThinkingLevel,
} from "@pi-relay/agent-core";
import type { ImageContent, Message, Model, SimpleStreamOptions, TextContent, ThinkingBudgets, Transport, Usage } from "@pi-relay/ai";
import type { AgentSessionEvent, BackgroundUsageScope, PromptSource, SessionStats, ToolDefinition, ToolInfo } from "@pi-relay/coding-agent";

export type { BackgroundUsageScope, SessionStats };

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
	/**
	 * Attribute usage from a background (out-of-band) LLM call — worklog forks,
	 * compaction summaries, branch/turn-prefix summaries — to this session so
	 * the tokens/cost flow through `getSessionStats`, the TUI footer, the
	 * `[pi:cache]` telemetry, and orchestrator subtree aggregation.
	 *
	 * Implementations persist only in memory. See `AgentSession.addBackgroundUsage`.
	 */
	addBackgroundUsage(usage: Usage, scope?: BackgroundUsageScope): void;
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
	/**
	 * Optional list of topic slugs the child should focus on. When
	 * provided, the ancestor-worklog injection filters entries whose
	 * `meta.topics` intersect with this set. Legacy entries (no topics
	 * field) and pinned entries bypass the filter. An empty or missing
	 * array means no topic filtering (pre-PR-7 behavior).
	 */
	topics?: string[];
	/**
	 * Optional parent-authored handoff context prepended to the child's
	 * initial prompt inside a `<parent-handoff>` block. Use this when the
	 * ancestor worklog doesn't yet cover the critical context the child
	 * needs to start on-task. Empty strings are treated as absent.
	 */
	handoff?: string;
}

export interface OrchestratorConfig {
	maxDepth: number;
	maxChildren: number;
	maxActiveAgents: number;
	/**
	 * Optional override for the model used by worklog forks. The worklog
	 * fork is a short, off-transcript LLM call whose job is to decide
	 * whether the last turn produced anything durable and, if so, emit a
	 * short markdown entry. It tolerates a cheaper/smaller model than the
	 * main agent loop. When unset, forks fall back to the parent session's
	 * `model`, preserving pre-existing behavior.
	 */
	forkModel?: Model<any>;
	/**
	 * Optional override for the reasoning effort / thinking level of the
	 * worklog fork. Unset falls back to the parent session's
	 * `thinkingLevel`. For OpenAI-family reasoning models this is mapped to
	 * `reasoning.effort` via `normalizeOpenAIReasoning`.
	 */
	forkThinkingLevel?: ThinkingLevel;
	/**
	 * Maximum allowed age (in `turnCount` delta) of an ancestor's
	 * `currentFocus` pointer before it is treated as stale and the
	 * spawn-prompt falls back to the raw `<ancestor-recent-context>` tail.
	 * A successful `set_focus_pointer` call refreshes the pointer's `turn`
	 * field; if no fork has emitted one in more than this many turns the
	 * pointer is assumed outdated. Defaults to
	 * {@link DEFAULT_MAX_FOCUS_STALENESS_TURNS}.
	 */
	maxFocusStalenessTurns?: number;
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
	/**
	 * Persisted mirror of {@link AgentRecord.currentFocus}. Written on
	 * successful `set_focus_pointer` tool calls in the worklog fork so
	 * restore round-trips the pointer. Absent on legacy tree.json files
	 * written before PR-8; loaded as `undefined` which makes spawn fall
	 * back to the transcript tail.
	 */
	currentFocus?: { content: string; turn: number };
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
	/**
	 * Compact "what am I working on right now" pointer emitted by the
	 * agent's own worklog fork via the `set_focus_pointer` tool. When set
	 * (and not stale past {@link OrchestratorConfig.maxFocusStalenessTurns}
	 * turns) the spawn prompt prefers this pointer over the raw
	 * `<ancestor-recent-context>` tail. Updated in-memory; mirrored to
	 * {@link AgentTreeMetadataEntry.currentFocus} via `persistTree`.
	 */
	currentFocus?: { content: string; turn: number };
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

/**
 * Default staleness threshold for `AgentRecord.currentFocus`. Measured in
 * `turnCount` delta. A focus pointer older than this many turns is treated
 * as outdated and the spawn prompt falls back to the raw
 * `<ancestor-recent-context>` tail. Chosen to be larger than typical
 * exploration bursts (a handful of tool-only turns) but small enough that
 * a pointer stale for many turns doesn't silently mislead child agents
 * about the parent's current task.
 */
export const DEFAULT_MAX_FOCUS_STALENESS_TURNS = 10;
