import type { ImageContent, Model } from "@pi-relay/ai";
import type { ThinkingLevel } from "@pi-relay/agent-core";
import type {
	AgentSessionEvent,
	ModelCycleResult,
	PromptOptions,
	SessionStats,
} from "../core/agent-session.js";
import type { SessionInfo } from "../core/session-manager.js";

export type SessionEvent = AgentSessionEvent;
export type { ModelCycleResult, PromptOptions, SessionStats };

/**
 * Snapshot of a session's externally visible state.
 */
export interface SessionState {
	id: string;
	cwd: string;
	sessionFile: string | undefined;
	model: Model<any> | undefined;
	thinkingLevel: ThinkingLevel | undefined;
	isStreaming: boolean;
	isCompacting: boolean;
	isBashRunning: boolean;
	autoCompactionEnabled: boolean;
	steeringMode: "all" | "one-at-a-time";
	followUpMode: "all" | "one-at-a-time";
	scopedModels: ReadonlyArray<{ model: Model<any>; thinkingLevel?: ThinkingLevel }>;
	stats: SessionStats;
}

export type SessionSummary = SessionInfo;

export interface OpenSessionOptions {
	parentSession?: string;
}

export interface ResumeOptions {
	cwdOverride?: string;
}

/**
 * Handle to a running session. Provides the operations a Client consumer
 * needs to drive a conversation without talking to AgentSession directly.
 */
export interface SessionHandle {
	readonly id: string;
	readonly events: AsyncIterable<SessionEvent>;
	prompt(text: string, opts?: PromptOptions): Promise<void>;
	steer(text: string, images?: ImageContent[]): Promise<void>;
	followUp(text: string, images?: ImageContent[]): Promise<void>;
	abort(): Promise<void>;
	switchModel(model: Model<any>): Promise<void>;
	cycleModel(direction: "forward" | "backward"): Promise<ModelCycleResult | undefined>;
	cycleThinking(): Promise<ThinkingLevel>;
	getState(): Promise<SessionState>;
	close(): Promise<void>;
}

export interface ModelInfo {
	provider: string;
	id: string;
	hasAuth: boolean;
}

export type AuthStatus = ReadonlyArray<{ provider: string; hasCredential: boolean }>;

/**
 * Headless SDK surface consumed by the TUI and (eventually) remote clients.
 *
 * LocalClient delegates in-process; a future RpcClient will serialize calls
 * over the wire. Both implement this interface.
 */
export interface Client {
	readonly sessions: {
		open(opts?: OpenSessionOptions): Promise<SessionHandle>;
		resume(sessionPath: string, opts?: ResumeOptions): Promise<SessionHandle>;
		list(): Promise<SessionSummary[]>;
	};
	session(): SessionHandle;

	readonly models: {
		list(): Promise<Model<any>[]>;
		listAvailable(): Promise<Model<any>[]>;
	};

	readonly auth: {
		login(provider: string): Promise<void>;
		logout(provider: string): Promise<void>;
		status(): Promise<AuthStatus>;
	};

	dispose(): Promise<void>;
}
