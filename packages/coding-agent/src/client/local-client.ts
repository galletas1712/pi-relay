import type { ImageContent, Model } from "@pi-relay/ai";
import type { ThinkingLevel } from "@pi-relay/agent-core";
import type { AgentSession, ModelCycleResult, PromptOptions } from "../core/agent-session.js";
import type { AgentSessionRuntime } from "../core/agent-session-runtime.js";
import type { AgentSessionServices } from "../core/agent-session-services.js";
import { SessionManager } from "../core/session-manager.js";
import type {
	AuthStatus,
	Client,
	OpenSessionOptions,
	ResumeOptions,
	SessionEvent,
	SessionHandle,
	SessionState,
	SessionSummary,
} from "./types.js";

/**
 * Internals exposed to in-process consumers that still need deep runtime
 * access (e.g. the TUI and orchestrator). External/RPC consumers must not
 * depend on these — every access is a candidate for future removal as the
 * Client surface grows.
 */
export interface LocalClientInternals {
	readonly runtime: AgentSessionRuntime;
	readonly services: AgentSessionServices;
}

/**
 * Single-consumer async event queue. The SessionHandle owns it and pushes
 * AgentSession events into it; one iterator drains the backlog then blocks
 * on new events until close() is called. A fresh iterator starts from the
 * current tail, not the historical buffer.
 */
class EventQueue {
	private buffer: SessionEvent[] = [];
	private resolvers: Array<(result: IteratorResult<SessionEvent>) => void> = [];
	private closed = false;

	push(event: SessionEvent): void {
		if (this.closed) return;
		const resolver = this.resolvers.shift();
		if (resolver) {
			resolver({ value: event, done: false });
			return;
		}
		this.buffer.push(event);
	}

	close(): void {
		if (this.closed) return;
		this.closed = true;
		while (this.resolvers.length > 0) {
			const resolver = this.resolvers.shift();
			resolver?.({ value: undefined as unknown as SessionEvent, done: true });
		}
	}

	iterator(): AsyncIterableIterator<SessionEvent> {
		return {
			[Symbol.asyncIterator]() {
				return this;
			},
			next: (): Promise<IteratorResult<SessionEvent>> => {
				if (this.buffer.length > 0) {
					const value = this.buffer.shift() as SessionEvent;
					return Promise.resolve({ value, done: false });
				}
				if (this.closed) {
					return Promise.resolve({ value: undefined as unknown as SessionEvent, done: true });
				}
				return new Promise((resolve) => this.resolvers.push(resolve));
			},
			return: (): Promise<IteratorResult<SessionEvent>> => {
				return Promise.resolve({ value: undefined as unknown as SessionEvent, done: true });
			},
		};
	}
}

class LocalSessionHandle implements SessionHandle {
	private queue = new EventQueue();
	private unsubscribe: () => void;
	private closed = false;

	constructor(private readonly session: AgentSession) {
		this.unsubscribe = session.subscribe((event) => this.queue.push(event));
	}

	get id(): string {
		return this.session.sessionManager.getSessionId();
	}

	get events(): AsyncIterable<SessionEvent> {
		return { [Symbol.asyncIterator]: () => this.queue.iterator() };
	}

	async prompt(text: string, opts?: PromptOptions): Promise<void> {
		await this.session.prompt(text, opts);
	}

	async steer(text: string, images?: ImageContent[]): Promise<void> {
		await this.session.steer(text, images);
	}

	async followUp(text: string, images?: ImageContent[]): Promise<void> {
		await this.session.followUp(text, images);
	}

	async abort(): Promise<void> {
		await this.session.abort();
	}

	async switchModel(model: Model<any>): Promise<void> {
		await this.session.setModel(model);
	}

	async cycleModel(direction: "forward" | "backward"): Promise<ModelCycleResult | undefined> {
		return this.session.cycleModel(direction);
	}

	async cycleThinking(): Promise<ThinkingLevel> {
		return this.session.cycleThinkingLevel();
	}

	async getState(): Promise<SessionState> {
		return {
			id: this.session.sessionManager.getSessionId(),
			cwd: this.session.sessionManager.getCwd(),
			sessionFile: this.session.sessionFile,
			model: this.session.model,
			thinkingLevel: this.session.thinkingLevel,
			isStreaming: this.session.isStreaming,
			isCompacting: this.session.isCompacting,
			isBashRunning: this.session.isBashRunning,
			autoCompactionEnabled: this.session.autoCompactionEnabled,
			steeringMode: this.session.steeringMode,
			followUpMode: this.session.followUpMode,
			scopedModels: this.session.scopedModels,
			stats: this.session.getSessionStats(),
		};
	}

	async close(): Promise<void> {
		if (this.closed) return;
		this.closed = true;
		this.unsubscribe();
		this.queue.close();
	}
}

/**
 * In-process Client implementation that delegates to an AgentSessionRuntime.
 *
 * Every method call routes straight to the underlying services without
 * serializing anything. The interactive TUI consumes this through the Client
 * surface plus the `internals` escape hatch for areas the interface doesn't
 * yet cover (extension UI binding, footer data, resource loader, etc.).
 */
export class LocalClient implements Client {
	private handles = new WeakMap<AgentSession, LocalSessionHandle>();
	private disposed = false;

	constructor(private readonly runtime: AgentSessionRuntime) {}

	get internals(): LocalClientInternals {
		return { runtime: this.runtime, services: this.runtime.services };
	}

	private handleFor(session: AgentSession): LocalSessionHandle {
		let handle = this.handles.get(session);
		if (!handle) {
			handle = new LocalSessionHandle(session);
			this.handles.set(session, handle);
		}
		return handle;
	}

	session(): SessionHandle {
		return this.handleFor(this.runtime.session);
	}

	readonly sessions = {
		open: async (opts?: OpenSessionOptions): Promise<SessionHandle> => {
			await this.runtime.newSession(opts ? { parentSession: opts.parentSession } : undefined);
			return this.handleFor(this.runtime.session);
		},
		resume: async (sessionPath: string, opts?: ResumeOptions): Promise<SessionHandle> => {
			await this.runtime.switchSession(sessionPath, opts?.cwdOverride);
			return this.handleFor(this.runtime.session);
		},
		list: async (): Promise<SessionSummary[]> => {
			return SessionManager.list(this.runtime.cwd, this.runtime.services.settingsManager.getSessionDir());
		},
	};

	readonly models = {
		list: async (): Promise<Model<any>[]> => this.runtime.services.modelRegistry.getAll(),
		listAvailable: async (): Promise<Model<any>[]> => this.runtime.services.modelRegistry.getAvailable(),
	};

	readonly auth = {
		// OAuth login requires interactive callbacks (URL display, code prompt). The
		// TUI drives that flow through internals.services.authStorage directly. A
		// headless Client.auth.login belongs with the RPC transport work in Task 4.
		login: async (_provider: string): Promise<void> => {
			throw new Error("LocalClient.auth.login is not implemented; use internals.services.authStorage");
		},
		logout: async (provider: string): Promise<void> => {
			this.runtime.services.authStorage.logout(provider);
			this.runtime.services.modelRegistry.refresh();
		},
		status: async (): Promise<AuthStatus> => {
			const providers = this.runtime.services.authStorage.list();
			return providers.map((provider) => ({ provider, hasCredential: true }));
		},
	};

	async dispose(): Promise<void> {
		if (this.disposed) return;
		this.disposed = true;
		await this.runtime.dispose();
	}
}
