import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync, renameSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import type { AgentMessage, AgentToolCall, ThinkingLevel } from "@pi-relay/agent-core";
import { isBackgroundToolCompletionMessage, isPendingToolResult } from "@pi-relay/agent-core";
import { validateToolArguments, type Model, type ToolResultMessage, type UserMessage } from "@pi-relay/ai";
import { serializeConversation, type AgentSessionEvent, type ToolDefinition } from "@pi-relay/coding-agent";
import { createAgentContextTransform } from "./context-transform.js";
import { BackgroundCapabilitiesSource, MultiAgentInstructionsSource } from "./prompt/index.js";
import { createMessageTool } from "./tools/message.js";
import { createReportTool } from "./tools/report.js";
import { createSpawnTool } from "./tools/spawn.js";
import {
	createAgentDirectiveMessage,
	createAgentIdleMessage,
	createAgentReportMessage,
} from "./messages.js";
import { buildAncestorWorklogPrefix, buildWorklogPrompt, appendWorklogEntry, computeTopicVocabulary, getLastWorklogEntry, parseWorklogEntries, readWorklog, WORKLOG_UPDATE_TOOL } from "./worklog.js";
import { ToolCallTracker } from "./tool-tracker.js";
import {
	DEFAULT_ORCHESTRATOR_CONFIG,
	type AgentRecord,
	type AgentTreeMetadata,
	type AgentTreeMetadataEntry,
	type AgentSessionFactory,
	type AgentSessionHandle,
	type OrchestratorConfig,
	type OrchestratorOptions,
	type SessionCustomMessage,
	type SessionStats,
	type SpawnConfig,
	type SubtreeUsageStats,
} from "./types.js";

function slugifyRole(role: string): string {
	const base = role
		.toLowerCase()
		.replace(/[^a-z0-9]+/g, "-")
		.replace(/^-+|-+$/g, "");
	return base || "agent";
}

function createAgentId(role: string): string {
	return `${slugifyRole(role)}-${randomUUID().slice(0, 8)}`;
}

function ensureDir(path: string): void {
	mkdirSync(path, { recursive: true });
}

interface PendingSpawnDraft {
	id: string;
	role: string;
	prompt: string;
}

/**
 * Result of the pre-fork triviality gate. `skip === true` means do NOT
 * schedule a worklog fork for this turn; `reason` is a stable identifier
 * surfaced in debug logs and tests.
 */
export interface WorklogForkGateResult {
	skip: boolean;
	reason?: string;
}

/**
 * Per-fork telemetry shape consumed by `logWorklogFork`. Kept small enough
 * that a single JSON line is trivially grep-friendly.
 */
export interface WorklogForkLogEntry {
	agentId: string;
	turn: number;
	deltaMsgCount: number;
	skipped: boolean;
	skipReason?: string;
	entryEmitted: boolean;
}

/**
 * Estimate the size of an assistant thinking block in tokens. The input to
 * the gate is raw `AgentMessage` content (not a provider response), so we
 * approximate with character-count / 4 — good enough to distinguish "no
 * meaningful thinking" from "substantive thinking" for the purposes of
 * skipping pure tool-chatter turns.
 */
function approximateThinkingTokens(message: AgentMessage): number {
	if (message.role !== "assistant") {
		return 0;
	}
	let chars = 0;
	for (const content of message.content) {
		if (content.type === "thinking") {
			chars += content.thinking.length;
		}
	}
	return Math.ceil(chars / 4);
}

function assistantHasSubstantiveText(message: AgentMessage): boolean {
	if (message.role !== "assistant") {
		return false;
	}
	for (const content of message.content) {
		if (content.type === "text" && content.text.trim().length > 0) {
			return true;
		}
	}
	return false;
}

/**
 * Decide whether a just-completed turn is trivial enough to skip the
 * worklog fork. The first rule is a HARD GATE: if the delta since
 * `lastWorklogMessageCount` contains no `assistant` message at all, we
 * unconditionally skip — there's no new assistant output to mine for
 * durable knowledge. The remaining rules are additional optional gates on
 * top of that.
 *
 * The function is pure and exported so callers (and tests) can reason
 * about its decisions without spinning up an orchestrator.
 */
export function isLikelyTrivialTurn(
	record: Pick<AgentRecord, "lastWorklogMessageCount">,
	turnMessages: AgentMessage[],
): WorklogForkGateResult {
	const start = Math.min(record.lastWorklogMessageCount, turnMessages.length);
	const delta = turnMessages.slice(start);

	// HARD GATE: a turn with no new assistant message cannot contribute
	// anything the worklog cares about. Always skip.
	const hasAssistant = delta.some((message) => message.role === "assistant");
	if (!hasAssistant) {
		return { skip: true, reason: "no-new-assistant-message" };
	}

	// The remaining heuristics are only sensible when the gate above has
	// already passed.

	// Tool-chatter only: every assistant message in the delta is purely
	// toolCall content (no text, no substantive thinking). Such turns
	// rarely carry durable insight — the durable parts land on the
	// wrap-up turn that contains the reasoned summary.
	const assistantMessages = delta.filter((message) => message.role === "assistant");
	const anyTextOrThinking = assistantMessages.some((message) => {
		if (assistantHasSubstantiveText(message)) {
			return true;
		}
		return approximateThinkingTokens(message) > 50;
	});
	if (!anyTextOrThinking) {
		return { skip: true, reason: "tool-chatter-only" };
	}

	return { skip: false };
}

/**
 * Debug-logged by setting `PI_RELAY_WORKLOG_DEBUG=1` in the environment.
 * Emits one JSON line per fork decision (skipped or completed). Kept
 * deliberately minimal so logs are grep-friendly without pulling in a
 * logging dependency.
 */
function logWorklogFork(entry: WorklogForkLogEntry): void {
	if (!process.env.PI_RELAY_WORKLOG_DEBUG) {
		return;
	}
	try {
		console.debug(`[pi:worklog-fork] ${JSON.stringify(entry)}`);
	} catch {
		// Never let logging crash the orchestrator.
	}
}

export class Orchestrator {
	private readonly records = new Map<string, AgentRecord>();
	private readonly sessionIdToAgentId = new Map<string, string>();
	private readonly config: OrchestratorConfig;
	private readonly toolTracker = new ToolCallTracker();
	private readonly sessionFactory: AgentSessionFactory;
	private readonly workspaceDir: string;
	private readonly agentsDir: string;
	private readonly worklogDir: string;
	private readonly treeFile: string;
	private readonly changeListeners = new Set<() => void>();
	private readonly pendingWorklogFork = new Map<string, Promise<void>>();
	private readonly pendingSpawnDrafts = new Map<string, Map<string, PendingSpawnDraft>>();
	private readonly restoredDisposedEntries = new Map<string, AgentTreeMetadataEntry>();
	private _isDisposing = false;
	private treeWriteChain: Promise<void> = Promise.resolve();

	readonly rootAgentId: string;

	constructor(options: OrchestratorOptions) {
		this.config = { ...DEFAULT_ORCHESTRATOR_CONFIG, ...options.config };
		this.sessionFactory = options.sessionFactory;
		this.rootAgentId = options.rootAgentId ?? "root";
		this.workspaceDir =
			options.workspaceDir ??
			join(options.rootSession.sessionManager.getSessionDir(), options.rootSession.sessionId);
		this.agentsDir = join(this.workspaceDir, "agents");
		this.worklogDir = join(this.workspaceDir, "worklogs");
		this.treeFile = join(this.workspaceDir, "tree.json");

		ensureDir(this.workspaceDir);
		ensureDir(this.agentsDir);
		ensureDir(this.worklogDir);

		const rootRecord: AgentRecord = {
			id: this.rootAgentId,
			session: options.rootSession,
			status: "idle",
			parentId: null,
			childIds: [],
			role: options.rootRole ?? "root",
			config: {
				role: options.rootRole ?? "root",
				prompt: "",
			},
			reactivating: false,
			worklogFile: this.getWorklogFile(this.rootAgentId),
			createdAt: Date.now(),
			lastStatusChange: Date.now(),
			lastWorklogTurn: 0,
			lastWorklogMessageCount: 0,
			turnCount: 0,
			pendingRestoreIdleNotice: false,
			orphanedPendingToolCallIds: [],
		};
		this.registerRecord(rootRecord);
		void this.persistTree();
	}

	get isDisposing(): boolean {
		return this._isDisposing;
	}

	getRecord(agentId: string): AgentRecord {
		const record = this.records.get(agentId);
		if (!record) {
			throw new Error(`Unknown agent: ${agentId}`);
		}
		return record;
	}

	getAgentIdBySessionId(sessionId: string): string | undefined {
		return this.sessionIdToAgentId.get(sessionId);
	}

	/**
	 * Current worklog-fork model override, or `undefined` when the fork
	 * falls back to the parent session's model.
	 */
	getForkModel(): Model<any> | undefined {
		return this.config.forkModel;
	}

	/**
	 * Current worklog-fork thinking level override, or `undefined` when the
	 * fork falls back to the parent session's thinking level.
	 */
	getForkThinkingLevel(): ThinkingLevel | undefined {
		return this.config.forkThinkingLevel;
	}

	/**
	 * Set (or clear) the worklog-fork model override. Takes effect on the
	 * next fork invocation; no in-flight forks are cancelled. Pass
	 * `undefined` to clear the override and fall back to the parent
	 * session's model. The orchestrator does NOT persist this choice —
	 * the caller is responsible for writing to whatever settings store
	 * should survive restart.
	 */
	setForkModel(model: Model<any> | undefined): void {
		this.config.forkModel = model;
		this.notifyChange();
	}

	/**
	 * Set (or clear) the worklog-fork thinking level override. Same
	 * semantics as `setForkModel`: next-fork takes effect, no persistence.
	 */
	setForkThinkingLevel(level: ThinkingLevel | undefined): void {
		this.config.forkThinkingLevel = level;
		this.notifyChange();
	}

	getChildrenOf(agentId: string): AgentRecord[] {
		return this.getRecord(agentId).childIds
			.map((childId) => this.records.get(childId))
			.filter((record): record is AgentRecord => record !== undefined && record.status !== "disposed");
	}

	getAgentSummaries(): Array<{
		id: string;
		parentId: string | null;
		role: string;
		status: AgentRecord["status"];
		depth: number;
		childCount: number;
		sessionFile: string | undefined;
		lastOutput: string | undefined;
	}> {
		const summaries: Array<{
			id: string;
			parentId: string | null;
			role: string;
			status: AgentRecord["status"];
			depth: number;
			childCount: number;
			sessionFile: string | undefined;
			lastOutput: string | undefined;
		}> = [];
		const visit = (agentId: string, depth: number) => {
			const record = this.records.get(agentId);
			if (!record || record.status === "disposed") {
				return;
			}

			summaries.push({
				id: record.id,
				parentId: record.parentId,
				role: record.role,
				status: record.status,
				depth,
				childCount: record.childIds.length,
				sessionFile: record.session.sessionFile,
				lastOutput: record.session.getLastAssistantText(),
			});

			for (const childId of record.childIds) {
				visit(childId, depth + 1);
			}
		};

		visit(this.rootAgentId, 0);
		return summaries;
	}

	/**
	 * Aggregate usage stats across the given agent and every descendant in its
	 * subtree. Returns `undefined` when the agent id is unknown.
	 *
	 * Counts and token/cost fields are summed; session-identifying fields
	 * (`sessionId`, `sessionFile`, `contextUsage`) on the tree aggregate are
	 * copied from the attached agent's `self` stats since they describe that
	 * agent specifically.
	 *
	 * A visited-set guard protects against pathological cycles in `childIds`;
	 * the tree is expected to be acyclic by construction.
	 */
	aggregateSubtreeUsage(agentId: string): SubtreeUsageStats | undefined {
		const root = this.records.get(agentId);
		if (!root) {
			return undefined;
		}

		const self = root.session.getSessionStats();
		const visited = new Set<string>([agentId]);
		const treeAcc = {
			userMessages: self.userMessages,
			assistantMessages: self.assistantMessages,
			toolCalls: self.toolCalls,
			toolResults: self.toolResults,
			totalMessages: self.totalMessages,
			input: self.tokens.input,
			output: self.tokens.output,
			cacheRead: self.tokens.cacheRead,
			cacheWrite: self.tokens.cacheWrite,
			totalTokens: self.tokens.total,
			cost: self.cost,
		};
		let descendantCount = 0;

		const visit = (id: string): void => {
			const record = this.records.get(id);
			if (!record) {
				return;
			}
			for (const childId of record.childIds) {
				if (visited.has(childId)) {
					continue;
				}
				const child = this.records.get(childId);
				if (!child || child.status === "disposed") {
					continue;
				}
				visited.add(childId);
				descendantCount += 1;
				const childStats = child.session.getSessionStats();
				treeAcc.userMessages += childStats.userMessages;
				treeAcc.assistantMessages += childStats.assistantMessages;
				treeAcc.toolCalls += childStats.toolCalls;
				treeAcc.toolResults += childStats.toolResults;
				treeAcc.totalMessages += childStats.totalMessages;
				treeAcc.input += childStats.tokens.input;
				treeAcc.output += childStats.tokens.output;
				treeAcc.cacheRead += childStats.tokens.cacheRead;
				treeAcc.cacheWrite += childStats.tokens.cacheWrite;
				treeAcc.totalTokens += childStats.tokens.total;
				treeAcc.cost += childStats.cost;
				visit(childId);
			}
		};
		visit(agentId);

		const tree: SessionStats = {
			sessionFile: self.sessionFile,
			sessionId: self.sessionId,
			userMessages: treeAcc.userMessages,
			assistantMessages: treeAcc.assistantMessages,
			toolCalls: treeAcc.toolCalls,
			toolResults: treeAcc.toolResults,
			totalMessages: treeAcc.totalMessages,
			tokens: {
				input: treeAcc.input,
				output: treeAcc.output,
				cacheRead: treeAcc.cacheRead,
				cacheWrite: treeAcc.cacheWrite,
				total: treeAcc.totalTokens,
			},
			cost: treeAcc.cost,
			contextUsage: self.contextUsage,
		};

		return {
			agentId,
			hasDescendants: descendantCount > 0,
			self,
			tree,
		};
	}

	findAgentIdBySessionFile(sessionFile: string): string | undefined {
		const resolvedSessionFile = resolve(sessionFile);
		for (const record of this.records.values()) {
			if (record.status === "disposed" || !record.session.sessionFile) {
				continue;
			}
			if (resolve(record.session.sessionFile) === resolvedSessionFile) {
				return record.id;
			}
		}
		return undefined;
	}

	subscribeToChanges(listener: () => void): () => void {
		this.changeListeners.add(listener);
		return () => {
			this.changeListeners.delete(listener);
		};
	}

	async restore(): Promise<boolean> {
		if (!existsSync(this.treeFile)) {
			await this.persistTree();
			return false;
		}

		const metadata = this.readTreeMetadata();
		if (!metadata) {
			await this.persistTree();
			return false;
		}

		const rootRecord = this.getRecord(this.rootAgentId);
		const rootEntry = metadata.agents[this.rootAgentId];
		if (rootEntry) {
			rootRecord.role = rootEntry.role;
			rootRecord.config = rootEntry.spawnConfig;
			rootRecord.lastWorklogTurn = rootEntry.lastWorklogTurn;
			rootRecord.lastWorklogMessageCount = rootEntry.lastWorklogMessageCount ?? 0;
			rootRecord.turnCount = rootEntry.turnCount ?? rootEntry.lastWorklogTurn;
		}
		rootRecord.orphanedPendingToolCallIds = this.appendInterruptedToolResults(rootRecord.session);

		const childEntries = Object.values(metadata.agents)
			.filter((entry) => entry.id !== this.rootAgentId && entry.status !== "disposed")
			.sort((left, right) => this.getMetadataDepth(metadata, left.id) - this.getMetadataDepth(metadata, right.id));

		for (const entry of childEntries) {
			const parent = entry.parentId ? this.records.get(entry.parentId) : undefined;
			if (!parent) {
				continue;
			}
			if (!entry.sessionFile || !existsSync(entry.sessionFile) || statSync(entry.sessionFile).size === 0) {
				this.restoredDisposedEntries.set(entry.id, {
					...entry,
					status: "disposed",
				});
				continue;
			}

			const created = await this.sessionFactory({
				mode: "restore",
				agentId: entry.id,
				parentId: entry.parentId,
				config: entry.spawnConfig,
				sessionFile: entry.sessionFile,
				customTools: this.createChildTools(entry.id),
				parentSession: parent.session,
				sessionDir: this.agentsDir,
			});

			await created.session.bindExtensions({});

			const record: AgentRecord = {
				id: entry.id,
				session: created.session,
				status: entry.status === "running" ? "idle" : "idle",
				parentId: entry.parentId,
				childIds: [],
				role: entry.role,
				config: entry.spawnConfig,
				reactivating: false,
				worklogFile: entry.worklogFile,
				createdAt: entry.createdAt,
				lastStatusChange: Date.now(),
				lastWorklogTurn: entry.lastWorklogTurn,
				lastWorklogMessageCount: entry.lastWorklogMessageCount ?? 0,
				turnCount: entry.turnCount ?? entry.lastWorklogTurn,
				pendingRestoreIdleNotice: entry.status === "running",
				orphanedPendingToolCallIds: [],
			};
			this.registerRecord(record);
			record.orphanedPendingToolCallIds = this.appendInterruptedToolResults(record.session);
		}

		for (const entry of Object.values(metadata.agents)) {
			const record = this.records.get(entry.id);
			if (!record) {
				continue;
			}
			record.childIds = entry.childIds.filter((childId) => {
				const child = this.records.get(childId);
				return child !== undefined && child.status !== "disposed";
			});
		}

		for (const record of [...this.records.values()]) {
			if (!record.pendingRestoreIdleNotice || !record.parentId) {
				continue;
			}
			record.pendingRestoreIdleNotice = false;
			const idleMessage = createAgentIdleMessage(
				record.id,
				record.role,
				{ note: "Session restored from interrupted state." },
			);
			if (record.parentId === this.rootAgentId) {
				await this.getRecord(this.rootAgentId).session.sendCustomMessage(idleMessage);
				continue;
			}
			await this.deliverMessage(record.parentId, idleMessage);
		}
		await this.persistTree();
		return true;
	}

	async spawnAgent(parentId: string, config: SpawnConfig): Promise<string> {
		const parent = this.getRecord(parentId);
		this.assertSpawnAllowed(parentId);

		const agentId = createAgentId(config.role);
		this.addPendingSpawnDraft(parentId, {
			id: agentId,
			role: config.role,
			prompt: config.prompt,
		});

		try {
			const childCustomTools = this.createChildTools(agentId);
			const created = await this.sessionFactory({
				mode: "spawn",
				agentId,
				parentId,
				config,
				customTools: childCustomTools,
				parentSession: parent.session,
				sessionDir: this.agentsDir,
			});

			await created.session.bindExtensions({});

			const record: AgentRecord = {
				id: agentId,
				session: created.session,
				status: "running",
				parentId,
				childIds: [],
				role: config.role,
				config,
				reactivating: false,
				worklogFile: this.getWorklogFile(agentId),
				createdAt: Date.now(),
				lastStatusChange: Date.now(),
				lastWorklogTurn: 0,
				lastWorklogMessageCount: 0,
				turnCount: 0,
				pendingRestoreIdleNotice: false,
				orphanedPendingToolCallIds: [],
			};
			parent.childIds.push(agentId);
			this.registerRecord(record);
			const prompt = await this.buildSpawnPrompt(parentId, agentId, config.prompt);
			void created.session.prompt(prompt).catch((error) => {
				void this.handleAgentError(agentId, error);
			});

			return agentId;
		} finally {
			this.removePendingSpawnDraft(parentId, agentId);
		}
	}

	async routeMessage(fromAgentId: string, targetAgentId: string, content: string): Promise<void> {
		const source = this.getRecord(fromAgentId);
		if (!source.childIds.includes(targetAgentId)) {
			throw new Error(`Agent ${targetAgentId} is not a direct child of ${fromAgentId}`);
		}

		await this.deliverMessage(
			targetAgentId,
			createAgentDirectiveMessage(fromAgentId, source.role, content),
			{ waitForTurn: false },
		);
	}

	async handleReport(agentId: string, content: string): Promise<void> {
		const record = this.getRecord(agentId);
		if (!record.parentId) {
			return;
		}

		await this.deliverMessage(
			record.parentId,
			createAgentReportMessage(agentId, record.role, content),
			{ waitForTurn: false },
		);
	}

	async dispose(): Promise<void> {
		if (this._isDisposing) {
			return;
		}
		this._isDisposing = true;

		for (const childId of [...this.getRecord(this.rootAgentId).childIds]) {
			await this.disposeAgent(childId);
		}

		this.setStatus(this.rootAgentId, "disposed");
		await this.persistTree();
	}

	private assertSpawnAllowed(parentId: string): void {
		const parent = this.getRecord(parentId);
		const activeChildCount = parent.childIds.filter((childId) => this.records.get(childId)?.status === "running").length;
		const pendingDirectChildren = this.pendingSpawnDrafts.get(parentId)?.size ?? 0;
		if (activeChildCount + pendingDirectChildren >= this.config.maxChildren) {
			throw new Error(`Agent ${parentId} already has the maximum number of children`);
		}

		let depth = 0;
		let current: AgentRecord | undefined = parent;
		while (current) {
			depth++;
			current = current.parentId ? this.records.get(current.parentId) : undefined;
		}
		if (depth >= this.config.maxDepth) {
			throw new Error(`Spawning from ${parentId} would exceed the maximum agent depth`);
		}

		const pendingSpawns = [...this.pendingSpawnDrafts.values()].reduce((total, drafts) => total + drafts.size, 0);
		const activeAgents = [...this.records.values()].filter((record) => record.status === "running").length + pendingSpawns;
		if (activeAgents >= this.config.maxActiveAgents) {
			throw new Error("The orchestrator is already at its active agent limit");
		}
	}

	private registerRecord(record: AgentRecord): void {
		const baseTransform = record.session.agent.transformContext;
		record.session.agent.transformContext = createAgentContextTransform(this, record.id, baseTransform);
		record.unsubscribe = record.session.subscribe((event) => {
			void this.handleSessionEvent(record.id, event);
		});
		record.session.agent.onBackgroundToolStart = (context) => {
			this.toolTracker.attachAbortController(context.toolCallId, context.abortController);
		};
		record.session.agent.onBackgroundToolEnd = (context) => {
			this.toolTracker.complete(context.toolCallId, context.status);
		};
		record.session.addPromptSource(new BackgroundCapabilitiesSource());
		record.session.addPromptSource(
			new MultiAgentInstructionsSource({
				role: record.role,
				hasParent: record.parentId !== null,
			}),
		);
		this.records.set(record.id, record);
		this.sessionIdToAgentId.set(record.session.sessionId, record.id);
		this.restoredDisposedEntries.delete(record.id);
		this.notifyChange();
		void this.persistTree();
	}

	private async handleSessionEvent(agentId: string, event: AgentSessionEvent): Promise<void> {
		const record = this.records.get(agentId);
		if (!record || record.status === "disposed") {
			return;
		}

		if (event.type === "agent_start") {
			this.setStatus(agentId, "running");
			return;
		}

		if (event.type === "turn_end") {
			record.turnCount += 1;
			const turnMessages = [...record.session.agent.state.messages];
			// Phase 1 (fork gating): skip the fork entirely on trivial turns
			// before paying the LLM bill. Gating-skipped turns still advance
			// `turnCount` but do NOT advance `lastWorklogMessageCount`, so the
			// next non-trivial turn's fork will see the accumulated delta.
			const gate = isLikelyTrivialTurn(record, turnMessages);
			if (gate.skip) {
				logWorklogFork({
					agentId,
					turn: record.turnCount,
					deltaMsgCount: Math.max(0, turnMessages.length - record.lastWorklogMessageCount),
					skipped: true,
					skipReason: gate.reason,
					entryEmitted: false,
				});
				return;
			}
			this.scheduleWorklogFork(agentId, record.turnCount, turnMessages);
			return;
		}

		if (event.type === "tool_execution_start") {
			this.toolTracker.register(agentId, event.toolCallId, event.toolName);
			return;
		}

		if (event.type === "tool_execution_end") {
			this.toolTracker.complete(event.toolCallId, event.isError ? "aborted" : "completed");
			return;
		}

		if (event.type === "agent_end") {
			this.scheduleIdleFinalization(agentId);
			return;
		}

		if (event.type === "compaction_end" && !event.willRetry) {
			this.scheduleIdleFinalization(agentId);
		}
	}

	private async finalizeIdle(agentId: string): Promise<void> {
		const record = this.records.get(agentId);
		if (!record || record.status === "disposed") {
			return;
		}

		if (
			record.session.isStreaming ||
			record.session.isRetrying ||
			record.session.isCompacting ||
			record.session.agent.hasQueuedMessages()
		) {
			return;
		}

		if (this.hasRunningChildren(agentId)) {
			this.setStatus(agentId, "running");
			return;
		}

		this.setStatus(agentId, "idle");

		if (!record.parentId) {
			return;
		}

		await this.deliverIdleMessage(
			record.parentId,
			agentId,
			createAgentIdleMessage(agentId, record.role),
		);
	}

	private async handleAgentError(agentId: string, error: unknown): Promise<void> {
		const record = this.records.get(agentId);
		if (!record || record.status === "disposed") {
			return;
		}

		this.setStatus(agentId, "idle");

		if (!record.parentId) {
			return;
		}

		const errorMessage = error instanceof Error ? error.message : String(error);
		await this.deliverMessage(
			record.parentId,
			createAgentIdleMessage(agentId, record.role, { errorMessage }),
		);
	}

	private scheduleIdleFinalization(agentId: string): void {
		void Promise.resolve()
			.then(async () => {
				const record = this.records.get(agentId);
				if (!record || record.status === "disposed") {
					return;
				}

				await record.session.agent.waitForIdle();
				await this.finalizeIdle(agentId);
			})
			.catch(() => {
				// Ignore shutdown races and let explicit error/dispose paths own cleanup.
			});
	}

	private async deliverMessage(
		targetAgentId: string,
		message: SessionCustomMessage,
		options: { waitForTurn?: boolean } = {},
	): Promise<void> {
		const target = this.getRecord(targetAgentId);
		if (target.status === "disposed") {
			return;
		}

		const targetIsBusy = target.session.isStreaming || target.session.isRetrying || target.session.isCompacting;
		if (!targetIsBusy && !target.reactivating) {
			target.reactivating = true;
			this.setStatus(targetAgentId, "running");
			const reactivation = target.session
				.sendCustomMessage(message, { triggerTurn: true })
				.catch(async (error) => {
					await this.handleAgentError(targetAgentId, error);
				})
				.finally(() => {
					target.reactivating = false;
				});
			if (options.waitForTurn ?? true) {
				await reactivation;
			}
			return;
		}

		try {
			await target.session.sendCustomMessage(message, { deliverAs: "steer" });
		} catch (error) {
			await this.handleAgentError(targetAgentId, error);
		}
	}

	private async deliverIdleMessage(
		targetAgentId: string,
		sourceAgentId: string,
		message: SessionCustomMessage,
	): Promise<void> {
		const target = this.getRecord(targetAgentId);
		if (target.status === "disposed") {
			return;
		}

		if (this.hasRunningChildren(targetAgentId, sourceAgentId)) {
			try {
				await target.session.sendCustomMessage(message);
			} catch (error) {
				await this.handleAgentError(targetAgentId, error);
			}
			return;
		}

		await this.deliverMessage(targetAgentId, message);
	}

	private async disposeAgent(agentId: string): Promise<void> {
		const record = this.records.get(agentId);
		if (!record || record.status === "disposed") {
			return;
		}

		for (const childId of [...record.childIds]) {
			await this.disposeAgent(childId);
		}

		this.toolTracker.killAllForAgent(agentId);
		await record.session.extensionRunner?.emit({ type: "session_shutdown" });
		record.session.abortCompaction?.();
		record.session.abortBranchSummary?.();
		try {
			await record.session.abort();
		} catch {
			// Best-effort shutdown.
		}
		try {
			await record.session.agent.waitForIdle();
		} catch {
			// Ignore shutdown races.
		}
		record.session.agent.mailbox?.close();
		record.unsubscribe?.();
		record.session.dispose();

		if (record.parentId) {
			const parent = this.records.get(record.parentId);
			if (parent) {
				parent.childIds = parent.childIds.filter((childId) => childId !== agentId);
			}
		}

		this.sessionIdToAgentId.delete(record.session.sessionId);
		this.setStatus(agentId, "disposed");
	}

	private setStatus(agentId: string, status: AgentRecord["status"]): void {
		const record = this.records.get(agentId);
		if (!record) {
			return;
		}
		record.status = status;
		record.lastStatusChange = Date.now();
		this.notifyChange();
		void this.persistTree();
	}

	private notifyChange(): void {
		for (const listener of this.changeListeners) {
			listener();
		}
	}

	private hasRunningChildren(agentId: string, excludingAgentId?: string): boolean {
		const record = this.records.get(agentId);
		if (!record) {
			return false;
		}

		return record.childIds.some((childId) => {
			if (childId === excludingAgentId) {
				return false;
			}
			return this.records.get(childId)?.status === "running";
		});
	}

	private createChildTools(agentId: string): ToolDefinition[] {
		return [createSpawnTool(this, agentId), createMessageTool(this, agentId), createReportTool(this, agentId)];
	}

	private getWorklogFile(agentId: string): string {
		const filePath = join(this.worklogDir, `${agentId}.worklog.md`);
		ensureDir(dirname(filePath));
		return filePath;
	}

	private getMetadataDepth(metadata: AgentTreeMetadata, agentId: string): number {
		let depth = 0;
		let current: AgentTreeMetadataEntry | undefined = metadata.agents[agentId];
		while (current) {
			depth++;
			current = current.parentId ? metadata.agents[current.parentId] : undefined;
		}
		return depth;
	}

	private snapshotTree(): AgentTreeMetadata {
		const agents: Record<string, AgentTreeMetadataEntry> = {};
		for (const [agentId, entry] of this.restoredDisposedEntries) {
			agents[agentId] = {
				...entry,
			};
		}
		for (const record of this.records.values()) {
			agents[record.id] = {
				id: record.id,
				parentId: record.parentId,
				childIds: [...record.childIds],
				role: record.role,
				status: record.status,
				spawnConfig: record.config,
				sessionFile: record.session.sessionFile,
				worklogFile: record.worklogFile,
				createdAt: record.createdAt,
				lastStatusChange: record.lastStatusChange,
				lastWorklogTurn: record.lastWorklogTurn,
				lastWorklogMessageCount: record.lastWorklogMessageCount,
				turnCount: record.turnCount,
			};
		}
		return {
			sessionId: this.getRecord(this.rootAgentId).session.sessionId,
			agents,
		};
	}

	private persistTree(): Promise<void> {
		const snapshot = this.snapshotTree();
		this.treeWriteChain = this.treeWriteChain.catch(() => undefined).then(async () => {
			const tempFile = `${this.treeFile}.tmp`;
			writeFileSync(tempFile, JSON.stringify(snapshot, null, 2), "utf-8");
			renameSync(tempFile, this.treeFile);
		});
		return this.treeWriteChain;
	}

	private readTreeMetadata(): AgentTreeMetadata | undefined {
		try {
			return JSON.parse(readFileSync(this.treeFile, "utf-8")) as AgentTreeMetadata;
		} catch {
			return undefined;
		}
	}

	private async buildSpawnPrompt(parentId: string, agentId: string, prompt: string): Promise<string> {
		const ancestors: Array<{
			id: string;
			role: string;
			worklogFile: string;
			lastWorklogMessageCount: number;
			messages: AgentMessage[];
		}> = [];
		let current: AgentRecord | undefined = this.records.get(parentId);
		while (current) {
			ancestors.unshift({
				id: current.id,
				role: current.role,
				worklogFile: current.worklogFile,
				lastWorklogMessageCount: current.lastWorklogMessageCount,
				messages: [...current.session.agent.state.messages],
			});
			current = current.parentId ? this.records.get(current.parentId) : undefined;
		}

		const sections: string[] = [];

		// Build the ancestor-worklog prefix once across ALL ancestors so
		// `buildAncestorWorklogPrefix` can apply cross-file supersession
		// tombstones (a parent entry can tombstone a grandparent entry). This
		// also clusters the byte-stable worklog blocks at the front of the
		// prompt ahead of the varying `<ancestor-recent-context>` tails, which
		// is the shape later PRs (pinned facts, spawn-prefix caching) rely on.
		const worklogSection = await buildAncestorWorklogPrefix(
			ancestors.map((ancestor) => ({
				agentId: ancestor.id,
				role: ancestor.role,
				filePath: ancestor.worklogFile,
			})),
		);
		if (worklogSection) {
			sections.push(worklogSection);
		}

		for (const ancestor of ancestors) {
			const recentContext = await this.serializeRecentAncestorContext(ancestor);
			if (recentContext) {
				sections.push(
					`<ancestor-recent-context agent="${ancestor.id}" role="${ancestor.role}">\n${recentContext}\n</ancestor-recent-context>`,
				);
			}
		}

		const siblingBatch = this.buildSiblingBatchPrefix(parentId, agentId);
		if (siblingBatch) {
			sections.push(siblingBatch);
		}

		if (sections.length === 0) {
			return prompt;
		}
		return `${sections.join("\n\n")}\n\n${prompt}`;
	}

	private scheduleWorklogFork(agentId: string, turn: number, turnMessages: AgentMessage[]): void {
		const previous = this.pendingWorklogFork.get(agentId) ?? Promise.resolve();
		const next = previous.then(() => this.runWorklogFork(agentId, turn, turnMessages)).catch(() => {
			// Best-effort worklog generation should not poison future turns.
		});
		this.pendingWorklogFork.set(agentId, next);
	}

	private addPendingSpawnDraft(parentId: string, draft: PendingSpawnDraft): void {
		let drafts = this.pendingSpawnDrafts.get(parentId);
		if (!drafts) {
			drafts = new Map();
			this.pendingSpawnDrafts.set(parentId, drafts);
		}
		drafts.set(draft.id, draft);
	}

	private removePendingSpawnDraft(parentId: string, agentId: string): void {
		const drafts = this.pendingSpawnDrafts.get(parentId);
		if (!drafts) {
			return;
		}
		drafts.delete(agentId);
		if (drafts.size === 0) {
			this.pendingSpawnDrafts.delete(parentId);
		}
	}

	private async serializeRecentAncestorContext(ancestor: {
		id: string;
		role: string;
		lastWorklogMessageCount: number;
		messages: AgentMessage[];
	}): Promise<string | undefined> {
		const startIndex = Math.min(ancestor.lastWorklogMessageCount, ancestor.messages.length);
		const recentMessages = ancestor.messages.slice(startIndex);
		if (recentMessages.length === 0) {
			return undefined;
		}

		const record = this.getRecord(ancestor.id);
		const llmMessages = await record.session.agent.convertToLlm(recentMessages);
		const serialized = serializeConversation(llmMessages).trim();
		if (!serialized) {
			return undefined;
		}

		const maxChars = 4_000;
		if (serialized.length <= maxChars) {
			return serialized;
		}
		return `[Truncated to the most recent ${maxChars} characters of ancestor context]\n${serialized.slice(-maxChars)}`;
	}

	private buildSiblingBatchPrefix(parentId: string, agentId: string): string | undefined {
		const siblings = new Map<string, { role: string; prompt: string; status: string }>();
		const parent = this.records.get(parentId);
		for (const childId of parent?.childIds ?? []) {
			if (childId === agentId) {
				continue;
			}
			const sibling = this.records.get(childId);
			if (!sibling || sibling.status === "disposed") {
				continue;
			}
			siblings.set(childId, {
				role: sibling.role,
				prompt: sibling.config.prompt,
				status: sibling.status,
			});
		}

		for (const draft of this.pendingSpawnDrafts.get(parentId)?.values() ?? []) {
			if (draft.id === agentId || siblings.has(draft.id)) {
				continue;
			}
			siblings.set(draft.id, {
				role: draft.role,
				prompt: draft.prompt,
				status: "spawning",
			});
		}

		if (siblings.size === 0) {
			return undefined;
		}

		const lines = [
			`<parent-sibling-batch parent="${parentId}">`,
			"Other direct children of your parent are already active or being spawned now. Coordinate through your parent and avoid duplicating their work:",
		];
		for (const [siblingId, sibling] of siblings) {
			lines.push(`- ${siblingId} (${sibling.status}): ${sibling.role} — ${sibling.prompt}`);
		}
		lines.push("</parent-sibling-batch>");
		return lines.join("\n");
	}

	private async runWorklogFork(agentId: string, turn: number, turnMessages: AgentMessage[]): Promise<void> {
		const record = this.records.get(agentId);
		if (!record || record.status === "disposed") {
			return;
		}
		// Resolve the fork model and reasoning level. A configured
		// `forkModel` lets operators route worklog forks to a cheaper model
		// (e.g. GPT-5.4 medium) than the parent session's main-loop model;
		// absent that, fall back to the parent's model and thinking level so
		// behavior matches pre-Phase-1.
		const forkModel = this.config.forkModel ?? record.session.model;
		if (!forkModel) {
			return;
		}
		const forkThinkingLevel = this.config.forkThinkingLevel ?? record.session.thinkingLevel;

		const transformed = record.session.agent.transformContext
			? await record.session.agent.transformContext(turnMessages)
			: turnMessages;
		// Phase 1 (delta-only fork input): only send messages that have arrived
		// since the last worklog entry. The `<last-worklog-entry>` block inside
		// `buildWorklogPrompt` already carries prior semantic state, so the fork
		// does not need the full transcript. On first-ever fork
		// (`lastWorklogMessageCount === 0`) this falls through to sending
		// everything, which is the intended bootstrap behavior.
		//
		// Additionally, drop everything except `user` and `assistant` messages
		// from the fork input. Tool-result payloads (often many KB each) and
		// custom orchestrator messages (agent_roster, agent_directive,
		// agent_report, agent_idle, background tool completion notifications)
		// are the bulk of per-turn bytes but add little to the fork's "is there
		// anything durable to record?" decision — user prompts and assistant
		// text/thinking/toolCall content are sufficient signal.
		const deltaStart = Math.min(record.lastWorklogMessageCount, transformed.length);
		const deltaMessages = transformed
			.slice(deltaStart)
			.filter((message) => message.role === "user" || message.role === "assistant");
		const contextMessages = await record.session.agent.convertToLlm(deltaMessages);
		const worklogContents = await readWorklog(record.worklogFile);
		const lastEntry = getLastWorklogEntry(worklogContents);
		// Hint the fork at slugs already in use so topic choices stay stable
		// across entries. Capped at top-30 by count inside computeTopicVocabulary.
		const topicVocabulary = computeTopicVocabulary(parseWorklogEntries(worklogContents));
		const prompt: UserMessage = {
			role: "user",
			content: [{ type: "text", text: buildWorklogPrompt(lastEntry, topicVocabulary) }],
			timestamp: Date.now(),
		};
		// Use a distinct `sessionId` for the worklog fork so OpenAI-family
		// providers emit a separate `prompt_cache_key` for fork calls. This
		// keeps main-loop and fork caches from cross-contaminating: fork
		// turns never evict cache entries the main loop is relying on, and
		// vice versa.
		const forkSessionId = record.session.agent.sessionId
			? `${record.session.agent.sessionId}:worklog`
			: undefined;
		const streamOptions = {
			reasoning: forkThinkingLevel === "off" ? undefined : forkThinkingLevel,
			getApiKey: record.session.agent.getApiKey,
			onPayload: record.session.agent.onPayload,
			sessionId: forkSessionId,
			thinkingBudgets: record.session.agent.thinkingBudgets,
			transport: record.session.agent.transport,
			maxRetryDelayMs: record.session.agent.maxRetryDelayMs,
		} as Parameters<typeof record.session.agent.streamFn>[2];
		const stream = await record.session.agent.streamFn(
			forkModel,
			{
				systemPrompt: record.session.agent.state.systemPrompt,
				messages: [...contextMessages, prompt],
				tools: [WORKLOG_UPDATE_TOOL],
			},
			streamOptions,
		);
		const assistant = await stream.result();
		// The worklog fork is its own model turn, paid for out-of-band from the
		// child's main transcript. Attribute those tokens to the child session
		// so they flow through getSessionStats, the TUI footer, and subtree
		// aggregation. Record the usage before any early-return so even turns
		// that didn't produce a worklog tool call still count.
		if (assistant.usage) {
			record.session.addBackgroundUsage(assistant.usage, "worklog");
		}
		if (assistant.stopReason !== "toolUse") {
			logWorklogFork({
				agentId,
				turn,
				deltaMsgCount: deltaMessages.length,
				skipped: false,
				entryEmitted: false,
			});
			return;
		}

		const toolCall = assistant.content.find(
			(content): content is AgentToolCall => content.type === "toolCall" && content.name === WORKLOG_UPDATE_TOOL.name,
		);
		if (!toolCall) {
			logWorklogFork({
				agentId,
				turn,
				deltaMsgCount: deltaMessages.length,
				skipped: false,
				entryEmitted: false,
			});
			return;
		}

		const args = validateToolArguments(WORKLOG_UPDATE_TOOL, toolCall);
		const entry = await appendWorklogEntry(record.worklogFile, args.content, turn, {
			topics: args.topics,
			supersedes: args.supersedes,
			pin: args.pin,
		});
		record.lastWorklogTurn = turn;
		record.lastWorklogMessageCount = turnMessages.length;
		await this.persistTree();
		logWorklogFork({
			agentId,
			turn,
			deltaMsgCount: deltaMessages.length,
			skipped: false,
			entryEmitted: true,
		});
	}

	private appendInterruptedToolResults(session: AgentSessionHandle): string[] {
		const assistantToolCalls = new Map<string, { toolName: string; pendingMessage?: ToolResultMessage }>();
		const completedBackgroundToolCalls = new Set<string>();
		for (const message of session.agent.state.messages) {
			if (isBackgroundToolCompletionMessage(message)) {
				completedBackgroundToolCalls.add(message.details.toolCallId);
				continue;
			}

			if (message.role === "assistant") {
				for (const content of message.content) {
					if (content.type !== "toolCall") {
						continue;
					}
					assistantToolCalls.set(content.id, { toolName: content.name });
				}
				continue;
			}

			if (message.role !== "toolResult") {
				continue;
			}

			const existing = assistantToolCalls.get(message.toolCallId);
			if (!existing) {
				continue;
			}
			if (isPendingToolResult(message)) {
				existing.pendingMessage = message;
				continue;
			}
			assistantToolCalls.delete(message.toolCallId);
		}

		const orphanedPendingToolCallIds: string[] = [];
		for (const [toolCallId, info] of assistantToolCalls) {
			if (completedBackgroundToolCalls.has(toolCallId)) {
				continue;
			}

			if (info.pendingMessage) {
				orphanedPendingToolCallIds.push(toolCallId);
				continue;
			}

			const text = `[INTERRUPTED] ${info.toolName} did not produce a result before the session ended. It may still be running if the process was killed abruptly. Inspect or re-run it if you still need the result.`;
			const terminatedResult: ToolResultMessage = {
				role: "toolResult",
				toolCallId,
				toolName: info.toolName,
				content: [{ type: "text", text }],
				isError: true,
				timestamp: Date.now(),
			};
			session.sessionManager.appendMessage(terminatedResult as AgentMessage);
			session.agent.state.messages.push(terminatedResult as AgentMessage);
		}

		return orphanedPendingToolCallIds;
	}
}
