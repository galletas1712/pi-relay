import { existsSync, mkdirSync, readFileSync, statSync, writeFileSync, renameSync } from "node:fs";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";
import type { AgentMessage, AgentToolCall } from "@mariozechner/pi-agent-core";
import { isBackgroundToolCompletionMessage, isPendingToolResult } from "@mariozechner/pi-agent-core";
import { validateToolArguments, type ToolResultMessage, type UserMessage } from "@mariozechner/pi-ai";
import type { AgentSessionEvent, ToolDefinition } from "@mariozechner/pi-coding-agent";
import { createAgentContextTransform } from "./context-transform.js";
import { createMessageTool } from "./tools/message.js";
import { createReportTool } from "./tools/report.js";
import { createSpawnTool } from "./tools/spawn.js";
import {
	createAgentDirectiveMessage,
	createAgentIdleMessage,
	createAgentReportMessage,
	createAgentWorklogMessage,
} from "./messages.js";
import { buildAncestorWorklogPrefix, buildWorklogPrompt, appendWorklogEntry, getLastWorklogEntry, readWorklog, WORKLOG_UPDATE_TOOL } from "./worklog.js";
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
	type SpawnConfig,
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
	private readonly pendingWorklogFork = new Map<string, Promise<void>>();
	private readonly restoredDisposedEntries = new Map<string, AgentTreeMetadataEntry>();
	private _isDisposing = false;
	private treeWriteChain: Promise<void> = Promise.resolve();
	private pendingRootResumeSessionId?: string;

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

	consumePendingRootResume(sessionId: string): boolean {
		if (this.pendingRootResumeSessionId !== sessionId) {
			return false;
		}
		this.pendingRootResumeSessionId = undefined;
		return true;
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
				record.session.getLastAssistantText(),
				undefined,
				"Session restored from interrupted state.",
			);
			if (record.parentId === this.rootAgentId) {
				await this.getRecord(this.rootAgentId).session.sendCustomMessage(idleMessage);
				continue;
			}
			await this.deliverMessage(record.parentId, idleMessage);
		}

		this.pendingRootResumeSessionId = rootRecord.session.sessionId;
		await this.persistTree();
		return true;
	}

	async spawnAgent(parentId: string, config: SpawnConfig): Promise<string> {
		const parent = this.getRecord(parentId);
		this.assertSpawnAllowed(parentId);
		await this.awaitAncestorWorklogs(parentId);

		const agentId = createAgentId(config.role);
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
			turnCount: 0,
			pendingRestoreIdleNotice: false,
			orphanedPendingToolCallIds: [],
		};
		parent.childIds.push(agentId);
		this.registerRecord(record);
		const prompt = await this.buildSpawnPrompt(parentId, config.prompt);
		void created.session.prompt(prompt).catch((error) => {
			void this.handleAgentError(agentId, error);
		});

		return agentId;
	}

	async routeMessage(fromAgentId: string, targetAgentId: string, content: string): Promise<void> {
		const source = this.getRecord(fromAgentId);
		if (!source.childIds.includes(targetAgentId)) {
			throw new Error(`Agent ${targetAgentId} is not a direct child of ${fromAgentId}`);
		}

		await this.deliverMessage(
			targetAgentId,
			createAgentDirectiveMessage(fromAgentId, source.role, content),
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
		if (parent.childIds.length >= this.config.maxChildren) {
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

		const activeAgents = [...this.records.values()].filter((record) => record.status !== "disposed").length;
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
			void record.session.agent
				.waitForIdle()
				.then(() => this.finalizeIdle(agentId))
				.catch(() => {
					// Ignore shutdown races and let explicit error/dispose paths own cleanup.
				});
		}
	}

	private async finalizeIdle(agentId: string): Promise<void> {
		const record = this.records.get(agentId);
		if (!record || record.status === "disposed") {
			return;
		}

		if (record.session.isStreaming || record.session.isRetrying || record.session.isCompacting) {
			return;
		}

		this.setStatus(agentId, "idle");

		if (!record.parentId) {
			return;
		}

		await this.deliverMessage(
			record.parentId,
			createAgentIdleMessage(agentId, record.role, record.session.getLastAssistantText()),
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
			createAgentIdleMessage(agentId, record.role, record.session.getLastAssistantText(), errorMessage),
		);
	}

	private async deliverMessage(targetAgentId: string, message: SessionCustomMessage): Promise<void> {
		const target = this.getRecord(targetAgentId);
		if (target.status === "disposed") {
			return;
		}

		const targetIsBusy = target.session.isStreaming || target.session.isRetrying || target.session.isCompacting;
		if (!targetIsBusy && !target.reactivating) {
			target.reactivating = true;
			this.setStatus(targetAgentId, "running");
			try {
				await target.session.sendCustomMessage(message, { triggerTurn: true });
			} catch (error) {
				await this.handleAgentError(targetAgentId, error);
			} finally {
				target.reactivating = false;
			}
			return;
		}

		try {
			await target.session.sendCustomMessage(message, { deliverAs: "steer" });
		} catch (error) {
			await this.handleAgentError(targetAgentId, error);
		}
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
		record.session.agent.mailbox.close();
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
		void this.persistTree();
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

	private async awaitAncestorWorklogs(parentId: string): Promise<void> {
		const ancestorIds: string[] = [];
		let current: AgentRecord | undefined = this.records.get(parentId);
		while (current) {
			ancestorIds.push(current.id);
			current = current.parentId ? this.records.get(current.parentId) : undefined;
		}

		await Promise.all(
			ancestorIds.map(async (agentId) => {
				try {
					await this.pendingWorklogFork.get(agentId);
				} catch {
					// Ignore best-effort worklog failures when spawning children.
				}
			}),
		);
	}

	private async buildSpawnPrompt(parentId: string, prompt: string): Promise<string> {
		const ancestors: Array<{ agentId: string; role: string; filePath: string }> = [];
		let current: AgentRecord | undefined = this.records.get(parentId);
		while (current) {
			ancestors.unshift({
				agentId: current.id,
				role: current.role,
				filePath: current.worklogFile,
			});
			current = current.parentId ? this.records.get(current.parentId) : undefined;
		}

		const prefix = await buildAncestorWorklogPrefix(ancestors);
		if (!prefix) {
			return prompt;
		}
		return `${prefix}\n\n${prompt}`;
	}

	private scheduleWorklogFork(agentId: string, turn: number, turnMessages: AgentMessage[]): void {
		const previous = this.pendingWorklogFork.get(agentId) ?? Promise.resolve();
		const next = previous.then(() => this.runWorklogFork(agentId, turn, turnMessages)).catch(() => {
			// Best-effort worklog generation should not poison future turns.
		});
		this.pendingWorklogFork.set(agentId, next);
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
		const streamOptions = {
			reasoning: record.session.thinkingLevel === "off" ? undefined : record.session.thinkingLevel,
			getApiKey: record.session.agent.getApiKey,
			onPayload: record.session.agent.onPayload,
			sessionId: record.session.agent.sessionId,
			thinkingBudgets: record.session.agent.thinkingBudgets,
			transport: record.session.agent.transport,
			maxRetryDelayMs: record.session.agent.maxRetryDelayMs,
		} as Parameters<typeof record.session.agent.streamFn>[2];
		const stream = await record.session.agent.streamFn(
			record.session.model,
			{
				systemPrompt: record.session.agent.state.systemPrompt,
				messages: [...contextMessages, prompt],
				tools: [WORKLOG_UPDATE_TOOL],
			},
			streamOptions,
		);
		const assistant = await stream.result();
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
		record.lastWorklogTurn = turn;
		await this.persistTree();

		if (!record.parentId) {
			return;
		}

		await this.deliverMessage(
			record.parentId,
			createAgentWorklogMessage(record.id, record.role, entry, record.worklogFile, turn),
		);
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

			const text = `[TERMINATED] ${info.toolName} did not produce a result before the session was interrupted. Re-run it if you still need the result.`;
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
