import { AgentRpcClient, defaultWsUrl, type ConnectionStatus, type RpcClient } from "./rpc.ts";
import type {
	Activity,
	ActiveBranchSyncResponse,
	ContentBlock,
	SystemPromptResponse,
	EventFrame,
	HistoryTree,
	InputPriority,
	Project,
	ProviderConfig,
	QueueProjection,
	QueuedInputStatus,
	SessionSnapshot,
	SessionSummary,
	ToolListing,
	TranscriptEntriesResult,
	TranscriptEntry,
	TranscriptTreeIndex,
	TranscriptItem,
	TranscriptTurnDetailResult,
	TranscriptTurnsResult,
	ProjectWorkspace,
	WorkSessionsResult,
} from "./types.ts";
import type { EntryScope } from "./queryKeys.ts";

type EventHandler = (event: EventFrame) => void;
type StatusHandler = (status: ConnectionStatus) => void;

export interface AgentApi {
	connect(): Promise<void>;
	close(): void;
	isOpen(): boolean;
	onEvent(handler: EventHandler): () => void;
	onStatus(handler: StatusHandler): () => void;
	listProjects(): Promise<Project[]>;
	createProject(params: CreateProjectParams): Promise<Project>;
	updateProject(params: UpdateProjectParams): Promise<Project>;
	deleteProject(projectId: string): Promise<DeleteProjectResult>;
	listSessions(limit?: number, projectId?: string | null): Promise<SessionSummary[]>;
	getSystemPrompt(sessionId: string): Promise<SystemPromptResponse>;
	listTools(provider: string): Promise<ToolListing[]>;
	getSession(sessionId: string, options?: GetSessionOptions): Promise<SessionSnapshot>;
	syncActiveBranch(sessionId: string, baseLeafId: string | null): Promise<ActiveBranchSyncResponse>;
	getTranscriptIndex(sessionId: string, options?: TranscriptIndexOptions): Promise<TranscriptTreeIndex>;
	getTranscriptEntries(sessionId: string, entryIds: string[]): Promise<TranscriptEntriesResult>;
	getTranscriptTurns(sessionId: string, options?: TranscriptTurnsOptions): Promise<TranscriptTurnsResult>;
	getTranscriptTurnDetail(sessionId: string, request: TranscriptTurnDetailRequest): Promise<TranscriptTurnDetailResult>;
	listWorkSessions(sessionId: string): Promise<WorkSessionsResult>;
	steerSubagent(params: SteerSubagentParams): Promise<SteerSubagentResult>;
	getHistoryTree(sessionId: string): Promise<HistoryTree>;
	subscribeEvents(sessionId: string, afterEventId: number | null): Promise<EventFrame[]>;
	unsubscribeEvents(sessionId: string): Promise<void>;
	startSession(params: StartSessionParams): Promise<StartSessionResult>;
	queueFollowUp(params: QueueFollowUpParams): Promise<FollowUpResult>;
	interrupt(sessionId: string): Promise<InterruptResult>;
	resumeTurn(params: ResumeTurnParams): Promise<ResumeTurnResult>;
	switchHistory(params: SwitchHistoryParams): Promise<SwitchHistoryResult>;
	renameSession(sessionId: string, title: string): Promise<RenameSessionResult>;
	deleteSession(sessionId: string): Promise<DeleteSessionResult>;
	configureSession(params: ConfigureSessionParams): Promise<ConfigureSessionResult>;
	promoteQueuedInput(sessionId: string, inputId: string): Promise<PromoteQueuedResult>;
	updateQueuedInput(sessionId: string, inputId: string, content: ContentBlock[], expectedQueueRevision?: number | null): Promise<UpdateQueuedResult>;
	cancelQueuedInput(sessionId: string, inputId: string, expectedQueueRevision?: number | null): Promise<CancelQueuedResult>;
	reorderQueuedFollowUps(sessionId: string, inputIds: string[], expectedQueueRevision?: number | null): Promise<ReorderQueuedResult>;
	requestCompaction(sessionId: string): Promise<{ action_row_id: string | null }>;
	getHistoryContext(sessionId: string, leafId?: string): Promise<TranscriptItem[]>;
}

export interface SteerSubagentResult {
	parent_session_id: string;
	child_session_id: string;
	input_id?: string;
	queued?: boolean;
	replayed?: boolean;
	queue?: QueueProjection | null;
}

export interface CreateProjectParams {
	name: string;
	workspaces: ProjectWorkspace[];
	metadata?: Record<string, unknown>;
}

export interface UpdateProjectParams {
	projectId: string;
	name?: string;
	workspaces?: ProjectWorkspace[];
}

export interface DeleteProjectResult {
	project_id: string;
	deleted: boolean;
}

export interface GetSessionOptions {
	includeEntries?: boolean;
	entryScope?: EntryScope;
}

export interface TranscriptIndexOptions {
	afterSequence?: number | null;
	limit?: number | null;
}

export interface TranscriptTurnsOptions {
	beforeEntryId?: string | null;
	limit?: number | null;
}

export interface TranscriptTurnDetailRequest {
	cardId: string;
	leafId: string;
	startSequence: number;
	endSequence: number;
}

export interface StartSessionWorkspace {
	workspaceDir: string;
	branch?: string | null;
}

export interface SteerSubagentParams {
	parentSessionId: string;
	childSessionId: string;
	message: string;
	priority?: InputPriority;
}

export interface StartSessionParams {
	sessionId: string;
	projectId?: string | null;
	provider: ProviderConfig;
	metadata: Record<string, unknown>;
	clientInputId: string;
	priority: InputPriority;
	content: ContentBlock[];
	/** Subset of the project's workspaces to materialize, with optional per-session git branch overrides. Omit for all. */
	workspaces?: StartSessionWorkspace[] | null;
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
	baseLeafId?: string | null;
	content: ContentBlock[];
}

export interface FollowUpResult {
	input_id?: string;
	accepted?: boolean;
	queued?: boolean;
	replayed?: boolean;
	queue?: QueueProjection | null;
	active_branch?: SwitchHistoryResult | null;
	active_branch_sync?: ActiveBranchSyncResponse | null;
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

export interface SwitchHistoryResult {
	session_id: string;
	active_leaf_id: string | null;
	activity?: Activity;
	session_revision?: number;
	queue_revision?: number;
	transcript_revision?: number;
	last_event_id?: number;
	active_branch_entry_ids?: string[] | null;
	active_branch_entries?: TranscriptEntry[] | null;
}

export interface PromoteQueuedResult {
	input_id: string;
	priority: InputPriority;
	status: QueuedInputStatus;
	promoted: boolean;
	queue?: QueueProjection;
}

export interface UpdateQueuedResult {
	input_id: string;
	updated: boolean;
	reason?: string | null;
	priority: InputPriority;
	status: QueuedInputStatus;
	queue: QueueProjection;
}

export interface CancelQueuedResult {
	input_id: string;
	cancelled: boolean;
	reason?: string | null;
	priority: InputPriority;
	status: QueuedInputStatus;
	queue: QueueProjection;
}

export interface ReorderQueuedResult {
	reordered: boolean;
	reason?: string | null;
	input_ids: string[];
	queue: QueueProjection;
}

export interface RenameSessionResult {
	session_id: string;
	title: string;
	activity: Activity;
	metadata?: Record<string, unknown>;
}

export interface DeleteSessionResult {
	session_id: string;
	deleted: boolean;
}

export interface ConfigureSessionResult {
	session_id: string;
	activity: Activity;
	provider?: ProviderConfig;
	metadata?: Record<string, unknown>;
}

export interface SwitchHistoryParams {
	sessionId: string;
	leafId: string | null;
	expectedActiveLeafId: string | null;
	returnActiveBranch?: boolean;
	expectedTranscriptRevision?: number | null;
	activeBranchEntryIds?: string[];
	missingBodyIds?: string[];
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

	async listProjects(): Promise<Project[]> {
		const result = await this.client.request<{ projects: Project[] }>("project.list");
		return result.projects;
	}

	createProject(params: CreateProjectParams): Promise<Project> {
		return this.client.request<Project>("project.create", {
			name: params.name,
			workspaces: params.workspaces,
			metadata: params.metadata
		});
	}

	updateProject(params: UpdateProjectParams): Promise<Project> {
		return this.client.request<Project>("project.update", {
			project_id: params.projectId,
			name: params.name,
			workspaces: params.workspaces
		});
	}

	deleteProject(projectId: string): Promise<DeleteProjectResult> {
		return this.client.request<DeleteProjectResult>("project.delete", {
			project_id: projectId
		});
	}

	async listSessions(limit = 100, projectId: string | null = null): Promise<SessionSummary[]> {
		const result = await this.client.request<{ sessions: SessionSummary[] }>("session.list", {
			limit,
			project_id: projectId || undefined
		});
		return result.sessions;
	}

	getSystemPrompt(sessionId: string): Promise<SystemPromptResponse> {
		return this.client.request<SystemPromptResponse>("system.prompt", {
			session_id: sessionId
		});
	}

	async listTools(provider: string): Promise<ToolListing[]> {
		const result = await this.client.request<{ tools: ToolListing[] }>("tools.list", { provider });
		return result.tools;
	}

	getSession(sessionId: string, options: GetSessionOptions = {}): Promise<SessionSnapshot> {
		return this.client.request<SessionSnapshot>("session.get", {
			session_id: sessionId,
			include_entries: options.includeEntries || undefined,
			entries_scope: options.entryScope
		});
	}

	syncActiveBranch(sessionId: string, baseLeafId: string | null): Promise<ActiveBranchSyncResponse> {
		return this.client.request<ActiveBranchSyncResponse>("session.sync_active_branch", {
			session_id: sessionId,
			base_leaf_id: baseLeafId,
		});
	}

	getTranscriptIndex(sessionId: string, options: TranscriptIndexOptions = {}): Promise<TranscriptTreeIndex> {
		return this.client.request<TranscriptTreeIndex>("transcript.index", {
			session_id: sessionId,
			after_sequence: options.afterSequence ?? undefined,
			limit: options.limit ?? undefined
		});
	}

	getTranscriptEntries(sessionId: string, entryIds: string[]): Promise<TranscriptEntriesResult> {
		return this.client.request<TranscriptEntriesResult>("transcript.entries", {
			session_id: sessionId,
			entry_ids: entryIds
		});
	}

	getTranscriptTurns(sessionId: string, options: TranscriptTurnsOptions = {}): Promise<TranscriptTurnsResult> {
		return this.client.request<TranscriptTurnsResult>("transcript.turns", {
			session_id: sessionId,
			before_entry_id: options.beforeEntryId ?? undefined,
			limit: options.limit ?? undefined
		});
	}

	getTranscriptTurnDetail(sessionId: string, request: TranscriptTurnDetailRequest): Promise<TranscriptTurnDetailResult> {
		return this.client.request<TranscriptTurnDetailResult>("transcript.turn_detail", {
			session_id: sessionId,
			card_id: request.cardId,
			leaf_id: request.leafId,
			start_sequence: request.startSequence,
			end_sequence: request.endSequence
		});
	}

	listWorkSessions(sessionId: string): Promise<WorkSessionsResult> {
		return this.client.request<WorkSessionsResult>("work.read", {
			source_session_id: sessionId,
			view: "sessions",
			scope: "mine"
		});
	}

	steerSubagent(params: SteerSubagentParams): Promise<SteerSubagentResult> {
		return this.client.request<SteerSubagentResult>("work.send", {
			source_session_id: params.parentSessionId,
			to: params.childSessionId,
			message: params.message,
			priority: params.priority ?? "steer"
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
			project_id: params.projectId || undefined,
			provider: params.provider,
			metadata: params.metadata,
			client_input_id: params.clientInputId,
			priority: params.priority,
			content: params.content,
			workspaces: params.workspaces?.length
				? params.workspaces.map((workspace) => ({
						workspace_dir: workspace.workspaceDir,
						branch: workspace.branch?.trim() || undefined
					}))
				: undefined
		});
	}

	queueFollowUp(params: QueueFollowUpParams): Promise<FollowUpResult> {
		return this.client.request<FollowUpResult>("input.follow_up", {
			session_id: params.sessionId,
			client_input_id: params.clientInputId,
			expected_active_leaf_id: params.expectedActiveLeafId,
			base_leaf_id: params.baseLeafId,
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

	switchHistory(params: SwitchHistoryParams): Promise<SwitchHistoryResult> {
		return this.client.request<SwitchHistoryResult>("history.switch", {
			session_id: params.sessionId,
			leaf_id: params.leafId,
			expected_active_leaf_id: params.expectedActiveLeafId,
			return_active_branch: params.returnActiveBranch || undefined,
			expected_transcript_revision: params.expectedTranscriptRevision ?? undefined,
			active_branch_entry_ids: params.activeBranchEntryIds,
			missing_body_ids: params.missingBodyIds
		});
	}

	renameSession(sessionId: string, title: string): Promise<RenameSessionResult> {
		return this.client.request<RenameSessionResult>("session.rename", {
			session_id: sessionId,
			title
		});
	}

	deleteSession(sessionId: string): Promise<DeleteSessionResult> {
		return this.client.request<DeleteSessionResult>("session.delete", {
			session_id: sessionId
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

	updateQueuedInput(sessionId: string, inputId: string, content: ContentBlock[], expectedQueueRevision?: number | null): Promise<UpdateQueuedResult> {
		return this.client.request<UpdateQueuedResult>("input.update_queued", {
			session_id: sessionId,
			input_id: inputId,
			expected_queue_revision: expectedQueueRevision ?? undefined,
			content
		});
	}

	cancelQueuedInput(sessionId: string, inputId: string, expectedQueueRevision?: number | null): Promise<CancelQueuedResult> {
		return this.client.request<CancelQueuedResult>("input.cancel_queued", {
			session_id: sessionId,
			input_id: inputId,
			expected_queue_revision: expectedQueueRevision ?? undefined
		});
	}

	reorderQueuedFollowUps(sessionId: string, inputIds: string[], expectedQueueRevision?: number | null): Promise<ReorderQueuedResult> {
		return this.client.request<ReorderQueuedResult>("input.reorder_queued_follow_ups", {
			session_id: sessionId,
			expected_queue_revision: expectedQueueRevision ?? undefined,
			input_ids: inputIds
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
