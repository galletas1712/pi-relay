import { resolve } from "node:path";
import type { AgentSession, AgentSessionRuntime } from "@mariozechner/pi-coding-agent";
import type { Orchestrator } from "@pi-relay/orchestrator";

export interface RelayRuntimeStateRef {
	current?: {
		orchestrator: Orchestrator;
	};
}

export interface RelayRuntimeSessionChange {
	message: string;
	reason: "fallback";
}

export class RelayRuntimeHost {
	private attachedAgentId = "root";
	private readonly sessionChangeListeners = new Set<(change: RelayRuntimeSessionChange) => void>();
	private orchestratorCleanup?: () => void;
	private observedOrchestrator?: Orchestrator;

	constructor(
		private readonly rootRuntime: AgentSessionRuntime,
		private readonly stateRef: RelayRuntimeStateRef,
	) {
		this.ensureOrchestratorSubscription();
	}

	get services() {
		return this.rootRuntime.services;
	}

	get diagnostics() {
		return this.rootRuntime.diagnostics;
	}

	get cwd(): string {
		return this.session.sessionManager.getCwd();
	}

	get modelFallbackMessage(): string | undefined {
		return this.rootRuntime.modelFallbackMessage;
	}

	get session(): AgentSession {
		return this.getAttachedRecord().session as unknown as AgentSession;
	}

	getAttachedAgentId(): string {
		return this.attachedAgentId;
	}

	getAttachedAgentProgress():
		| {
				displayStatus: "running" | "waiting" | "starting" | "idle";
				runningChildren: Array<{
					id: string;
					role: string;
					displayStatus: "running" | "waiting" | "starting" | "idle";
				}>;
		  }
		| undefined {
		const orchestrator = this.stateRef.current?.orchestrator;
		if (!orchestrator) {
			return undefined;
		}

		const attachedAgentId = this.getAttachedRecord().id;
		const summary = orchestrator.getAgentSummaries().find((entry) => entry.id === attachedAgentId);
		if (!summary) {
			return undefined;
		}

		const runningChildren = orchestrator
			.getDirectChildSummaries(attachedAgentId)
			.filter((child) => child.displayStatus !== "idle")
			.map((child) => ({
				id: child.id,
				role: child.role,
				displayStatus: child.displayStatus,
			}));

		return {
			displayStatus: summary.displayStatus,
			runningChildren,
		};
	}

	async terminateRunningChildren(): Promise<void> {
		const orchestrator = this.stateRef.current?.orchestrator;
		if (!orchestrator) {
			return;
		}

		const attachedAgentId = this.getAttachedRecord().id;
		const children = orchestrator.getDirectChildSummaries(attachedAgentId);
		for (const child of children) {
			if (child.displayStatus !== "idle") {
				try {
					await orchestrator.terminateAgent(attachedAgentId, child.id);
				} catch {
					// Best-effort termination
				}
			}
		}
	}

	subscribeToSessionChanges(listener: (change: RelayRuntimeSessionChange) => void): () => void {
		this.ensureOrchestratorSubscription();
		this.sessionChangeListeners.add(listener);
		return () => {
			this.sessionChangeListeners.delete(listener);
		};
	}

	async switchSession(
		sessionPath: string,
		cwdOverride?: string,
	): Promise<{ cancelled: boolean; message?: string; reason?: "attach" }> {
		const orchestrator = this.getOrchestrator();
		const resolvedSessionPath = resolve(sessionPath);
		const currentRecord = this.getAttachedRecord();
		const attachedAgentId = orchestrator.findAgentIdBySessionFile(resolvedSessionPath);
		if (attachedAgentId) {
			if (await this.emitBeforeSwitch(sessionPath)) {
				return { cancelled: true };
			}
			if (currentRecord.id !== attachedAgentId) {
				this.detachSessionUi(currentRecord.session as unknown as AgentSession);
			}
			this.attachedAgentId = attachedAgentId;
			return {
				cancelled: false,
				message: `Attached to ${attachedAgentId}.`,
				reason: "attach",
			};
		}

		if (currentRecord.id !== orchestrator.rootAgentId) {
			return {
				cancelled: true,
				message: "Session management is only available while attached to the root agent. Use /agents to return to root first.",
			} as { cancelled: boolean };
		}

		const result = await this.rootRuntime.switchSession(sessionPath, cwdOverride);
		if (!result.cancelled) {
			this.attachedAgentId = this.getOrchestrator().rootAgentId;
		}
		return result;
	}

	async newSession(options?: Parameters<AgentSessionRuntime["newSession"]>[0]): Promise<{ cancelled: boolean }> {
		if (this.getAttachedRecord().id !== this.getOrchestrator().rootAgentId) {
			return {
				cancelled: true,
				message: "New sessions can only be created from the root agent view. Use /agents to return to root first.",
			} as { cancelled: boolean };
		}
		return this.rootRuntime.newSession(options);
	}

	async fork(entryId: string): Promise<{ cancelled: boolean; selectedText?: string }> {
		if (this.getAttachedRecord().id !== this.getOrchestrator().rootAgentId) {
			return {
				cancelled: true,
				message: "Session forks can only be created from the root agent view. Use /agents to return to root first.",
			} as { cancelled: boolean; selectedText?: string };
		}
		return this.rootRuntime.fork(entryId);
	}

	async importFromJsonl(inputPath: string, cwdOverride?: string): Promise<{ cancelled: boolean }> {
		if (this.getAttachedRecord().id !== this.getOrchestrator().rootAgentId) {
			return {
				cancelled: true,
				message: "Importing sessions is only available from the root agent view. Use /agents to return to root first.",
			} as { cancelled: boolean };
		}
		return this.rootRuntime.importFromJsonl(inputPath, cwdOverride);
	}

	async dispose(): Promise<void> {
		this.orchestratorCleanup?.();
		this.orchestratorCleanup = undefined;
		this.observedOrchestrator = undefined;
		await this.rootRuntime.dispose();
	}

	private getOrchestrator(): Orchestrator {
		this.ensureOrchestratorSubscription();
		const orchestrator = this.stateRef.current?.orchestrator;
		if (!orchestrator) {
			throw new Error("Relay orchestrator has not been initialized yet.");
		}
		return orchestrator;
	}

	private getAttachedRecord() {
		const orchestrator = this.getOrchestrator();
		const record = this.peekRecord(orchestrator, this.attachedAgentId);
		if (record && record.status !== "disposed") {
			return record;
		}

		this.attachedAgentId = orchestrator.rootAgentId;
		return orchestrator.getRecord(orchestrator.rootAgentId);
	}

	private ensureOrchestratorSubscription(): void {
		const orchestrator = this.stateRef.current?.orchestrator;
		if (!orchestrator || this.observedOrchestrator === orchestrator) {
			return;
		}

		this.orchestratorCleanup?.();
		this.observedOrchestrator = orchestrator;
		this.orchestratorCleanup = orchestrator.subscribeToChanges(() => {
			if (this.attachedAgentId === orchestrator.rootAgentId) {
				return;
			}

			const record = this.peekRecord(orchestrator, this.attachedAgentId);
			if (record && record.status !== "disposed") {
				return;
			}

			this.attachedAgentId = orchestrator.rootAgentId;
			this.emitSessionChange({
				message: "Attached agent exited; returned to root.",
				reason: "fallback",
			});
		});
	}

	private peekRecord(orchestrator: Orchestrator, agentId: string) {
		try {
			return orchestrator.getRecord(agentId);
		} catch {
			return undefined;
		}
	}

	private detachSessionUi(session: AgentSession): void {
		const relaySession = session as AgentSession & { detachExtensions?: () => void };
		if (typeof relaySession.detachExtensions === "function") {
			relaySession.detachExtensions();
			return;
		}
		session.extensionRunner?.setUIContext?.(undefined);
		session.extensionRunner?.bindCommandContext?.(undefined);
	}

	private async emitBeforeSwitch(sessionPath: string): Promise<boolean> {
		const currentSession = this.session;
		const runner = currentSession.extensionRunner;
		if (!runner?.hasHandlers("session_before_switch")) {
			return false;
		}

		const result = await runner.emit({
			type: "session_before_switch",
			reason: "resume",
			targetSessionFile: sessionPath,
		});
		return result?.cancel === true;
	}

	private emitSessionChange(change: RelayRuntimeSessionChange): void {
		for (const listener of this.sessionChangeListeners) {
			listener(change);
		}
	}
}
