// Contract v0 wire types for the pi-relay bridge (packages/bridge/README.md).
// This module is the single source of truth for the bridge data layer's view
// of the wire: JSON-RPC-ish requests {id, method, params} -> {id, result} |
// {id, error:{code,message,data?}} plus server->client event envelopes with a
// per-session monotonic seq.

// ---- sessions ---------------------------------------------------------------

export type BridgeSessionStateName =
	| "starting"
	| "idle"
	| "running"
	| "compacting"
	| "compacted"
	| "host_down"
	| "respawning"
	| "host_exited"
	| "host_respawned"
	| "closed";

export interface SessionSummary {
	sessionId: string;
	state: string;
	cwd: string;
	name: string | null;
	/** M8 (G1): owning project, null = ungrouped */
	projectId?: string | null;
	hostGeneration: number;
	hostAlive: boolean;
	headSeq: number;
	createdAt: string;
	updatedAt: string;
}

/** M8 (G1): project row for sidebar grouping (mirrors bridge project.* methods). */
export interface ProjectSummary {
	/** M11b: project.list wire field (was mistyped `id`; the bridge sends projectId). */
	projectId: string;
	name: string;
	workspaceRoot?: string | null;
	/** M11b: project workspace decls (project.list shape). */
	workspaces?: BridgeSessionWorkspace[];
	createdAt: string;
	updatedAt: string;
}

export interface ProjectListResult {
	projects: ProjectSummary[];
}

export interface SessionLiveState {
	isStreaming: boolean;
	isCompacting: boolean;
	messageCount: number;
	pendingMessageCount: number;
	sessionName: string | null;
	model: string | null;
	/** M11a: effective thinking level on the live host. */
	thinkingLevel?: string | null;
}

export interface SessionState extends SessionSummary {
	sessionFile: string | null;
	hostPid: number | null;
	live?: SessionLiveState | null;
	/** M9: repl console runtime (active cell + kernel queue depth). */
	repl?: ReplRuntimeState;
	/** M11a: last persisted model/level when the host is down (session file). */
	model?: { provider: string | null; modelId: string | null; thinkingLevel: string | null } | null;
	/** M11b: fork lineage (session.fork sets this on the child). */
	parentSessionId?: string | null;
	/** M11b: session workspaces (managed or unmanaged). */
	workspaces?: BridgeSessionWorkspace[];
	/** M11b: per-session MCP selection. */
	mcpSelection?: McpSelectionPayload | null;
}

/** M11b: getState with the rebuild transcript (kept off SessionState proper
 * so the M6 sessionStore's live-projection block typing stays untouched). */
export interface SessionStateWithTranscript extends SessionState {
	transcript?: { blocks: TranscriptBlock[] };
	/** M11b: real id of the active branch's tip entry (any kind). The legacy
	 * adapter reports it as active_leaf_id when the branch has no renderable
	 * blocks (legacy daemon semantics: the active leaf is never null for a
	 * session with history). */
	branchTipId?: string | null;
}

export interface BridgeSessionWorkspace {
	kind?: "git" | "local";
	workspaceDir: string;
	remoteUrl?: string;
	remoteBranch?: string;
	branchOverride?: string;
	sourcePath?: string;
	baseSha?: string;
	localBranch?: string;
}

export interface McpSelectionPayload {
	inventoryRevision?: string;
	servers?: { server: string; tools: string[] }[];
}

// ---- M11b: transcript block projection (mirrors bridge transcript.ts) -------

export interface MessageBlock {
	kind: "message";
	id: string;
	role: string;
	text: string;
	thinking: string;
	complete: boolean;
	ts?: string;
}

export interface ToolExecBlock {
	kind: "tool";
	toolCallId: string;
	toolName: string;
	args?: string;
	result?: string;
	isError?: boolean;
	done: boolean;
	ts?: string;
}

export interface CommsBlock {
	kind: "comms";
	id: string;
	ts?: string;
	direction: "in";
	fromSessionId: string | null;
	fromName: string | null;
	message: string;
}

export interface ReplCellOutputBlock {
	stream: string;
	data: string;
	mimeType?: string;
	truncated?: boolean;
}

export interface ReplCellBlock {
	kind: "repl_cell";
	id: string;
	ts?: string;
	cellId: string;
	provenance: string | null;
	status: string;
	code: string | null;
	toolCallId?: string;
	durationMs?: number;
	error?: { ename: string; evalue: string };
	outputs: ReplCellOutputBlock[];
}

export interface CompactionBlock {
	kind: "compaction";
	id: string;
	ts?: string;
	summary: string;
	tokensBefore?: number;
	fromHook?: boolean;
}

export type TranscriptBlock = MessageBlock | ToolExecBlock | CommsBlock | ReplCellBlock | CompactionBlock;

// ---- subagents ----------------------------------------------------------------

export type SubagentLifecyclePhase = "admitted" | "completed" | "error" | "deleted";

export interface SubagentNode {
	rlmChildId: string;
	name: string | null;
	childSessionId: string | null;
	depth: number;
	status: string;
	createdAt: string | null;
	phases: string[];
}

// ---- method params / results --------------------------------------------------

export interface SessionCreateParams {
	cwd?: string;
	name?: string;
	projectId?: string;
	workspaces?: Array<{
		kind: "git" | "local";
		workspaceDir: string;
		remoteUrl?: string;
		remoteBranch?: string;
		branchOverride?: string;
		sourcePath?: string;
	}>;
	idempotencyKey?: string;
}

export interface SessionCreateResult {
	sessionId: string;
	state: string;
	replay?: boolean;
}

export interface SessionAttachResult {
	sessionId: string;
	headSeq: number;
	replayed: number;
}

export interface PromptSendResult {
	accepted: boolean;
	seq: number;
	queued: boolean;
	replay?: boolean;
}

export interface CommandAckResult {
	accepted: boolean;
	seq: number;
	queued: boolean;
}

export interface SessionListResult {
	sessions: SessionSummary[];
}

export interface SubagentTreeResult {
	sessionId: string;
	children: SubagentNode[];
}

// ---- repl console (contract v0.1, M9) -------------------------------------------

export type ReplProvenance = "user" | "model";
export type ReplCellStatus = "queued" | "running" | "done" | "error";
export type ReplOutputStream = "stdout" | "stderr" | "display" | "error";

export interface ReplCellData {
	cell_id: string;
	provenance?: ReplProvenance;
	status: ReplCellStatus;
	/** code echo (≤16KiB) on the queued event */
	code?: string;
	code_truncated?: boolean;
	client_cell_id?: string;
	tool_call_id?: string;
	position?: number;
	queued_at?: string;
	started_at?: string;
	finished_at?: string;
	duration_ms?: number;
	error?: { ename: string; evalue: string };
	stdout_truncated?: boolean;
	stderr_truncated?: boolean;
}

export interface ReplOutputData {
	cell_id: string;
	stream: ReplOutputStream;
	data: string;
	/** display payloads: text/plain | image/png | image/jpeg */
	mime_type?: string;
	truncated?: boolean;
}

export interface ReplExecuteResult {
	accepted: boolean;
	cell_id: string;
	replay?: boolean;
}

/** session.getState repl block (bridge-side event-sourced reducer). */
export interface ReplRuntimeState {
	active_cell: string | null;
	queue_depth: number;
	queued_cells: string[];
}

// ---- events -------------------------------------------------------------------

export interface MessageDeltaData {
	kind: "start" | "end" | "text" | "thinking" | "toolcall";
	role?: string;
	delta?: string;
	contentIndex?: number;
	toolCall?: string;
	piType?: string;
}

export interface ToolExecData {
	phase: "start" | "update" | "end";
	toolName: string;
	toolCallId: string;
	args?: string;
	partialResult?: string;
	result?: string;
	isError?: boolean;
	piType?: string;
}

export interface SessionStateEventData {
	state: string;
	piType?: string;
}

export interface SubagentLifecycleData {
	phase: SubagentLifecyclePhase;
	rlm_child_id: string;
	session_name?: string;
	status?: string;
	session_id?: string;
	created_at?: string;
	piType?: string;
}

export interface SessionErrorData {
	code: string;
	message: string;
	maybeLostQueued?: number;
	[key: string]: unknown;
}

export type ContractEventData =
	| { event: "session.state"; data: SessionStateEventData }
	| { event: "message.delta"; data: MessageDeltaData }
	| { event: "tool.exec"; data: ToolExecData }
	| { event: "subagent.lifecycle"; data: SubagentLifecycleData }
	| { event: "repl.cell"; data: ReplCellData }
	| { event: "repl.output"; data: ReplOutputData }
	| { event: "session.model"; data: SessionModelData }
	| { event: "comms.message"; data: CommsMessageData }
	| { event: "session.error"; data: SessionErrorData };

export interface ContractEventEnvelope {
	event: string;
	sessionId: string;
	seq: number;
	data: Record<string, unknown>;
	at: string;
	replayed?: boolean;
}

// ---- errors -------------------------------------------------------------------

export const BRIDGE_ERROR_CODES = [
	"unauthorized",
	"forbidden_origin",
	"bad_request",
	"method_not_found",
	"not_implemented",
	"session_not_found",
	"session_busy",
	"host_unavailable",
	"host_down",
	"repl_unavailable",
	"model_not_found",
	"model_unavailable",
	"subagent_not_found",
	"event_gap",
	"idempotency_conflict",
	"internal",
] as const;

export type BridgeErrorCode = (typeof BRIDGE_ERROR_CODES)[number] | (string & {});

export interface EventGapErrorData {
	minAvailable: number;
	headSeq: number;
}

// ---- model surface (contract v0.2, M11a) --------------------------------------

export interface BridgeModelEntry {
	provider: string;
	id: string;
	name: string;
	api: string;
	reasoning: boolean;
	/** pi-ai getSupportedThinkingLevels semantics (bridge-computed). */
	thinkingLevels: string[];
	contextWindow: number;
	maxTokens: number;
	input: string[];
	cost: { input: number; output: number; cacheRead: number; cacheWrite: number };
	baseUrl: string;
	source: "models.json" | "builtin";
	/** in the runtime's available snapshot (upstream set_model gate) */
	available: boolean;
	authConfigured: boolean;
	authSource?: string;
}

export interface BridgeProviderInfo {
	id: string;
	source: "models.json" | "builtin";
	authConfigured: boolean;
	authSource?: string;
}

export interface ModelsListResult {
	models: BridgeModelEntry[];
	providers: BridgeProviderInfo[];
	defaults: { provider: string; modelId: string } | null;
	fetchedAt: string;
	error?: string;
}

/** session.model event data — emitted by the bridge after set_model /
 * set_thinking_level (pi emits no wire event for those) and mapped from the
 * host's thinking_level_changed (which carries only the level → other fields
 * are null and merge-client-side). */
export interface SessionModelData {
	provider?: string | null;
	modelId?: string | null;
	name?: string | null;
	thinkingLevel?: string | null;
	piType?: string;
}

export interface SetModelResult {
	provider: string;
	modelId: string;
	name: string | null;
	thinkingLevel: string | null;
}

export interface SetThinkingLevelResult {
	thinkingLevel: string | null;
	availableLevels: string[];
}

// ---- subagent drill-down (contract v0.2, M11a) --------------------------------

export interface SubagentReplCell {
	cellId: string;
	provenance: string | null;
	status: string;
	code: string | null;
	toolCallId?: string;
	queuedAt?: string;
	startedAt?: string;
	finishedAt?: string;
	durationMs?: number;
	error?: { ename: string; evalue: string };
	outputs: Array<{ stream: string; data: string; mimeType?: string; truncated?: boolean }>;
}

export interface SubagentTranscriptResult {
	sessionId: string;
	childId: string;
	childSessionId: string | null;
	sessionFile: string;
	blocks: Array<
		| { kind: "message"; id: string; role: string; text: string; thinking: string; complete: boolean }
		| { kind: "tool"; toolCallId: string; toolName: string; args?: string; result?: string; isError?: boolean; done: boolean }
	>;
	replCells: SubagentReplCell[];
}

// ---- comms visibility (contract v0.2, M11a) -------------------------------------

/** comms.list message (outbox-folded outbound + session-file inbound). */
export interface CommsMessageRecord {
	id: string;
	ts: string;
	direction: "in" | "out";
	from: { sessionId: string; name?: string; depth?: number };
	to: { sessionId: string; name?: string } | null;
	role?: string;
	content: string;
	deliveryStatus: string;
	deliveryMode?: string;
	detail?: string;
}

export interface CommsListResult {
	sessionId: string;
	messages: CommsMessageRecord[];
}

/** comms.message live event — prime-comms appendEntry on terminal delivery
 * transitions (sender side). */
export interface CommsMessageData {
	fromSessionId?: string;
	fromName?: string;
	role?: "parent" | "sibling" | "child";
	receiverName?: string;
	targetSessionId?: string;
	message?: string;
	deliveryStatus?: "delivered" | "failed" | "persisted";
	ts?: string;
	piType?: string;
}
