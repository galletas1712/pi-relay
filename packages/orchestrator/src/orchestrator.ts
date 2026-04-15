import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync, renameSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { randomUUID } from "node:crypto";
import type { AgentMessage, AgentToolCall } from "@mariozechner/pi-agent-core";
import { isBackgroundToolCompletionMessage, isPendingToolResult } from "@mariozechner/pi-agent-core";
import { validateToolArguments, type ToolResultMessage, type UserMessage } from "@mariozechner/pi-ai";
import {
	DEFAULT_COMPACTION_SETTINGS,
	serializeConversation,
	type AgentSessionEvent,
	type ToolDefinition,
} from "@mariozechner/pi-coding-agent";
import { createAgentContextTransform } from "./context-transform.js";
import { buildDirectChildRoster } from "./roster.js";
import { createChildrenTool } from "./tools/children.js";
import { createMessageTool } from "./tools/message.js";
import { createReportTool } from "./tools/report.js";
import { createSpawnTool } from "./tools/spawn.js";

import {
	createAgentDirectiveMessage,
	createAgentIdleMessage,
	createAgentInterruptedMessage,
	createAgentReportMessage,
} from "./messages.js";
import {
	appendWorklogEntry,
	appendWorklogSection,
	buildAncestorWorklogPrefix,
	buildWorklogCompactionPrompt,
	buildWorklogPrompt,
	formatWorklogSummary,
	getLastWorklogEntry,
	getWorklogEntries,
	getWorklogTurn,
	parseCompactedWorklog,
	readWorklog,
	renderCompactedWorklog,
	selectWorklogEntriesToKeep,
	shouldCompactText,
	shouldCompactWorklog,
	WORKLOG_COMPACTION_SYSTEM_PROMPT,
	WORKLOG_COMPACTION_TOOL,
	WORKLOG_UPDATE_TOOL,
} from "./worklog.js";
import { ToolCallTracker } from "./tool-tracker.js";
import {
	DEFAULT_ORCHESTRATOR_CONFIG,
	type AgentRecord,
	type AgentSummary,
	type AgentTreeMetadata,
	type AgentTreeMetadataEntry,
	type AgentSessionFactory,
	type AgentSessionHandle,
	type OrchestratorConfig,
	type OrchestratorOptions,
	type SessionCustomMessage,
	type SpawnConfig,
} from "./types.js";

const WORKLOG_LLM_TIMEOUT_MS = 60_000;

function withTimeout<T>(value: T | PromiseLike<T>, ms: number): Promise<Awaited<T>> {
	let timer: ReturnType<typeof setTimeout>;
	const timeout = new Promise<never>((_, reject) => {
		timer = setTimeout(() => reject(new Error(`Worklog LLM call timed out after ${ms}ms`)), ms);
	});
	return Promise.race([Promise.resolve(value), timeout]).finally(() => clearTimeout(timer));
}

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

interface SpawnAncestorContext {
	id: string;
	role: string;
	worklogFile: string;
	compactedWorklogFile: string;
	lastWorklogMessageCount: number;
	messages: AgentMessage[];
	recentContext?: string;
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
	private readonly pendingFinalization = new Set<string>();
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

	getChildrenOf(agentId: string): AgentRecord[] {
		return this.getRecord(agentId).childIds
			.map((childId) => this.records.get(childId))
			.filter((record): record is AgentRecord => record !== undefined && record.status !== "disposed");
	}

	getAgentSummaries(): AgentSummary[] {
		const summaries: AgentSummary[] = [];
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
				displayStatus: this.getDisplayStatus(record),
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

	getDirectChildSummaries(agentId: string): AgentSummary[] {
		return this.getRecord(agentId).childIds
			.map((childId) => this.records.get(childId))
			.filter((record): record is AgentRecord => record !== undefined && record.status !== "disposed")
			.map((record) => ({
				id: record.id,
				parentId: record.parentId,
				role: record.role,
				status: record.status,
				displayStatus: this.getDisplayStatus(record),
				depth: 1,
				childCount: record.childIds.length,
				sessionFile: record.session.sessionFile,
				lastOutput: record.session.getLastAssistantText(),
			}));
	}

	private getDisplayStatus(record: AgentRecord): AgentSummary["displayStatus"] {
		const sessionBusy =
			record.reactivating ||
			record.session.isStreaming ||
			record.session.isRetrying ||
			record.session.isCompacting ||
			record.session.agent.hasQueuedMessages();
		if (sessionBusy) {
			return "running";
		}

		if (this.hasRunningChildren(record.id)) {
			return "waiting";
		}

		if (record.status === "running" && record.turnCount === 0) {
			return "starting";
		}

		return "idle";
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

			try {
				const prompt = await this.buildSpawnPrompt(parentId, agentId, config.prompt);
				await this.startSpawnedAgentPrompt(record, prompt);
				return agentId;
			} catch (error) {
				await this.disposeAgent(agentId).catch(() => {
					// Best-effort cleanup after a failed spawn startup.
				});
				throw error;
			}
		} finally {
			this.removePendingSpawnDraft(parentId, agentId);
		}
	}

	private async startSpawnedAgentPrompt(record: AgentRecord, prompt: string): Promise<void> {
		let settled = false;
		let resolveStarted = () => {};
		let rejectStarted = (_error: unknown) => {};
		const started = new Promise<void>((resolve, reject) => {
			resolveStarted = resolve;
			rejectStarted = reject;
		});

		const unsubscribe = record.session.subscribe((event) => {
			if (settled || event.type !== "agent_start") {
				return;
			}
			settled = true;
			unsubscribe();
			resolveStarted();
		});

		void record.session.prompt(prompt)
			.then(() => {
				if (settled) {
					return;
				}
				settled = true;
				unsubscribe();
				resolveStarted();
			})
			.catch((error) => {
				if (settled) {
					void this.handleAgentError(record.id, error);
					return;
				}
				settled = true;
				unsubscribe();
				rejectStarted(error);
			});

		await started;
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

	async terminateAgent(fromAgentId: string, targetAgentId: string): Promise<void> {
		const source = this.getRecord(fromAgentId);
		if (!source.childIds.includes(targetAgentId)) {
			throw new Error(`Agent ${targetAgentId} is not a direct child of ${fromAgentId}`);
		}

		await this.disposeAgent(targetAgentId);
	}

	async describeChildren(agentId: string): Promise<string> {
		return buildDirectChildRoster(this, agentId);
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
			this.scheduleWorklogFork(agentId, record.turnCount, [...record.session.agent.state.messages]);
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
			record.session.isCompacting
		) {
			return;
		}

		// Drain stranded mailbox messages instead of silently bailing
		if (record.session.agent.hasQueuedMessages()) {
			if (!record.reactivating) {
				record.reactivating = true;
				this.setStatus(agentId, "running");
				record.session.agent.continue()
					.catch(async (error) => {
						await this.handleAgentError(agentId, error);
					})
					.finally(() => {
						record.reactivating = false;
					});
			}
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

		const wasInterrupted = this.wasInterruptedByUser(record);
		const message = wasInterrupted
			? createAgentInterruptedMessage(agentId, record.role)
			: createAgentIdleMessage(agentId, record.role);
		await this.deliverIdleMessage(record.parentId, agentId, message);
	}

	private wasInterruptedByUser(record: AgentRecord): boolean {
		const messages = record.session.agent.state.messages;
		for (let i = messages.length - 1; i >= 0; i--) {
			const msg = messages[i];
			if (msg && "role" in msg && msg.role === "assistant") {
				return (msg as { stopReason?: string }).stopReason === "aborted";
			}
		}
		return false;
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
		if (this.pendingFinalization.has(agentId)) {
			return;
		}
		this.pendingFinalization.add(agentId);
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
			})
			.finally(() => {
				this.pendingFinalization.delete(agentId);
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
				await target.session.sendCustomMessage(message, { deliverAs: "nextTurn" });
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
		// Unsubscribe before abort so the agent_end event doesn't trigger finalizeIdle
		record.unsubscribe?.();
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
		record.session.agent.mailbox.close();
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
		return [
			createSpawnTool(this, agentId),
			createChildrenTool(this, agentId),
			createMessageTool(this, agentId),
			createReportTool(this, agentId),
		];
	}

	private getWorklogFile(agentId: string): string {
		const filePath = join(this.worklogDir, `${agentId}.worklog.md`);
		ensureDir(dirname(filePath));
		return filePath;
	}

	private getCompactedWorklogFile(agentId: string): string {
		const filePath = join(this.worklogDir, `${agentId}.compacted.worklog.md`);
		ensureDir(dirname(filePath));
		return filePath;
	}

	private writeWorklogFile(filePath: string, content: string): void {
		const tempFile = `${filePath}.tmp`;
		writeFileSync(tempFile, content.trim() ? `${content.trim()}\n` : "", "utf-8");
		renameSync(tempFile, filePath);
	}

	private buildStreamOptions(record: AgentRecord): Parameters<AgentRecord["session"]["agent"]["streamFn"]>[2] {
		return {
			reasoning: record.session.thinkingLevel === "off" ? undefined : record.session.thinkingLevel,
			getApiKey: record.session.agent.getApiKey,
			onPayload: record.session.agent.onPayload,
			sessionId: record.session.agent.sessionId,
			thinkingBudgets: record.session.agent.thinkingBudgets,
			transport: record.session.agent.transport,
			maxRetryDelayMs: record.session.agent.maxRetryDelayMs,
		} as Parameters<AgentRecord["session"]["agent"]["streamFn"]>[2];
	}

	private async ensureCompactedWorklog(record: AgentRecord): Promise<void> {
		const rawContents = await readWorklog(record.worklogFile);
		if (!rawContents.trim()) {
			return;
		}
		const summaryTurn = Math.max(record.turnCount, record.lastWorklogTurn);

		const compactedFile = this.getCompactedWorklogFile(record.id);
		if (
			!existsSync(compactedFile) ||
			(existsSync(record.worklogFile) && statSync(record.worklogFile).mtimeMs > statSync(compactedFile).mtimeMs)
		) {
			await this.rebuildCompactedWorklog(record, rawContents);
			return;
		}

		const compactedContents = await readWorklog(compactedFile);
		const parsed = parseCompactedWorklog(compactedContents);
		const compacted = await this.compactWorklogStateUntilFit(record, parsed.summary, parsed.entries, summaryTurn);
		const nextContents = renderCompactedWorklog(
			compacted.summary ? formatWorklogSummary(compacted.summary, summaryTurn) : undefined,
			compacted.entries,
		);
		if (nextContents.trim() !== compactedContents.trim()) {
			this.writeWorklogFile(compactedFile, nextContents);
		}
	}

	private async compactWorklogFile(record: AgentRecord, turn: number): Promise<void> {
		const compactedFile = this.getCompactedWorklogFile(record.id);
		const compactedContents = await readWorklog(compactedFile);
		if (!compactedContents.trim()) {
			return;
		}

		const parsed = parseCompactedWorklog(compactedContents);
		const compacted = await this.compactWorklogStateUntilFit(record, parsed.summary, parsed.entries, turn);
		const nextContents = renderCompactedWorklog(
			compacted.summary ? formatWorklogSummary(compacted.summary, turn) : undefined,
			compacted.entries,
		);
		if (nextContents.trim() !== compactedContents.trim()) {
			this.writeWorklogFile(compactedFile, nextContents);
		}
	}

	private async rebuildCompactedWorklog(record: AgentRecord, rawContents: string): Promise<void> {
		const entries = getWorklogEntries(rawContents);
		const compactedFile = this.getCompactedWorklogFile(record.id);
		if (entries.length === 0) {
			this.writeWorklogFile(compactedFile, "");
			return;
		}

		let summary: string | undefined;
		let keptEntries: string[] = [];
		for (const entry of entries) {
			keptEntries.push(entry);
			const turn = getWorklogTurn(entry) ?? record.turnCount;
			const compacted = await this.compactWorklogStateUntilFit(record, summary, keptEntries, turn);
			summary = compacted.summary;
			keptEntries = compacted.entries;
		}

		this.writeWorklogFile(
			compactedFile,
			renderCompactedWorklog(
				summary ? formatWorklogSummary(summary, Math.max(record.turnCount, record.lastWorklogTurn)) : undefined,
				keptEntries,
			),
		);
	}

	private async compactWorklogStateUntilFit(
		record: AgentRecord,
		summary: string | undefined,
		entries: string[],
		turn: number,
	): Promise<{ summary?: string; entries: string[] }> {
		if (!record.session.model) {
			return { summary, entries };
		}

		let nextSummary = summary;
		let nextEntries = [...entries];
		for (let attempt = 0; attempt <= nextEntries.length; attempt += 1) {
			const rendered = renderCompactedWorklog(
				nextSummary ? formatWorklogSummary(nextSummary, turn) : undefined,
				nextEntries,
			);
			if (!shouldCompactWorklog(rendered, record.session.model.contextWindow)) {
				return { summary: nextSummary, entries: nextEntries };
			}

			const { compactedEntries, keptEntries } = selectWorklogEntriesToKeep(
				nextEntries,
				DEFAULT_COMPACTION_SETTINGS.keepRecentTokens,
			);
			if (compactedEntries.length === 0) {
				return { summary: nextSummary, entries: nextEntries };
			}

			const mergedSummary = await this.generateWorklogCompactionSummary(record, nextSummary, compactedEntries);
			if (!mergedSummary) {
				return { summary: nextSummary, entries: nextEntries };
			}

			nextSummary = mergedSummary;
			nextEntries = keptEntries;
		}

		return { summary: nextSummary, entries: nextEntries };
	}

	private async generateWorklogCompactionSummary(
		record: AgentRecord,
		previousSummary: string | undefined,
		compactedEntries: string[],
		recentContext?: string,
	): Promise<string | undefined> {
		if (!record.session.model) {
			return undefined;
		}

		const prompt: UserMessage = {
			role: "user",
			content: [{ type: "text", text: buildWorklogCompactionPrompt(previousSummary, compactedEntries, recentContext) }],
			timestamp: Date.now(),
		};
		const stream = await withTimeout(
			record.session.agent.streamFn(
				record.session.model,
				{
					systemPrompt: WORKLOG_COMPACTION_SYSTEM_PROMPT,
					messages: [prompt],
					tools: [WORKLOG_COMPACTION_TOOL],
				},
				this.buildStreamOptions(record),
			),
			WORKLOG_LLM_TIMEOUT_MS,
		);
		const assistant = await withTimeout(stream.result(), WORKLOG_LLM_TIMEOUT_MS);
		if (assistant.stopReason !== "toolUse") {
			return undefined;
		}

		const toolCall = assistant.content.find(
			(content): content is AgentToolCall =>
				content.type === "toolCall" && content.name === WORKLOG_COMPACTION_TOOL.name,
		);
		if (!toolCall) {
			return undefined;
		}

		const args = validateToolArguments(WORKLOG_COMPACTION_TOOL, toolCall);
		return args.summary.trim() || undefined;
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

	private async compactAncestorRecentContext(ancestor: SpawnAncestorContext): Promise<boolean> {
		if (!ancestor.recentContext) {
			return false;
		}

		const record = this.getRecord(ancestor.id);
		let compactedContents = await readWorklog(ancestor.compactedWorklogFile);
		if (!compactedContents.trim()) {
			compactedContents = await readWorklog(ancestor.worklogFile);
		}

		const parsed = parseCompactedWorklog(compactedContents);
		const mergedSummary = await this.generateWorklogCompactionSummary(
			record,
			parsed.summary,
			parsed.entries,
			ancestor.recentContext,
		);
		if (!mergedSummary) {
			return false;
		}

		const summaryTurn = Math.max(record.turnCount, record.lastWorklogTurn);
		this.writeWorklogFile(
			ancestor.compactedWorklogFile,
			renderCompactedWorklog(formatWorklogSummary(mergedSummary, summaryTurn), []),
		);
		if (record.lastWorklogMessageCount < ancestor.messages.length) {
			record.lastWorklogMessageCount = ancestor.messages.length;
			await this.persistTree();
		}
		return true;
	}

	private async renderSpawnPrompt(
		parentId: string,
		agentId: string,
		prompt: string,
		ancestors: SpawnAncestorContext[],
	): Promise<string> {
		const sections: string[] = [];
		for (const ancestor of ancestors) {
			const worklogSection = await buildAncestorWorklogPrefix([
				{
					agentId: ancestor.id,
					role: ancestor.role,
					filePath: ancestor.compactedWorklogFile,
					fallbackFilePath: ancestor.worklogFile,
				},
			]);
			if (worklogSection) {
				sections.push(worklogSection);
			}

			if (ancestor.recentContext) {
				sections.push(
					`<ancestor-recent-context agent="${ancestor.id}" role="${ancestor.role}">\n${ancestor.recentContext}\n</ancestor-recent-context>`,
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

	private async buildSpawnPrompt(parentId: string, agentId: string, prompt: string): Promise<string> {
		const ancestors: SpawnAncestorContext[] = [];
		let current: AgentRecord | undefined = this.records.get(parentId);
		while (current) {
			await this.ensureCompactedWorklog(current);
			const messages = [...current.session.agent.state.messages];
			const ancestor: SpawnAncestorContext = {
				id: current.id,
				role: current.role,
				worklogFile: current.worklogFile,
				compactedWorklogFile: this.getCompactedWorklogFile(current.id),
				lastWorklogMessageCount: current.lastWorklogMessageCount,
				messages,
			};
			ancestor.recentContext = await this.serializeRecentAncestorContext(ancestor);
			ancestors.unshift({
				...ancestor,
			});
			current = current.parentId ? this.records.get(current.parentId) : undefined;
		}

		let spawnPrompt = await this.renderSpawnPrompt(parentId, agentId, prompt, ancestors);
		const child = this.getRecord(agentId);
		if (!child.session.model) {
			return spawnPrompt;
		}

		for (const ancestor of ancestors) {
			if (!shouldCompactText(spawnPrompt, child.session.model.contextWindow)) {
				break;
			}
			if (!ancestor.recentContext) {
				continue;
			}
			if (!(await this.compactAncestorRecentContext(ancestor))) {
				continue;
			}
			ancestor.recentContext = undefined;
			ancestor.lastWorklogMessageCount = ancestor.messages.length;
			spawnPrompt = await this.renderSpawnPrompt(parentId, agentId, prompt, ancestors);
		}

		return spawnPrompt;
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
		return serialized;
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
		if (!record || record.status === "disposed" || !record.session.model) {
			return;
		}

		const transformed = record.session.agent.transformContext
			? await record.session.agent.transformContext(turnMessages)
			: turnMessages;
		const contextMessages = await record.session.agent.convertToLlm(transformed);
		const worklogContents = await readWorklog(record.worklogFile);
		const lastEntry = getLastWorklogEntry(worklogContents);
		const prompt: UserMessage = {
			role: "user",
			content: [{ type: "text", text: buildWorklogPrompt(lastEntry) }],
			timestamp: Date.now(),
		};
		const stream = await withTimeout(
			record.session.agent.streamFn(
				record.session.model,
				{
					systemPrompt: record.session.agent.state.systemPrompt,
					messages: [...contextMessages, prompt],
					tools: [WORKLOG_UPDATE_TOOL],
				},
				this.buildStreamOptions(record),
			),
			WORKLOG_LLM_TIMEOUT_MS,
		);
		const assistant = await withTimeout(stream.result(), WORKLOG_LLM_TIMEOUT_MS);
		if (assistant.stopReason !== "toolUse") {
			return;
		}

		const toolCall = assistant.content.find(
			(content): content is AgentToolCall => content.type === "toolCall" && content.name === WORKLOG_UPDATE_TOOL.name,
		);
		if (!toolCall) {
			return;
		}

		const args = validateToolArguments(WORKLOG_UPDATE_TOOL, toolCall);
		const entry = await appendWorklogEntry(record.worklogFile, args.content, turn);
		const compactedWorklogFile = this.getCompactedWorklogFile(record.id);
		if (existsSync(compactedWorklogFile)) {
			await appendWorklogSection(compactedWorklogFile, entry);
		} else {
			const existingEntryCount = getWorklogEntries(worklogContents).length;
			if (existingEntryCount === 0) {
				this.writeWorklogFile(compactedWorklogFile, entry);
			} else {
				await this.rebuildCompactedWorklog(record, `${worklogContents.trim()}\n\n${entry}`);
			}
		}
		record.lastWorklogTurn = turn;
		record.lastWorklogMessageCount = turnMessages.length;
		await this.persistTree();
		await this.compactWorklogFile(record, turn).catch(() => {
			// A failed compacted-worklog update should not discard the raw entry.
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
