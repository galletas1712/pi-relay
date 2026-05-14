import { AgentRpcClient, defaultWsUrl, type ConnectionStatus, type RpcClient } from "./rpc.ts";
import type {
	Activity,
	ContentBlock,
	DaemonConfig,
	EventFrame,
	HistoryTree,
	InputPriority,
	ProviderConfig,
	QueuedInputStatus,
	SessionSnapshot,
	SessionSummary,
	ToolListing,
	TranscriptItem
} from "./types.ts";
import type { HistoryPlacement } from "./historyTargets.ts";

type EventHandler = (event: EventFrame) => void;
type StatusHandler = (status: ConnectionStatus) => void;

export interface AgentApi {
	connect(): Promise<void>;
	close(): void;
	isOpen(): boolean;
	onEvent(handler: EventHandler): () => void;
	onStatus(handler: StatusHandler): () => void;
	listSessions(limit?: number): Promise<SessionSummary[]>;
	getConfig(): Promise<DaemonConfig>;
	setConfig(systemPrompt: string | null): Promise<DaemonConfig>;
	listTools(provider: string): Promise<ToolListing[]>;
	getSession(sessionId: string, options?: GetSessionOptions): Promise<SessionSnapshot>;
	getHistoryTree(sessionId: string): Promise<HistoryTree>;
	subscribeEvents(sessionId: string, afterEventId: number | null): Promise<EventFrame[]>;
	unsubscribeEvents(sessionId: string): Promise<void>;
	startSession(params: StartSessionParams): Promise<StartSessionResult>;
	queueFollowUp(params: QueueFollowUpParams): Promise<FollowUpResult>;
	interrupt(sessionId: string): Promise<InterruptResult>;
	resumeTurn(params: ResumeTurnParams): Promise<ResumeTurnResult>;
	rewindHistory(params: RewindHistoryParams): Promise<RewindHistoryResult>;
	forkHistory(params: ForkHistoryParams): Promise<ForkHistoryResult>;
	renameSession(sessionId: string, title: string): Promise<RenameSessionResult>;
	configureSession(params: ConfigureSessionParams): Promise<ConfigureSessionResult>;
	promoteQueuedInput(sessionId: string, inputId: string): Promise<PromoteQueuedResult>;
	requestCompaction(sessionId: string): Promise<{ action_row_id: string | null }>;
	getHistoryContext(sessionId: string, leafId?: string): Promise<TranscriptItem[]>;
}

export interface GetSessionOptions {
	includeEntries?: boolean;
}

export interface StartSessionParams {
	sessionId: string;
	provider: ProviderConfig;
	metadata: Record<string, unknown>;
	clientInputId: string;
	priority: InputPriority;
	content: ContentBlock[];
}

export interface StartSessionResult {
	session_id: string;
	activity: Activity;
	replayed?: boolean;
}

export interface QueueFollowUpParams {
	sessionId: string;
	clientInputId: string;
	expectedActiveLeafId?: string | null;
	content: ContentBlock[];
}

export interface FollowUpResult {
	input_id?: string;
	accepted?: boolean;
	queued?: boolean;
	replayed?: boolean;
}

export interface InterruptResult {
	interrupted?: boolean;
	ignored?: boolean;
}

export interface ResumeTurnParams {
	sessionId: string;
	leafId?: string | null;
	expectedActiveLeafId?: string | null;
}

export interface ResumeTurnResult {
	session_id: string;
	turn_id: number;
	outcome: "Interrupted" | "Crashed";
	checkpoint_leaf_id: string;
}

export interface RewindHistoryResult {
	session_id: string;
	active_leaf_id: string | null;
}

export interface ForkHistoryResult {
	session_id: string;
	source_leaf_id: string;
	placement: HistoryPlacement;
	active_leaf_id: string | null;
}

export interface PromoteQueuedResult {
	input_id: string;
	priority: InputPriority;
	status: QueuedInputStatus;
	promoted: boolean;
}

export interface RenameSessionResult {
	session_id: string;
	title: string;
	activity: Activity;
}

export interface ConfigureSessionResult {
	session_id: string;
	activity: Activity;
}

export interface RewindHistoryParams {
	sessionId: string;
	leafId: string | null;
	expectedActiveLeafId: string | null;
}

export interface ForkHistoryParams {
	sessionId: string;
	leafId: string | null;
	placement: HistoryPlacement;
}

export interface ConfigureSessionParams {
	sessionId: string;
	provider?: ProviderConfig;
	metadata?: Record<string, unknown>;
}

export function createAgentApi(client: RpcClient = new AgentRpcClient(defaultWsUrl())): AgentApi {
	return new AgentApiClient(client);
}

class AgentApiClient implements AgentApi {
	constructor(private readonly client: RpcClient) {}

	connect(): Promise<void> {
		return this.client.connect();
	}

	close(): void {
		this.client.close();
	}

	isOpen(): boolean {
		return this.client.isOpen();
	}

	onEvent(handler: EventHandler): () => void {
		return this.client.onEvent(handler);
	}

	onStatus(handler: StatusHandler): () => void {
		return this.client.onStatus(handler);
	}

	async listSessions(limit = 100): Promise<SessionSummary[]> {
		const result = await this.client.request<{ sessions: SessionSummary[] }>("session.list", { limit });
		return result.sessions;
	}

	getConfig(): Promise<DaemonConfig> {
		return this.client.request<DaemonConfig>("config.get");
	}

	setConfig(systemPrompt: string | null): Promise<DaemonConfig> {
		return this.client.request<DaemonConfig>("config.set", { system_prompt: systemPrompt });
	}

	async listTools(provider: string): Promise<ToolListing[]> {
		const result = await this.client.request<{ tools: ToolListing[] }>("tools.list", { provider });
		return result.tools;
	}

	getSession(sessionId: string, options: GetSessionOptions = {}): Promise<SessionSnapshot> {
		return this.client.request<SessionSnapshot>("session.get", {
			session_id: sessionId,
			include_entries: options.includeEntries || undefined
		});
	}

	getHistoryTree(sessionId: string): Promise<HistoryTree> {
		return this.client.request<HistoryTree>("history.tree", { session_id: sessionId });
	}

	async subscribeEvents(sessionId: string, afterEventId: number | null): Promise<EventFrame[]> {
		const result = await this.client.request<{ replayed: EventFrame[] }>("events.subscribe", {
			session_id: sessionId,
			after_event_id: afterEventId
		});
		return result.replayed;
	}

	async unsubscribeEvents(sessionId: string): Promise<void> {
		await this.client.request("events.unsubscribe", { session_id: sessionId });
	}

	startSession(params: StartSessionParams): Promise<StartSessionResult> {
		return this.client.request<StartSessionResult>("session.start", {
			session_id: params.sessionId,
			provider: params.provider,
			metadata: params.metadata,
			client_input_id: params.clientInputId,
			priority: params.priority,
			content: params.content
		});
	}

	queueFollowUp(params: QueueFollowUpParams): Promise<FollowUpResult> {
		return this.client.request<FollowUpResult>("input.follow_up", {
			session_id: params.sessionId,
			client_input_id: params.clientInputId,
			expected_active_leaf_id: params.expectedActiveLeafId,
			content: params.content
		});
	}

	interrupt(sessionId: string): Promise<InterruptResult> {
		return this.client.request<InterruptResult>("input.interrupt", { session_id: sessionId });
	}

	resumeTurn(params: ResumeTurnParams): Promise<ResumeTurnResult> {
		return this.client.request<ResumeTurnResult>("turn.resume", {
			session_id: params.sessionId,
			leaf_id: params.leafId,
			expected_active_leaf_id: params.expectedActiveLeafId
		});
	}

	rewindHistory(params: RewindHistoryParams): Promise<RewindHistoryResult> {
		return this.client.request<RewindHistoryResult>("history.rewind", {
			session_id: params.sessionId,
			leaf_id: params.leafId,
			expected_active_leaf_id: params.expectedActiveLeafId
		});
	}

	forkHistory(params: ForkHistoryParams): Promise<ForkHistoryResult> {
		return this.client.request<ForkHistoryResult>("history.fork", {
			session_id: params.sessionId,
			leaf_id: params.leafId,
			placement: params.placement
		});
	}

	renameSession(sessionId: string, title: string): Promise<RenameSessionResult> {
		return this.client.request<RenameSessionResult>("session.rename", {
			session_id: sessionId,
			title
		});
	}

	configureSession(params: ConfigureSessionParams): Promise<ConfigureSessionResult> {
		return this.client.request<ConfigureSessionResult>("session.configure", {
			session_id: params.sessionId,
			provider: params.provider,
			metadata: params.metadata
		});
	}

	promoteQueuedInput(sessionId: string, inputId: string): Promise<PromoteQueuedResult> {
		return this.client.request<PromoteQueuedResult>("input.promote_queued", {
			session_id: sessionId,
			input_id: inputId
		});
	}

	requestCompaction(sessionId: string): Promise<{ action_row_id: string | null }> {
		return this.client.request<{ action_row_id: string | null }>("compaction.request", {
			session_id: sessionId
		});
	}

	async getHistoryContext(sessionId: string, leafId?: string): Promise<TranscriptItem[]> {
		const result = await this.client.request<{ items: TranscriptItem[] }>("history.context", {
			session_id: sessionId,
			leaf_id: leafId || undefined
		});
		return result.items;
	}
}
