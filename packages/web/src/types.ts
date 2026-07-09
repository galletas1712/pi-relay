export type Activity = "idle" | "queued" | "running";
export type InputPriority = "follow_up" | "steer";
export type QueuedInputStatus = "queued" | "consuming" | "consumed" | "cancelled";
export type ActionKind = "model" | "tool" | "compaction";
export type ActionStatus = "pending" | "blocked" | "running" | "completed" | "error" | "interrupted" | "stale";
export type ToolResultStatus = "Success" | "Error" | "Interrupted" | "Crashed";
export type TurnOutcome = "Graceful" | "Interrupted" | "Crashed";
export type ReasoningEffort = "none" | "minimal" | "low" | "medium" | "high" | "xhigh" | "max";

export interface ProviderConfig {
	kind: "openai" | "claude";
	model: string;
	reasoning_effort?: ReasoningEffort;
	max_tokens?: number;
	prompt_cache?: Record<string, unknown>;
}

export interface ProjectWorkspace {
	kind?: "git" | "local";
	workspace_dir: string;
	remote_url?: string;
	remote_branch?: string;
	source_path?: string;
}

export interface SessionWorkspace extends ProjectWorkspace {
	base_sha?: string;
	local_branch?: string;
}

export interface SessionSummary {
	session_id: string;
	project_id: string | null;
	parent_session_id?: string | null;
	delegation_id?: string | null;
	subagent_type?: SubagentType | null;
	outer_cwd: string;
	workspaces: SessionWorkspace[];
	activity: Activity;
	active_leaf_id: string | null;
	provider: ProviderConfig;
	metadata: Record<string, unknown>;
	created_at: string;
	updated_at: string;
	last_user_message_timestamp_ms?: number | null;
	has_transcript_entries?: boolean;
	has_running_delegations?: boolean;
}

export interface PendingAction {
	action_row_id: string;
	kind: ActionKind;
	status: ActionStatus;
	payload: Record<string, unknown>;
}

export interface QueuedInput {
	input_id: string;
	priority: InputPriority;
	status: QueuedInputStatus;
	content: ContentBlock[];
	content_type?: "user_message" | "daemon_tool_observation";
	editable?: boolean;
	client_input_id?: string | null;
	created_at: string;
	updated_at?: string;
	promoted_at?: string | null;
	follow_up_position?: number | null;
}

export interface QueueProjection {
	session_revision: number;
	queue_revision: number;
	transcript_revision: number;
	activity: Activity;
	queued_inputs: QueuedInput[];
}

export interface SessionSnapshot {
	session_id: string;
	project_id: string | null;
	parent_session_id?: string | null;
	delegation_id?: string | null;
	subagent_type?: SubagentType | null;
	outer_cwd: string;
	workspaces: SessionWorkspace[];
	activity: Activity;
	active_leaf_id: string | null;
	provider: ProviderConfig;
	metadata: Record<string, unknown>;
	pending_actions: PendingAction[];
	queued_inputs: QueuedInput[];
	session_revision?: number;
	queue_revision?: number;
	transcript_revision?: number;
	last_event_id: number;
	server_time_ms: number;
	last_user_message_timestamp_ms?: number | null;
	has_transcript_entries?: boolean;
	entries?: TranscriptEntry[];
}

export interface SystemPromptResponse {
	template: string;
	rendered: string | null;
}

export interface Project {
	project_id: string;
	name: string;
	workspaces: ProjectWorkspace[];
	metadata: Record<string, unknown>;
	created_at: string;
	updated_at: string;
}

export interface EventFrame {
	event_id: number;
	event: string;
	session_id: string;
	data: Record<string, unknown>;
}

export type DelegationKind = "full" | "readonly_fanout";
export type DelegationStatus = "running" | "done" | "done_with_failures" | "cancelled" | "failed";
export type DelegationSubagentStatus = DelegationStatus | "idle" | "queued" | "done";
export type SubagentType = "full" | "read_only";

export interface DelegationProgress {
	expected: number;
	spawned: number;
	terminal: number;
	running: number;
	failed: number;
}

/** A subagent row inside a delegation. `status` is the meaningful delegation
 * member status (`done`/`failed` for terminal subagents, `running` for in-flight
 * members, or the parent delegation's terminal status for cancelled/failed
 * delegations); `activity` preserves the live session activity separately. The
 * daemon intentionally does not inline raw task prompts, final-message prose,
 * or transcript bodies; use `task_prompt_file`, `final_message_file`, and
 * `transcript_file` with `delegation.read_handoff_file` when detail is needed. */
export interface DelegationSubagent {
	id: string;
	status: Activity | DelegationSubagentStatus;
	activity?: Activity;
	role?: string | null;
	title?: string | null;
	type?: SubagentType | null;
	subagent_type?: SubagentType | null;
	steerable?: boolean;
	outcome?: string | null;
	final_message_file?: string | null;
	transcript_file?: string | null;
	task_prompt_file?: string | null;
}

export interface Delegation {
	delegation_id: string;
	kind: DelegationKind;
	status: DelegationStatus;
	workflow?: string | null;
	label?: string | null;
	progress?: DelegationProgress | null;
	handoff_dir?: string;
	subagents: DelegationSubagent[];
}

export interface DelegationListResult {
	parent_session_id: string;
	limit?: number;
	has_more?: boolean;
	delegations: Delegation[];
}

export type CancellationTranscriptFileName = `cancelled/${string}.transcript.md`;
export type HandoffFileName = "task_prompt.md" | "final_message.md" | "transcript.md" | CancellationTranscriptFileName;

export interface ReadHandoffFileResult {
	delegation_id: string;
	subagent_id: string | null;
	file: HandoffFileName;
	content: string;
}

export type SessionOverview = Omit<SessionSnapshot, "entries">;

export interface ActiveBranchSyncResponse {
	session_id: string;
	base_leaf_id: string | null;
	active_leaf_id: string | null;
	status: "unchanged" | "extended" | "branch_changed";
	entries: TranscriptEntry[];
	overview: SessionOverview;
}

export type ContentBlock =
	| { type: "text"; text: string }
	| {
			type: "image";
			image: {
				mime_type: string;
				source: { kind: "url" | "base64"; value: string };
			};
	  };

export type AssistantItem =
	| { type: "text"; text: string }
	| {
			type: "tool_call";
			id: string;
			tool_name: string;
			args_json: string;
	  };

export interface ToolCall {
	id: string;
	tool_name: string;
	args_json: string;
}

export interface DaemonToolObservation {
	tool_call_id: string;
	tool_name: string;
	args_json: string;
	result_json: unknown;
	status: ToolResultStatus;
	summary?: string | null;
}

export type TranscriptItem =
	| { type: "turn_started"; turn_id: number }
	| { type: "user_message"; content: ContentBlock[]; replayed_after_compaction?: boolean }
	| { type: "assistant_message"; items: AssistantItem[] }
	| { type: "tool_call_started"; turn_id: number; tool_call: ToolCall }
	| { type: "tool_result"; tool_call_id: string; tool_name: string; output: string; status: ToolResultStatus }
	| { type: "turn_finished"; turn_id: number; outcome: TurnOutcome }
	| ({ type: "daemon_tool_observation" } & DaemonToolObservation)
	| {
			type: "compaction_summary";
			source_session_id: string;
			source_leaf_id: string;
			summary: string;
			tokens_before?: number | null;
			last_turn_id: number;
			turn_started_at_ms?: number | null;
	  };

export type TranscriptItemType = TranscriptItem["type"];

export interface TranscriptEntry {
	id: string;
	parent_id: string | null;
	timestamp_ms: number;
	sequence?: number;
	item: TranscriptItem;
}

export interface TranscriptTreeNode {
	id: string;
	parent_id: string | null;
	source_leaf_id?: string | null;
	timestamp_ms: number;
	sequence: number;
	item_type: TranscriptItemType;
	turn_id?: number | null;
	outcome?: TurnOutcome | null;
	can_switch_to: boolean;
	edit_target_leaf_id?: string | null;
	display_hint?: string | null;
}

export interface TranscriptTreeIndex {
	session_id: string;
	active_leaf_id: string | null;
	session_revision: number;
	transcript_revision: number;
	after_sequence: number;
	max_sequence: number;
	complete: boolean;
	nodes: TranscriptTreeNode[];
}

export interface TranscriptEntriesResult {
	session_id: string;
	session_revision: number;
	transcript_revision: number;
	entries: TranscriptEntry[];
}

export interface TurnCard {
	id: string;
	turn_id?: number | null;
	status: "completed" | "open" | "compacted";
	outcome?: TurnOutcome | null;
	start_entry_id?: string | null;
	boundary_entry_id?: string | null;
	active_leaf_id: string;
	start_sequence: number;
	end_sequence: number;
	start_timestamp_ms: number;
	timestamp_ms: number;
	user_messages: TranscriptEntry[];
	daemon_observations?: TranscriptEntry[];
	assistant_message?: TranscriptEntry | null;
	summary?: string | null;
	can_resume: boolean;
}

export interface TranscriptTurnsResult {
	session_id: string;
	active_leaf_id: string | null;
	session_revision: number;
	transcript_revision: number;
	before_entry_id?: string | null;
	next_before_entry_id?: string | null;
	has_more_before: boolean;
	limit: number;
	cards: TurnCard[];
}

export interface TranscriptTurnDetailResult {
	session_id: string;
	active_leaf_id: string | null;
	session_revision: number;
	transcript_revision: number;
	card_id: string;
	entries: TranscriptEntry[];
}

export interface HistoryTree {
	session_id: string;
	active_leaf_id: string | null;
	entries: TranscriptEntry[];
}

export interface ToolDefinition {
	name: string;
	description: string;
	input_schema: unknown;
}

export interface LocalToolListing extends ToolDefinition {
	kind: "local_tool" | "hosted_tool";
}

export interface McpToolListing extends ToolDefinition {
	kind: "mcp_tool";
	server: string;
	raw_name: string;
	manifest_fingerprint: string;
	contract_fingerprint: string;
	health: McpHealth;
}

export type ToolListing = LocalToolListing | McpToolListing;

export type McpHealth = "healthy" | "unavailable" | "revoked";

export interface McpInventoryTool {
	raw_name: string;
	description: string;
	context_token_estimate: number;
}

export interface McpInventoryServer {
	server: string;
	revision: string;
	health: McpHealth;
	tools: McpInventoryTool[];
}

export interface McpInventory {
	revision: string;
	servers: McpInventoryServer[];
}

export interface StartSessionMcpSelection {
	inventoryRevision: string;
	servers: { server: string; tools: string[] }[];
}

export type NoticeTone = "info" | "success" | "error";

export interface Notice {
	id: string;
	tone: NoticeTone;
	text: string;
	persistent?: boolean;
}
