// M11b: BridgeAgentApi — the legacy AgentApi surface implemented over the
// bridge contract (packages/bridge, contract v0). This is the whole port
// strategy: the legacy App (sidebar / chatPane / panels rail / inspector /
// files / git / history / MCP sheets / newSessionSetup) runs UNCHANGED on top
// of this adapter. The adapter translates:
//
//   data:   bridge blocks/events → legacy TranscriptEntry/TurnCard/EventFrame
//           (legacyTranscript.ts / legacyEvents.ts / legacySessions.ts)
//   rpc:    legacy AgentApi methods → bridge contract methods (this file)
//
// Degradation policy (owner rule): surfaces the bridge cannot serve fail with
// a typed, human-readable error — never silently pretend. Unsupported:
// queue management (promote/update/cancel/reorder), resumeTurn, delegation
// launch/cancel/handoffs (the bridge's subagents are model-spawned via the
// rlm tool, observed read-only through the SUBAGENTS rail), session metadata
// writes (archive), workspace fs watch pushes, system prompt read.

import { BridgeClient, isBridgeErrorCode, newIdempotencyKey, type BridgeConnectionStatus } from "./client.ts";
import { bridgeWebSocketUrl } from "./useBridge.tsx";
import type {
	BridgeModelEntry,
	BridgeSessionWorkspace,
	ContractEventEnvelope,
	SessionStateWithTranscript,
	SubagentNode,
} from "./types.ts";
import { reportedLeafId, resyncStore, splitModel, storeFromState, type SessionStore } from "./legacySessions.ts";
import {
	availableModelOptions,
	setDynamicModelOptions,
	DEFAULT_PROVIDER,
	type ModelOption,
} from "../sessionDefaults.ts";
import type {
	ActiveBranchSyncResponse,
	Activity,
	ContentBlock,
	Delegation,
	DelegationListResult,
	DelegationSubagent,
	EventFrame,
	GitStatusRoot as LegacyGitStatusRoot,
	HistoryTarget,
	HistoryTargetsResult,
	HistoryTree,
	McpAuthServerStatus,
	McpInventory,
	McpLoginResult,
	McpLogoutResult,
	McpSelection,
	McpStatus,
	Project,
	ProviderConfig,
	QueuedInput,
	ReasoningEffort,
	Runtime,
	SessionSnapshot,
	SessionSummary,
	SessionWorkspace,
	SystemPromptResponse,
	ToolListing,
	TranscriptEntriesResult,
	TranscriptEntry,
	TranscriptItem,
	TranscriptTreeIndex,
	TranscriptTurnDetailResult,
	TranscriptTurnsResult,
	TurnCard,
	WorkspaceDirListing,
	WorkspaceFilePrefix,
	WorkspaceGitDiff,
	WorkspaceGitStatus,
} from "../types.ts";
import type {
	AddMcpToolsParams,
	AddMcpToolsResult,
	AgentApi,
	CancelQueuedResult,
	ConfigureSessionParams,
	ConfigureSessionResult,
	CreateProjectParams,
	DeleteProjectResult,
	DeleteSessionResult,
	ForkHistoryParams,
	ForkHistoryResult,
	GetSessionOptions,
	GitDiffParams,
	GitStatusParams,
	HistoryTargetsOptions,
	InterruptResult,
	ListWorkspaceDirParams,
	PromoteQueuedResult,
	QueueFollowUpParams,
	ReadHandoffFileParams,
	ReadWorkspaceFileParams,
	RenameSessionResult,
	ReorderQueuedResult,
	ResumeTurnParams,
	ResumeTurnResult,
	StartFullDelegationParams,
	StartFullDelegationResult,
	StartReadonlyDelegationFanoutParams,
	StartReadonlyDelegationFanoutResult,
	StartSessionParams,
	StartSessionResult,
	SteerSubagentParams,
	SteerSubagentResult,
	SwitchHistoryParams,
	SwitchHistoryResult,
	TranscriptIndexOptions,
	TranscriptTurnDetailRequest,
	TranscriptTurnsOptions,
	UpdateProjectParams,
	UpdateQueuedResult,
	WatchWorkspaceParams,
	FollowUpResult,
} from "../agentApi.ts";
import { appendTurnCard } from "../selectedSessionCache/turns.ts";
import type { ReadHandoffFileResult } from "../types.ts";

type EventHandler = (event: EventFrame) => void;
type StatusHandler = (status: "connecting" | "open" | "closed" | "error") => void;

const BRIDGE_RUNTIME_ID = "bridge";
const BRIDGE_RUNTIME_NAME = "Bridge (pi)";
const DEFAULT_TURN_PAGE = 50;

function unsupported(feature: string): never {
	throw new Error(`${feature} is not supported by the bridge backend (contract v0)`);
}

function toMs(iso: string | null | undefined): number {
	if (!iso) return Date.now();
	const ms = Date.parse(iso);
	return Number.isNaN(ms) ? Date.now() : ms;
}

function mapActivity(state: string): Activity {
	switch (state) {
		case "running":
		case "compacting":
		case "compacted":
		case "starting":
		case "respawning":
			return "running";
		default:
			return "idle";
	}
}

const REASONING_EFFORTS: readonly string[] = ["none", "minimal", "low", "medium", "high", "xhigh", "max"];

function toReasoningEffort(level: string | null | undefined): ReasoningEffort | undefined {
	if (!level) return undefined;
	const mapped = level === "off" ? "none" : level;
	return REASONING_EFFORTS.includes(mapped) ? (mapped as ReasoningEffort) : undefined;
}

function toThinkingLevel(effort: ReasoningEffort | undefined): string | undefined {
	if (!effort) return undefined;
	return effort === "none" ? "off" : effort;
}

function legacyKindForProvider(provider: string | null | undefined): "openai" | "claude" {
	return provider === "anthropic" ? "claude" : "openai";
}

function providerFromModel(model: { provider: string | null; modelId: string | null; thinkingLevel: string | null } | null): ProviderConfig {
	if (!model || !model.modelId) return { ...DEFAULT_PROVIDER };
	return {
		kind: legacyKindForProvider(model.provider),
		model: model.modelId,
		reasoning_effort: toReasoningEffort(model.thinkingLevel),
	};
}

/** Resolve the exact bridge "provider/modelId" for a legacy ProviderConfig by
 * matching against the dynamic (models.list-fed) options first. */
function bridgeModelString(provider: ProviderConfig | undefined): string | undefined {
	if (!provider) return undefined;
	const option = availableModelOptions().find(
		(candidate) => candidate.provider.kind === provider.kind && candidate.provider.model === provider.model,
	);
	if (option?.bridgeModel) return option.bridgeModel;
	return `${provider.kind === "openai" ? "openai-codex" : provider.kind}/${provider.model}`;
}

function mapWorkspaces(ws: BridgeSessionWorkspace[] | undefined): SessionWorkspace[] {
	return (ws ?? []).map((w) => ({
		kind: w.kind,
		workspace_dir: w.workspaceDir,
		remote_url: w.remoteUrl,
		remote_branch: w.remoteBranch,
		source_path: w.sourcePath,
		base_sha: w.baseSha,
		local_branch: w.localBranch,
	}));
}

function metadataFor(name: string | null): Record<string, unknown> {
	return name ? { title: name } : {};
}

/** Owner picker rule (supersedes M11a): models without auth do not appear at
 * all — filter available===true here at the adapter layer. */
export function bridgeModelsToOptions(models: BridgeModelEntry[]): ModelOption[] {
	// Owner picker rule: only authenticated (available) models are listed.
	// Ids follow the legacy ModelOption idiom `${kind}:${model}` so the header
	// select value (providerModelKey) matches an option and no synthetic
	// "current" row is prepended; `bridgeModel` carries the exact contract
	// string for session.setModel / session.create.
	const seen = new Set<string>();
	const out: ModelOption[] = [];
	for (const m of models.filter((m) => m.available === true)) {
		const kind = legacyKindForProvider(m.provider);
		const id = `${kind}:${m.id}`;
		if (seen.has(id)) continue;
		seen.add(id);
		out.push({
			id,
			label: `${kind}:${m.id}`,
			description: m.name !== m.id ? m.name : undefined,
			provider: {
				kind,
				model: m.id,
				reasoning_effort: toReasoningEffort(
					m.thinkingLevels.includes("high") ? "high" : m.thinkingLevels.at(-1),
				),
			},
			bridgeModel: `${m.provider}/${m.id}`,
			bridgeThinkingLevels: m.thinkingLevels,
		});
	}
	return out;
}

function contentBlocksToTextLocal(content: ContentBlock[]): string {
	return content
		.filter((block): block is { type: "text"; text: string } => block.type === "text")
		.map((block) => block.text)
		.join("");
}

interface BridgeForkPoint {
	entryId: string;
	parentId?: string | null;
	timestamp?: string | null;
	preview: string;
	onActiveBranch?: boolean;
	live?: boolean;
}

interface BridgeGitRoot {
	workspaceDir: string;
	comparison: { baseBranch: string; mergeBaseOid: string; headOid: string } | null;
	error: string | null;
	entries: { path: string; status: string; staged: boolean }[];
}

export class BridgeAgentApi implements AgentApi {
	private client: BridgeClient;
	private readonly url: string;
	private readonly stores = new Map<string, SessionStore>();
	private readonly attachInFlight = new Map<string, Promise<SessionStore>>();
	private readonly eventHandlers = new Set<EventHandler>();
	private readonly statusHandlers = new Set<StatusHandler>();
	private offClientEvent: (() => void) | null = null;
	private offClientStatus: (() => void) | null = null;
	private status: "connecting" | "open" | "closed" | "error" = "connecting";
	private modelsLoaded = false;

	constructor(clientOrUrl?: BridgeClient | string) {
		this.url = typeof clientOrUrl === "string" ? clientOrUrl : bridgeWebSocketUrl();
		this.client = this.wireClient(
			clientOrUrl instanceof BridgeClient ? clientOrUrl : new BridgeClient(this.url),
		);
	}

	/** The shared socket (BridgeApp hands the same client to the REPL store). */
	get bridgeClient(): BridgeClient {
		return this.client;
	}

	private wireClient(client: BridgeClient): BridgeClient {
		this.offClientEvent?.();
		this.offClientStatus?.();
		this.offClientEvent = client.onEvent((env) => this.handleBridgeEvent(env));
		this.offClientStatus = client.onStatus((status) => this.handleClientStatus(status));
		return client;
	}

	// ---- connection -----------------------------------------------------------

	connect(): Promise<void> {
		return this.client.connect();
	}

	async reconnect(): Promise<void> {
		this.client.close();
		this.client = this.wireClient(new BridgeClient(this.url));
		this.markAllDetached();
		await this.client.connect();
	}

	close(): void {
		this.client.close();
	}

	isOpen(): boolean {
		return this.client.isOpen();
	}

	onEvent(handler: EventHandler): () => void {
		this.eventHandlers.add(handler);
		return () => this.eventHandlers.delete(handler);
	}

	onStatus(handler: StatusHandler): () => void {
		this.statusHandlers.add(handler);
		return () => this.statusHandlers.delete(handler);
	}

	private handleClientStatus(status: BridgeConnectionStatus): void {
		const mapped = status === "open" ? "open" : status === "connecting" ? "connecting" : status;
		this.status = mapped;
		if (status === "open") {
			void this.loadModels().catch(() => {});
			void this.reattachAll().catch(() => {});
		}
		for (const handler of this.statusHandlers) handler(mapped);
	}

	private markAllDetached(): void {
		for (const store of this.stores.values()) store.attached = false;
	}

	/** Reconnect: the bridge's server-side attachments died with the socket.
	 * Re-serve every attached session from getState and re-attach at head —
	 * resync is the only correct recovery for the synthesizer's live state. */
	private async reattachAll(): Promise<void> {
		for (const store of [...this.stores.values()]) {
			if (!store.attached) continue;
			store.attached = false;
			try {
				const fresh = await this.getStateFull(store.sessionId);
				resyncStore(store, fresh);
				await this.attachStore(store);
				this.dispatchRefresh(store);
			} catch {
				/* session may be gone; the next list refresh drops it */
			}
		}
	}

	/** models.list → dynamic picker options (owner rule: available only). */
	private async loadModels(): Promise<void> {
		const res = await this.client.listModels();
		setDynamicModelOptions(bridgeModelsToOptions(res.models));
		this.modelsLoaded = true;
	}

	// ---- frame dispatch -------------------------------------------------------

	private emit(frame: EventFrame): void {
		for (const handler of this.eventHandlers) handler(frame);
	}

	/** Dispatch synthesizer frames with sub-sequenced ids (the App drops frames
	 * whose event_id is not strictly increasing) and mirror entries into the
	 * store (single append path shared with rebuild). */
	private dispatchFrames(store: SessionStore, frames: EventFrame[]): void {
		for (const frame of frames) {
			store.frameSeq += 1;
			const data: Record<string, unknown> = { ...frame.data };
			for (const key of ["transcript_revision", "session_revision", "queue_revision"] as const) {
				if (typeof data[key] === "number") data[key] = store.frameSeq;
			}
			const entry = data.entry as TranscriptEntry | undefined;
			if (frame.event === "transcript.appended" && entry) this.storeAppend(store, entry);
			if (frame.event === "input.queued") {
				const queued = data.queued_inputs as QueuedInput[] | undefined;
				if (queued) store.queued = queued;
			}
			if (frame.event === "session.idle") store.state = "idle";
			if (frame.event === "input.accepted" || frame.event === "input.consumed" || frame.event === "tool.started") {
				store.state = "running";
			}
			this.emit({ ...frame, event_id: store.frameSeq, data });
		}
		store.revision = Math.max(store.revision, store.frameSeq);
	}

	private dispatchRefresh(store: SessionStore): void {
		// An entryless transcript.appended makes the legacy cache answer
		// "refresh"; the App refetches via getSession and gets the re-synced store.
		this.dispatchFrames(store, [{
			event_id: 0,
			event: "transcript.appended",
			session_id: store.sessionId,
			data: { transcript_revision: 0, session_revision: 0, queue_revision: 0, activity: mapActivity(store.state), queued_inputs: store.queued },
		}]);
	}

	private storeAppend(store: SessionStore, entry: TranscriptEntry): void {
		const index = store.entries.findIndex((candidate) => candidate.id === entry.id);
		if (index >= 0) store.entries[index] = entry;
		else store.entries.push(entry);
		const cards = new Map(store.cards.map((card) => [card.id, card]));
		const order = store.cards.map((card) => card.id);
		const next = appendTurnCard(cards, order, entry);
		store.cards = next.turnOrder.flatMap((id) => {
			const card = next.turnCardsById.get(id);
			return card ? [card] : [];
		});
		store.leafId = entry.id;
	}

	private handleBridgeEvent(env: ContractEventEnvelope): void {
		const store = this.stores.get(env.sessionId);
		if (!store?.attached || !store.synthesizer) return;
		const out = store.synthesizer.handle(env);
		store.queued = store.synthesizer.queueSnapshot();
		this.dispatchFrames(store, out.frames);
		if (out.drift) void this.resyncAndRefresh(store);
	}

	private async resyncAndRefresh(store: SessionStore): Promise<void> {
		try {
			const fresh = await this.getStateFull(store.sessionId);
			resyncStore(store, fresh);
			this.dispatchRefresh(store);
		} catch {
			/* next event or poll retries */
		}
	}

	// ---- attach lifecycle -----------------------------------------------------

	private async getStateFull(sessionId: string): Promise<SessionStateWithTranscript> {
		return (await this.client.getState(sessionId)) as SessionStateWithTranscript;
	}

	private async ensureStore(sessionId: string): Promise<SessionStore> {
		const existing = this.stores.get(sessionId);
		if (existing) return existing;
		const state = await this.getStateFull(sessionId);
		const store = storeFromState(state);
		this.stores.set(sessionId, store);
		return store;
	}

	private attachStore(store: SessionStore): Promise<SessionStore> {
		const inFlight = this.attachInFlight.get(store.sessionId);
		if (inFlight) return inFlight;
		const promise = (async () => {
			if (store.attached) return store;
			store.attached = true; // optimistic: replay frames precede the response
			try {
				const res = await this.client.attachSession(store.sessionId, store.headSeq);
				store.headSeq = Math.max(store.headSeq, res.headSeq);
				return store;
			} catch (error) {
				store.attached = false;
				const gap = isBridgeErrorCode(error, "event_gap");
				if (gap) {
					const fresh = await this.getStateFull(store.sessionId);
					resyncStore(store, fresh);
					return this.attachStore(store);
				}
				throw error;
			}
		})().finally(() => {
			this.attachInFlight.delete(store.sessionId);
		});
		this.attachInFlight.set(store.sessionId, promise);
		return promise;
	}

	private async ensureAttached(sessionId: string): Promise<SessionStore> {
		const store = await this.ensureStore(sessionId);
		if (!store.attached) {
			if (this.client.isOpen()) await this.attachStore(store);
		} else if (!this.client.isOpen()) {
			// reconnect in flight; reattachAll will re-serve
		}
		return store;
	}

	/** Fresh serve: getState → resync (history/fork/switch read paths). */
	private async ensureFresh(sessionId: string): Promise<SessionStore> {
		const fresh = await this.getStateFull(sessionId);
		const store = this.stores.get(sessionId);
		if (store) {
			resyncStore(store, fresh);
			return store;
		}
		const created = storeFromState(fresh);
		this.stores.set(sessionId, created);
		return created;
	}

	private snapshotFromStore(store: SessionStore, includeEntries: boolean): SessionSnapshot {
		const snapshot: SessionSnapshot = {
			session_id: store.sessionId,
			project_id: store.projectId,
			parent_session_id: store.parentSessionId,
			runtime_id: BRIDGE_RUNTIME_ID,
			workspace_id: store.cwd,
			workspaces: mapWorkspaces(store.workspaces),
			activity: mapActivity(store.state),
			active_leaf_id: reportedLeafId(store),
			provider: providerFromModel(store.model),
			metadata: metadataFor(store.name),
			pending_actions: [],
			queued_inputs: store.queued,
			session_revision: store.revision,
			queue_revision: store.revision,
			transcript_revision: store.revision,
			last_event_id: store.revision,
			server_time_ms: Date.now(),
		};
		if (includeEntries) snapshot.entries = store.entries;
		return snapshot;
	}

	private summaryFromState(state: import("./types.ts").SessionSummary): SessionSummary {
		const store = this.stores.get(state.sessionId);
		const extended = state as import("./types.ts").SessionSummary & {
			live?: { model?: string | null; thinkingLevel?: string | null } | null;
			model?: { provider: string | null; modelId: string | null; thinkingLevel: string | null } | null;
			workspaces?: BridgeSessionWorkspace[];
			parentSessionId?: string | null;
		};
		const liveModel = extended.live?.model
			? splitModel(extended.live.model, extended.live.thinkingLevel ?? null)
			: (extended.model ?? null);
		return {
			session_id: state.sessionId,
			project_id: state.projectId ?? null,
			parent_session_id: extended.parentSessionId ?? null,
			runtime_id: BRIDGE_RUNTIME_ID,
			workspace_id: state.cwd,
			workspaces: mapWorkspaces(extended.workspaces),
			activity: mapActivity(state.state),
			active_leaf_id: store ? reportedLeafId(store) : null,
			provider: providerFromModel(store?.model ?? liveModel ?? null),
			metadata: metadataFor(state.name),
			created_at: state.createdAt,
			updated_at: state.updatedAt,
		};
	}

	// ---- projects / runtimes --------------------------------------------------

	async listProjects(): Promise<Project[]> {
		const res = await this.client.listProjects();
		return res.projects.map((p) => ({
			project_id: p.projectId,
			runtime_id: BRIDGE_RUNTIME_ID,
			name: p.name,
			workspaces: mapWorkspaces(p.workspaces as BridgeSessionWorkspace[] | undefined),
			metadata: {},
			created_at: String(p.createdAt ?? ""),
			updated_at: String(p.updatedAt ?? ""),
		}));
	}

	listRuntimes(): Promise<Runtime[]> {
		return Promise.resolve([{
			runtime_id: BRIDGE_RUNTIME_ID,
			name: BRIDGE_RUNTIME_NAME,
			online: this.client.isOpen(),
			last_seen_at: null,
		}]);
	}

	async createProject(params: CreateProjectParams): Promise<Project> {
		const res = await this.client.request<{ projectId: string; name: string; workspaces?: BridgeSessionWorkspace[] }>("project.create", {
			name: params.name,
			workspaces: (params.workspaces ?? []).map((w) => ({
				kind: w.kind ?? "local",
				workspaceDir: w.workspace_dir,
				...(w.remote_url ? { remoteUrl: w.remote_url } : {}),
				...(w.remote_branch ? { remoteBranch: w.remote_branch } : {}),
				...(w.source_path ? { sourcePath: w.source_path } : {}),
			})),
			idempotencyKey: newIdempotencyKey("project-create"),
		});
		return {
			project_id: res.projectId,
			runtime_id: BRIDGE_RUNTIME_ID,
			name: res.name,
			workspaces: mapWorkspaces(res.workspaces),
			metadata: {},
			created_at: new Date().toISOString(),
			updated_at: new Date().toISOString(),
		};
	}

	async updateProject(params: UpdateProjectParams): Promise<Project> {
		await this.client.request("project.update", {
			id: params.projectId,
			...(params.name !== undefined ? { name: params.name } : {}),
			...(params.workspaces !== undefined
				? {
					workspaces: params.workspaces.map((w) => ({
						kind: w.kind ?? "local",
						workspaceDir: w.workspace_dir,
						...(w.remote_url ? { remoteUrl: w.remote_url } : {}),
						...(w.remote_branch ? { remoteBranch: w.remote_branch } : {}),
						...(w.source_path ? { sourcePath: w.source_path } : {}),
					})),
				}
				: {}),
		});
		const projects = await this.listProjects();
		const project = projects.find((candidate) => candidate.project_id === params.projectId);
		if (!project) throw new Error(`project ${params.projectId} not found after update`);
		return project;
	}

	async deleteProject(projectId: string): Promise<DeleteProjectResult> {
		await this.client.request("project.delete", { id: projectId });
		return { project_id: projectId, deleted: true };
	}

	// ---- session list / snapshots ---------------------------------------------

	async listSessions(limit?: number, projectId?: string | null): Promise<SessionSummary[]> {
		const res = await this.client.listSessions();
		let sessions = res.sessions;
		if (projectId !== undefined) {
			sessions = sessions.filter((s) => (s.projectId ?? null) === projectId);
		}
		if (limit !== undefined) sessions = sessions.slice(0, limit);
		return sessions.map((s) => this.summaryFromState(s));
	}

	async getSession(sessionId: string, options?: GetSessionOptions): Promise<SessionSnapshot> {
		const store = await this.ensureAttached(sessionId);
		if (!store.attached && this.client.isOpen()) {
			// unattached stores go stale silently — re-serve before answering
			const fresh = await this.getStateFull(sessionId);
			resyncStore(store, fresh);
			await this.attachStore(store).catch(() => {});
		}
		return this.snapshotFromStore(store, options?.includeEntries === true);
	}

	async syncActiveBranch(sessionId: string, baseLeafId: string | null): Promise<ActiveBranchSyncResponse> {
		const store = await this.ensureAttached(sessionId);
		const overview = this.snapshotFromStore(store, false);
		if (baseLeafId === reportedLeafId(store)) {
			return {
				session_id: sessionId,
				base_leaf_id: baseLeafId,
				active_leaf_id: reportedLeafId(store),
				status: "unchanged",
				entries: [],
				overview,
			};
		}
		const index = store.entries.findIndex((entry) => entry.id === baseLeafId);
		if (index >= 0) {
			return {
				session_id: sessionId,
				base_leaf_id: baseLeafId,
				active_leaf_id: reportedLeafId(store),
				status: "extended",
				entries: store.entries.slice(index + 1),
				overview,
			};
		}
		return {
			session_id: sessionId,
			base_leaf_id: baseLeafId,
			active_leaf_id: reportedLeafId(store),
			status: "branch_changed",
			entries: [...store.entries],
			overview,
		};
	}

	// ---- transcript reads -----------------------------------------------------

	async getTranscriptIndex(sessionId: string, options?: TranscriptIndexOptions): Promise<TranscriptTreeIndex> {
		const store = await this.ensureAttached(sessionId);
		const after = options?.afterSequence ?? 0;
		let nodes = store.entries
			.filter((entry) => (entry.sequence ?? 0) > after)
			.map((entry) => ({
				id: entry.id,
				parent_id: entry.parent_id,
				timestamp_ms: entry.timestamp_ms,
				sequence: entry.sequence ?? 0,
				item_type: entry.item.type,
				turn_id: entry.item.type === "turn_started" || entry.item.type === "turn_finished" ? entry.item.turn_id : null,
				outcome: entry.item.type === "turn_finished" ? entry.item.outcome : null,
				can_switch_to: entry.item.type === "user_message",
				edit_target_leaf_id: entry.item.type === "user_message" ? entry.parent_id : null,
			}));
		if (options?.limit != null) nodes = nodes.slice(0, options.limit);
		const maxSequence = store.entries.at(-1)?.sequence ?? 0;
		return {
			session_id: sessionId,
			active_leaf_id: reportedLeafId(store),
			session_revision: store.revision,
			transcript_revision: store.revision,
			after_sequence: after,
			max_sequence: maxSequence,
			complete: options?.limit == null || nodes.length < options.limit,
			nodes,
		};
	}

	async getTranscriptEntries(sessionId: string, entryIds: string[]): Promise<TranscriptEntriesResult> {
		const store = await this.ensureAttached(sessionId);
		const found = new Map(store.entries.map((entry) => [entry.id, entry]));
		const entries: TranscriptEntry[] = [];
		const missing: string[] = [];
		for (const id of entryIds) {
			const entry = found.get(id);
			if (entry) entries.push(entry);
			else missing.push(id);
		}
		if (missing.length > 0) {
			// Off-branch / rebuilt-away ids (e.g. /switch restore text): read the
			// persisted message text straight from the session file.
			const res = await this.client.request<{ entries: { id: string; role: string; text: string; timestamp: string | null }[] }>(
				"session.getEntries",
				{ sessionId, ids: missing },
			);
			for (const remote of res.entries) {
				entries.push({
					id: remote.id,
					parent_id: null,
					timestamp_ms: toMs(remote.timestamp),
					item: remote.role === "user"
						? { type: "user_message", content: [{ type: "text", text: remote.text }] }
						: { type: "assistant_message", items: remote.text ? [{ type: "text", text: remote.text }] : [] },
				});
			}
		}
		return {
			session_id: sessionId,
			session_revision: store.revision,
			transcript_revision: store.revision,
			entries,
		};
	}

	async getHistoryTargets(sessionId: string, options?: HistoryTargetsOptions): Promise<HistoryTargetsResult> {
		const store = await this.ensureFresh(sessionId);
		const res = await this.client.request<{ points: BridgeForkPoint[] }>("session.getForkPoints", { sessionId });
		// newest-first, matching the legacy picker's backward pagination
		const points = [...res.points].reverse();
		const sequenced = points.map((point, index) => ({ point, sequence: index + 1 }));
		const before = options?.beforeSequence ?? null;
		const eligible = before === null ? sequenced : sequenced.filter((row) => row.sequence < before);
		const limit = options?.limit ?? 50;
		const page = eligible.slice(0, limit);
		const last = page.at(-1);
		const targets: HistoryTarget[] = page.map(({ point }) => ({
			entry_id: point.entryId,
			target_leaf_id: point.parentId ?? null,
			timestamp_ms: toMs(point.timestamp),
			turn_id: null,
			is_on_active_branch: point.onActiveBranch ?? false,
			preview: point.preview,
		}));
		return {
			session_id: sessionId,
			active_leaf_id: reportedLeafId(store),
			session_revision: store.revision,
			transcript_revision: store.revision,
			before_sequence: before,
			next_before_sequence: last && eligible.length > page.length ? last.sequence : null,
			has_more: eligible.length > page.length,
			targets,
		};
	}

	async getTranscriptTurns(sessionId: string, options?: TranscriptTurnsOptions): Promise<TranscriptTurnsResult> {
		const store = await this.ensureAttached(sessionId);
		const limit = options?.limit ?? DEFAULT_TURN_PAGE;
		const cards = store.cards;
		let end = cards.length;
		if (options?.beforeEntryId) {
			const index = cards.findIndex((card) => card.id === options.beforeEntryId);
			if (index >= 0) end = index;
		}
		const start = Math.max(0, end - limit);
		const page = cards.slice(start, end);
		return {
			session_id: sessionId,
			active_leaf_id: reportedLeafId(store),
			session_revision: store.revision,
			transcript_revision: store.revision,
			before_entry_id: options?.beforeEntryId ?? null,
			next_before_entry_id: start > 0 ? cards[start].id : null,
			has_more_before: start > 0,
			limit,
			cards: page,
		};
	}

	async getTranscriptTurnDetail(sessionId: string, request: TranscriptTurnDetailRequest): Promise<TranscriptTurnDetailResult> {
		const store = await this.ensureAttached(sessionId);
		const entries = store.entries.filter(
			(entry) => (entry.sequence ?? 0) >= request.startSequence && (entry.sequence ?? 0) <= request.endSequence,
		);
		return {
			session_id: sessionId,
			active_leaf_id: reportedLeafId(store),
			session_revision: store.revision,
			transcript_revision: store.revision,
			card_id: request.cardId,
			entries,
		};
	}

	async getHistoryTree(sessionId: string): Promise<HistoryTree> {
		const store = await this.ensureAttached(sessionId);
		return { session_id: sessionId, active_leaf_id: reportedLeafId(store), entries: [...store.entries] };
	}

	async getHistoryContext(sessionId: string, leafId?: string): Promise<TranscriptItem[]> {
		const store = await this.ensureAttached(sessionId);
		const items: TranscriptItem[] = [];
		for (const entry of store.entries) {
			items.push(entry.item);
			if (leafId !== undefined && entry.id === leafId) break;
		}
		return items;
	}

	// ---- events ---------------------------------------------------------------

	async subscribeEvents(sessionId: string, _afterEventId: number | null): Promise<EventFrame[]> {
		// The adapter attaches at head and serves getState-coherent snapshots, so
		// there is no replay backlog to hand back — live frames flow via onEvent.
		await this.ensureAttached(sessionId);
		return [];
	}

	async unsubscribeEvents(sessionId: string): Promise<void> {
		const store = this.stores.get(sessionId);
		if (store) store.attached = false;
		await this.client.detachSession(sessionId).catch(() => {});
	}

	// ---- mutations ------------------------------------------------------------

	async startSession(params: StartSessionParams): Promise<StartSessionResult> {
		const model = bridgeModelString(params.provider);
		const workspaces = (params.workspaces ?? []).map((w) => ({
			kind: "local" as const,
			workspaceDir: w.workspaceDir,
			...(w.branch ? { branchOverride: w.branch } : {}),
		}));
		params.onProgress?.({
			workspace_dir: workspaces[0]?.workspaceDir ?? "",
			phase: "refreshing_base",
			index: 0,
			total: Math.max(1, workspaces.length),
		});
		const res = await this.client.request<{ sessionId: string; state: string; replay?: boolean }>("session.create", {
			sessionId: params.sessionId,
			...(params.projectId ? { projectId: params.projectId } : {}),
			...(params.metadata.title ? { name: String(params.metadata.title) } : {}),
			...(workspaces.length > 0 ? { workspaces } : {}),
			...(params.mcp ? { mcpSelection: { inventory_revision: params.mcp.inventoryRevision, servers: params.mcp.servers } } : {}),
			...(model ? { model } : {}),
			idempotencyKey: `start-${params.clientInputId}`,
		});
		params.onProgress?.({
			workspace_dir: workspaces[0]?.workspaceDir ?? "",
			phase: "done",
			index: Math.max(1, workspaces.length),
			total: Math.max(1, workspaces.length),
		});
		const text = contentBlocksToTextLocal(params.content);
		if (text.length > 0 && !res.replay) {
			const store = await this.ensureStore(res.sessionId);
			store.synthesizer?.registerEcho(text);
			await this.client.request("prompt.send", {
				sessionId: res.sessionId,
				text,
				idempotencyKey: `start-prompt-${params.clientInputId}`,
			});
			store.state = "running";
		}
		return { session_id: res.sessionId, activity: text.length > 0 ? "running" : "idle", replayed: res.replay === true };
	}

	async queueFollowUp(params: QueueFollowUpParams): Promise<FollowUpResult> {
		const store = await this.ensureAttached(params.sessionId);
		const text = contentBlocksToTextLocal(params.content);
		if (text.length === 0 && params.content.length > 0) {
			throw new Error("image inputs are not supported by the bridge backend");
		}
		const idle = mapActivity(store.state) === "idle";
		store.synthesizer?.registerEcho(text);
		if (!idle) {
			const queued: QueuedInput = {
				input_id: params.clientInputId,
				priority: "follow_up",
				status: "queued",
				content: params.content,
				editable: true,
				client_input_id: params.clientInputId,
				created_at: new Date().toISOString(),
			};
			store.synthesizer?.registerQueued(queued);
			store.queued = store.synthesizer?.queueSnapshot() ?? [...store.queued, queued];
			store.state = "running";
			this.dispatchFrames(store, [{
				event_id: 0,
				event: "input.queued",
				session_id: params.sessionId,
				data: { queued_input: queued, queued_inputs: store.queued, activity: "queued" },
			}]);
		}
		try {
			await this.client.request("prompt.send", {
				sessionId: params.sessionId,
				text,
				idempotencyKey: params.clientInputId,
			});
		} catch (error) {
			if (isBridgeErrorCode(error, "idempotency_conflict")) {
				return { input_id: params.clientInputId, accepted: true, queued: !idle, replayed: true, queue: null };
			}
			throw error;
		}
		return { input_id: params.clientInputId, accepted: true, queued: !idle, replayed: false, queue: null };
	}

	async interrupt(sessionId: string): Promise<InterruptResult> {
		await this.client.abort(sessionId);
		return { interrupted: true };
	}

	resumeTurn(_params: ResumeTurnParams): Promise<ResumeTurnResult> {
		unsupported("turn resume");
	}

	async switchHistory(params: SwitchHistoryParams): Promise<SwitchHistoryResult> {
		const store = await this.ensureFresh(params.sessionId);
		if (mapActivity(store.state) !== "idle") {
			throw new Error("stop the active turn before switching history");
		}
		const { points } = await this.client.request<{ points: BridgeForkPoint[] }>("session.getForkPoints", { sessionId: params.sessionId });
		const point = points.find((candidate) => (candidate.parentId ?? null) === (params.leafId ?? null));
		if (!point) {
			throw new Error("history_changed: the selected point is no longer available");
		}
		await this.client.request("session.switch", {
			sessionId: params.sessionId,
			entryId: point.entryId,
			idempotencyKey: newIdempotencyKey("switch"),
		});
		const fresh = await this.getStateFull(params.sessionId);
		resyncStore(store, fresh);
		return {
			session_id: params.sessionId,
			active_leaf_id: reportedLeafId(store),
			activity: mapActivity(store.state),
			session_revision: store.revision,
			queue_revision: store.revision,
			transcript_revision: store.revision,
			last_event_id: store.revision,
			active_branch_entry_ids: store.entries.map((entry) => entry.id),
			active_branch_entries: [...store.entries],
		};
	}

	async forkHistory(params: ForkHistoryParams): Promise<ForkHistoryResult> {
		const res = await this.client.request<{ childId?: string; newSessionId?: string }>("session.fork", {
			sessionId: params.sessionId,
			idempotencyKey: newIdempotencyKey("fork"),
		});
		const childId = res.childId ?? res.newSessionId;
		if (!childId) throw new Error("session.fork returned no child session id");
		return {
			session_id: childId,
			source_session_id: params.sessionId,
			active_leaf_id: null,
			session_revision: 0,
			queue_revision: 0,
			transcript_revision: 0,
			last_event_id: 0,
		};
	}

	async renameSession(sessionId: string, title: string): Promise<RenameSessionResult> {
		await this.client.request("session.rename", { id: sessionId, name: title });
		const store = this.stores.get(sessionId);
		if (store) store.name = title;
		return { session_id: sessionId, title, activity: mapActivity(store?.state ?? "idle") };
	}

	async deleteSession(sessionId: string): Promise<DeleteSessionResult> {
		await this.client.request("session.delete", { id: sessionId });
		const store = this.stores.get(sessionId);
		if (store) {
			store.attached = false;
			this.stores.delete(sessionId);
		}
		return { session_id: sessionId, deleted: true };
	}

	async configureSession(params: ConfigureSessionParams): Promise<ConfigureSessionResult> {
		const store = await this.ensureStore(params.sessionId);
		if (params.metadata && Object.keys(params.metadata).length > 0) {
			unsupported("session metadata writes (archive/flags)");
		}
		if (params.provider) {
			const model = bridgeModelString(params.provider);
			const current = store.model ? `${store.model.provider ?? ""}/${store.model.modelId ?? ""}` : null;
			if (model && model !== current) {
				const slash = model.indexOf("/");
				await this.client.setModel(params.sessionId, model.slice(0, slash), model.slice(slash + 1));
				store.model = { provider: model.slice(0, slash), modelId: model.slice(slash + 1), thinkingLevel: store.model?.thinkingLevel ?? null };
			}
			const level = toThinkingLevel(params.provider.reasoning_effort);
			if (level && level !== store.model?.thinkingLevel) {
				await this.client.setThinkingLevel(params.sessionId, level).catch(() => {});
				if (store.model) store.model = { ...store.model, thinkingLevel: level };
			}
		}
		return {
			session_id: params.sessionId,
			activity: mapActivity(store.state),
			provider: providerFromModel(store.model),
		};
	}

	// ---- queue management (unsupported on the bridge) -------------------------

	promoteQueuedInput(_sessionId: string, _inputId: string): Promise<PromoteQueuedResult> {
		unsupported("queue management (promote)");
	}

	updateQueuedInput(_sessionId: string, _inputId: string, _content: ContentBlock[]): Promise<UpdateQueuedResult> {
		unsupported("queue management (edit queued follow-up)");
	}

	cancelQueuedInput(_sessionId: string, _inputId: string): Promise<CancelQueuedResult> {
		unsupported("queue management (cancel queued follow-up)");
	}

	reorderQueuedFollowUps(_sessionId: string, _inputIds: string[]): Promise<ReorderQueuedResult> {
		unsupported("queue management (reorder)");
	}

	async requestCompaction(sessionId: string): Promise<{ action_row_id: string | null }> {
		await this.client.request("session.compact", { sessionId });
		return { action_row_id: null };
	}

	// ---- delegations (SUBAGENTS rail: read-only tree over subagent.tree) ------

	async listDelegations(parentSessionId: string, limit?: number): Promise<DelegationListResult> {
		const tree = await this.client.subagentTree(parentSessionId);
		const children = tree.children ?? [];
		if (children.length === 0) {
			return { parent_session_id: parentSessionId, limit, has_more: false, delegations: [] };
		}
		const subagents: DelegationSubagent[] = children.map((node: SubagentNode) => {
			const phases = node.phases ?? [];
			const status: DelegationSubagent["status"] =
				phases.includes("error") ? "failed"
				: phases.includes("completed") ? "done"
				: phases.includes("deleted") ? "cancelled"
				: node.status === "running" ? "running"
				: "idle";
			return {
				id: node.childSessionId ?? node.rlmChildId,
				status,
				role: node.name,
				title: node.name,
				type: "full",
				subagent_type: "full",
				steerable: node.childSessionId !== null,
			};
		});
		const anyRunning = subagents.some((subagent) => subagent.status === "running");
		const anyFailed = subagents.some((subagent) => subagent.status === "failed");
		const delegation: Delegation = {
			delegation_id: `bridge-tree-${parentSessionId}`,
			kind: "full",
			status: anyRunning ? "running" : anyFailed ? "done_with_failures" : "done",
			label: null,
			progress: {
				expected: subagents.length,
				spawned: subagents.length,
				terminal: subagents.filter((subagent) => ["done", "failed", "cancelled"].includes(String(subagent.status))).length,
				running: subagents.filter((subagent) => subagent.status === "running").length,
				failed: subagents.filter((subagent) => subagent.status === "failed").length,
			},
			subagents,
		};
		return { parent_session_id: parentSessionId, limit, has_more: false, delegations: [delegation] };
	}

	startFullDelegation(_params: StartFullDelegationParams): Promise<StartFullDelegationResult> {
		unsupported("user-launched delegations (bridge subagents are spawned by the model via the rlm tool)");
	}

	startReadonlyDelegationFanout(_params: StartReadonlyDelegationFanoutParams): Promise<StartReadonlyDelegationFanoutResult> {
		unsupported("user-launched delegations (bridge subagents are spawned by the model via the rlm tool)");
	}

	cancelDelegation(_parentSessionId: string, _delegationId: string): Promise<{ cancelled: boolean }> {
		unsupported("delegation cancel");
	}

	readHandoffFile(_params: ReadHandoffFileParams): Promise<ReadHandoffFileResult> {
		unsupported("delegation handoff files");
	}

	async steerSubagent(params: SteerSubagentParams): Promise<SteerSubagentResult> {
		await this.client.steer(params.subagentSessionId, params.message);
		return {
			subagent_id: params.subagentSessionId,
			accepted: true,
			queued: false,
			input_id: params.clientControlId ?? newIdempotencyKey("steer"),
			replayed: false,
			phase: "ready",
			interrupted: null,
			drive_status: "settled",
		};
	}

	// ---- inspector / MCP ------------------------------------------------------

	getSystemPrompt(_sessionId: string): Promise<SystemPromptResponse> {
		return Promise.resolve({
			template: "(the bridge contract does not expose the system prompt)",
			rendered: null,
		});
	}

	async listTools(_provider: string, sessionId?: string | null): Promise<ToolListing[]> {
		const listings: ToolListing[] = [{
			kind: "local_tool",
			name: "ipython",
			description: "IPython tool (persistent kernel)",
			input_schema: {},
		}];
		if (sessionId) {
			try {
				const inventory = await this.getMcpInventory("", BRIDGE_RUNTIME_ID, sessionId);
				const selected = new Set(inventory.selected_servers?.map((server) => server.server) ?? []);
				for (const server of inventory.servers) {
					if (inventory.selected_servers && !selected.has(server.server)) continue;
					for (const tool of server.tools) {
						listings.push({
							kind: "mcp_tool",
							server: server.server,
							raw_name: tool.raw_name,
							name: tool.raw_name,
							description: tool.description,
							input_schema: {},
							manifest_fingerprint: server.revision,
							contract_fingerprint: inventory.revision,
							health: server.health,
						});
					}
				}
			} catch {
				/* MCP optional */
			}
		}
		return listings;
	}

	async getMcpInventory(_provider: string, _runtimeId: string, sessionId?: string | null): Promise<McpInventory> {
		const inventory = await this.client.request<McpInventory>("mcp.inventory", {});
		if (sessionId) {
			const store = await this.ensureStore(sessionId);
			const selection = store.mcpSelection;
			if (selection?.servers) {
				return {
					...inventory,
					selected_servers: selection.servers.map((server) => ({ server: server.server, tools: server.tools })),
					session_revision: store.revision,
				};
			}
		}
		return inventory;
	}

	async addMcpTools(params: AddMcpToolsParams): Promise<AddMcpToolsResult> {
		const selection: McpSelection = params.selection;
		await this.client.request("mcp.select", {
			sessionId: params.sessionId,
			selection: { inventory_revision: selection.inventoryRevision, servers: selection.servers },
		});
		const store = this.stores.get(params.sessionId);
		if (store) store.mcpSelection = { inventoryRevision: selection.inventoryRevision, servers: selection.servers };
		return {
			session_id: params.sessionId,
			manifest_fingerprint: selection.inventoryRevision,
			session_revision: store?.revision ?? 0,
			queue_revision: store?.revision ?? 0,
			transcript_revision: store?.revision ?? 0,
		};
	}

	async getMcpStatus(_runtimeId: string): Promise<McpStatus> {
		const res = await this.client.request<{ servers: { server: string; auth_kind: "none" | "bearer" | "oauth"; status: string; detail?: string }[] }>("mcp.status", {});
		const servers: McpAuthServerStatus[] = res.servers.map((server) => {
			const state =
				server.status === "non_oauth" ? "not_applicable"
				: server.status === "bearer" ? "ready"
				: server.status === "login_required" ? "login_required"
				: server.status === "reauthentication_required" ? "reauthentication_required"
				: server.status === "authorization_pending" ? "authorization_pending"
				: server.status === "ready" ? "ready"
				: "unknown";
			return {
				server: server.server,
				auth_kind: server.auth_kind,
				auth_state: state,
				can_login: server.auth_kind === "oauth" && (state === "login_required" || state === "reauthentication_required"),
				can_logout: server.auth_kind === "oauth" && state === "ready",
			};
		});
		return { servers };
	}

	async loginMcp(server: string, _runtimeId: string): Promise<McpLoginResult> {
		const res = await this.client.request<{ authorizationUrl: string; state: string; expiresAtMs: number }>("mcp.login", { server });
		return {
			login_id: res.state,
			authorization_url: res.authorizationUrl,
			expires_at_unix_seconds: Math.floor(res.expiresAtMs / 1000),
		};
	}

	async completeMcpLogin(server: string, loginId: string, callbackUrl: string, _runtimeId: string): Promise<{ completed: true }> {
		const url = new URL(callbackUrl);
		const code = url.searchParams.get("code") ?? callbackUrl;
		await this.client.request("mcp.complete", { server, code, state: loginId });
		return { completed: true };
	}

	async cancelMcpLogin(server: string, _loginId: string, _runtimeId: string): Promise<{ cancelled: true }> {
		await this.client.request("mcp.cancel", { server });
		return { cancelled: true };
	}

	async logoutMcp(server: string, _runtimeId: string): Promise<McpLogoutResult> {
		const res = await this.client.request<{ removed: boolean }>("mcp.logout", { server });
		return { result: res.removed ? "removed" : "not_found" };
	}

	// ---- workspace / git ------------------------------------------------------

	async listWorkspaceDir(params: ListWorkspaceDirParams): Promise<WorkspaceDirListing> {
		const res = await this.client.request<{
			path: string;
			entries: { name: string; kind: string; size?: number | null; mtimeMs?: number | null }[];
			nextAfterName?: string;
		}>("workspace.list_dir", {
			sessionId: params.sessionId,
			path: params.path ?? "",
			...(params.afterName ? { afterName: params.afterName } : {}),
			...(params.limit !== undefined ? { limit: params.limit } : {}),
		});
		return {
			path: res.path,
			entries: res.entries.map((entry) => ({
				name: entry.name,
				kind: entry.kind === "file" || entry.kind === "directory" ? entry.kind : "other",
				size: entry.size ?? null,
				mtime_ms: entry.mtimeMs ?? null,
			})),
			next_after_name: res.nextAfterName ?? null,
		};
	}

	async readWorkspaceFile(params: ReadWorkspaceFileParams): Promise<WorkspaceFilePrefix> {
		const res = await this.client.request<{
			path: string;
			contentBase64: string;
			byteLen: number;
			totalSize: number;
			eof: boolean;
			mtimeMs?: number;
		}>("workspace.read_file", {
			sessionId: params.sessionId,
			path: params.path,
			...(params.offset !== undefined ? { offset: params.offset } : {}),
			...(params.maxBytes !== undefined ? { maxBytes: params.maxBytes } : {}),
		});
		return {
			path: res.path,
			content_base64: res.contentBase64,
			byte_len: res.byteLen,
			total_size: res.totalSize,
			eof: res.eof,
			mtime_ms: res.mtimeMs ?? null,
		};
	}

	watchWorkspace(_params: WatchWorkspaceParams): Promise<{ ok: boolean }> {
		// The bridge has no fs-watch push; the files pane refreshes on demand.
		return Promise.resolve({ ok: true });
	}

	async gitStatus(params: GitStatusParams): Promise<WorkspaceGitStatus> {
		const res = await this.client.request<{ roots: BridgeGitRoot[] }>("workspace.git_status", {
			sessionId: params.sessionId,
			against: params.against,
		});
		const roots: LegacyGitStatusRoot[] = res.roots.map((root) => ({
			workspace_dir: root.workspaceDir,
			comparison: root.comparison
				? {
					base: { branch: root.comparison.baseBranch, oid: root.comparison.mergeBaseOid },
					tip: { branch: root.comparison.baseBranch, oid: root.comparison.headOid },
					merge_base_oid: root.comparison.mergeBaseOid,
				}
				: null,
			error: root.error,
			entries: root.entries.map((entry) => ({
				path: entry.path,
				status: entry.status === "renamed" ? "modified" : entry.status as LegacyGitStatusRoot["entries"][number]["status"],
			})),
		}));
		return { against: params.against, roots };
	}

	async gitDiff(params: GitDiffParams): Promise<WorkspaceGitDiff> {
		const res = await this.client.request<{
			diff: string;
			path?: string;
			binary?: boolean;
			truncated?: boolean;
			comparison?: { baseBranch: string; mergeBaseOid: string; headOid: string } | null;
		}>("workspace.git_diff", {
			sessionId: params.sessionId,
			path: params.path,
			against: params.against,
		});
		return {
			path: res.path ?? params.path,
			against: params.against,
			comparison: res.comparison
				? {
					base: { branch: res.comparison.baseBranch, oid: res.comparison.mergeBaseOid },
					tip: { branch: res.comparison.baseBranch, oid: res.comparison.headOid },
					merge_base_oid: res.comparison.mergeBaseOid,
				}
				: null,
			status: null,
			unified: res.diff,
			binary: res.binary ?? false,
			truncated: res.truncated ?? false,
		};
	}
}

export function createBridgeAgentApi(url?: string): BridgeAgentApi {
	return new BridgeAgentApi(url);
}
