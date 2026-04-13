import { mkdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { randomUUID } from "node:crypto";
import type { AgentSessionEvent, ToolDefinition } from "@mariozechner/pi-coding-agent";
import { createMessageTool } from "./tools/message.js";
import { createReportTool } from "./tools/report.js";
import { createSpawnTool } from "./tools/spawn.js";
import { createAgentDirectiveMessage, createAgentIdleMessage, createAgentReportMessage } from "./messages.js";
import { ToolCallTracker } from "./tool-tracker.js";
import {
	DEFAULT_ORCHESTRATOR_CONFIG,
	type AgentRecord,
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
	private readonly worklogDir: string;
	private _isDisposing = false;

	readonly rootAgentId: string;

	constructor(options: OrchestratorOptions) {
		this.config = { ...DEFAULT_ORCHESTRATOR_CONFIG, ...options.config };
		this.sessionFactory = options.sessionFactory;
		this.rootAgentId = options.rootAgentId ?? "root";
		this.workspaceDir =
			options.workspaceDir ??
			join(options.rootSession.sessionManager.getSessionDir(), options.rootSession.sessionId);
		this.worklogDir = join(this.workspaceDir, "worklogs");

		ensureDir(this.workspaceDir);
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
		};
		this.registerRecord(rootRecord);
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

	async spawnAgent(parentId: string, config: SpawnConfig): Promise<string> {
		const parent = this.getRecord(parentId);
		this.assertSpawnAllowed(parentId);

		const agentId = createAgentId(config.role);
		const childCustomTools = this.createChildTools(agentId);
		const created = await this.sessionFactory({
			mode: "spawn",
			agentId,
			parentId,
			config,
			customTools: childCustomTools,
			parentSession: parent.session,
			sessionDir: join(this.workspaceDir, "agents"),
		});

		await created.session.bindExtensions({});

		const record: AgentRecord = {
			id: agentId,
			session: created.session,
			status: "created",
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
		};
		parent.childIds.push(agentId);
		this.registerRecord(record);
		this.setStatus(agentId, "running");

		void created.session.prompt(config.prompt).catch((error) => {
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
			queueMicrotask(() => {
				void this.finalizeIdle(agentId);
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

		if (target.status === "idle" && !target.reactivating) {
			target.reactivating = true;
			this.setStatus(targetAgentId, "running");
			try {
				await target.session.sendCustomMessage(message, { triggerTurn: true });
			} finally {
				target.reactivating = false;
			}
			return;
		}

		await target.session.sendCustomMessage(message, { deliverAs: "steer" });
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
	}

	private createChildTools(agentId: string): ToolDefinition[] {
		return [createSpawnTool(this, agentId), createMessageTool(this, agentId), createReportTool(this, agentId)];
	}

	private getWorklogFile(agentId: string): string {
		const filePath = join(this.worklogDir, `${agentId}.worklog.md`);
		ensureDir(dirname(filePath));
		return filePath;
	}
}
