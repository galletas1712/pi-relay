import { resolve } from "node:path";
import type { AgentSession, AgentSessionRuntime } from "@pi-relay/coding-agent";
import type { Orchestrator } from "@pi-relay/orchestrator";

export type RelayRuntimeEngineMode = "legacy" | "ts-core" | "rust-shadow" | "rust";

export interface RelayRuntimeNotice {
	level: "info" | "warning" | "error";
	message: string;
	source: "session-shadow";
	timestamp: string;
}

export interface RelayRuntimeNoticeStore {
	push(notice: Omit<RelayRuntimeNotice, "timestamp"> & { timestamp?: string }): void;
	drain(): RelayRuntimeNotice[];
	subscribe(listener: (notice: RelayRuntimeNotice) => void): () => void;
}

export interface RelaySessionShadowState {
	requestedMode: RelayRuntimeEngineMode;
	effectiveMode: "disabled" | "shadow";
	authority: "ts";
	status: "disabled" | "starting" | "running" | "disconnected" | "stopped";
	lastError?: string;
}

export function createRelayRuntimeNoticeStore(): RelayRuntimeNoticeStore {
	const buffered: RelayRuntimeNotice[] = [];
	const listeners = new Set<(notice: RelayRuntimeNotice) => void>();

	return {
		push(notice) {
			const stamped: RelayRuntimeNotice = {
				...notice,
				timestamp: notice.timestamp ?? new Date().toISOString(),
			};
			buffered.push(stamped);
			for (const listener of listeners) {
				listener(stamped);
			}
		},
		drain() {
			return buffered.splice(0, buffered.length);
		},
		subscribe(listener) {
			listeners.add(listener);
			return () => listeners.delete(listener);
		},
	};
}

export interface RelayRuntimeStateRef {
	current?: {
		orchestrator: Orchestrator;
		engineConfig?: {
			orchestrator: RelayRuntimeEngineMode;
			session: RelayRuntimeEngineMode;
		};
		runtimeNoticeStore?: RelayRuntimeNoticeStore;
		sessionShadow?: RelaySessionShadowState;
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

	get sessionShadow(): RelaySessionShadowState | undefined {
		return this.stateRef.current?.sessionShadow;
	}

	get session(): AgentSession {
		return this.getAttachedRecord().session as unknown as AgentSession;
	}

	getAttachedAgentId(): string {
		return this.attachedAgentId;
	}

	consumeRuntimeNotices(): RelayRuntimeNotice[] {
		return this.stateRef.current?.runtimeNoticeStore?.drain() ?? [];
	}

	subscribeToSessionChanges(listener: (change: RelayRuntimeSessionChange) => void): () => void {
		this.ensureOrchestratorSubscription();
		this.sessionChangeListeners.add(listener);
		return () => {
			this.sessionChangeListeners.delete(listener);
		};
	}

	subscribeToRuntimeNotices(listener: (notice: RelayRuntimeNotice) => void): () => void {
		return this.stateRef.current?.runtimeNoticeStore?.subscribe(listener) ?? (() => {});
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
