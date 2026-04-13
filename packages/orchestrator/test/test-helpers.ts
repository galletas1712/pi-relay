import { mkdtempSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import type { AgentMessage, AgentTool } from "@mariozechner/pi-agent-core";
import type { Message, Model } from "@mariozechner/pi-ai";
import type { AgentSessionEvent } from "@mariozechner/pi-coding-agent";
import type { AgentSessionHandle, SessionCustomMessage } from "../src/types.js";

export const TEST_MODEL: Model<any> = {
	id: "gpt-5.4",
	name: "gpt-5.4",
	api: "openai-responses",
	provider: "openai",
	baseUrl: "https://api.openai.com/v1",
	reasoning: true,
	input: ["text"],
	cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
	contextWindow: 200_000,
	maxTokens: 8_192,
};

export function createTempDir(prefix: string): string {
	return mkdtempSync(join(tmpdir(), prefix));
}

export async function waitForMicrotasks(): Promise<void> {
	await new Promise((resolve) => setTimeout(resolve, 0));
}

export class FakeSession implements AgentSessionHandle {
	agent: AgentSessionHandle["agent"];
	model: Model<any> | undefined = TEST_MODEL;
	thinkingLevel = "medium" as const;
	isStreaming = false;
	isRetrying = false;
	isCompacting = false;
	sessionManager: AgentSessionHandle["sessionManager"];
	sessionId: string;
	sessionFile: string | undefined;
	extensionRunner?: { emit(event: { type: "session_shutdown" }): Promise<void> };
	readonly sentMessages: Array<{ message: SessionCustomMessage; options: unknown }> = [];
	readonly prompts: string[] = [];
	readonly appendedSessionMessages: AgentMessage[] = [];
	readonly boundExtensionCalls: object[] = [];
	lastAssistantText?: string;
	private readonly listeners = new Set<(event: AgentSessionEvent) => void>();

	constructor(
		id: string,
		options?: {
			sessionDir?: string;
			sessionFile?: string;
			createSessionFile?: boolean;
			messages?: AgentMessage[];
			systemPrompt?: string;
			tools?: AgentTool<any>[];
			transformContext?: AgentSessionHandle["agent"]["transformContext"];
			convertToLlm?: AgentSessionHandle["agent"]["convertToLlm"];
			streamFn?: AgentSessionHandle["agent"]["streamFn"];
			waitForIdle?: AgentSessionHandle["agent"]["waitForIdle"];
			hasQueuedMessages?: AgentSessionHandle["agent"]["hasQueuedMessages"];
		},
	) {
		this.sessionId = id;
		const sessionDir = options?.sessionDir ?? createTempDir("pi-relay-orchestrator-");
		mkdirSync(sessionDir, { recursive: true });
		this.sessionFile = options?.sessionFile ?? join(sessionDir, `${id}.jsonl`);
		if (options?.createSessionFile ?? true) {
			writeFileSync(this.sessionFile, "seed\n", "utf-8");
		}

		this.sessionManager = {
			getCwd: () => sessionDir,
			getSessionDir: () => sessionDir,
			getSessionId: () => id,
			getSessionFile: () => this.sessionFile,
			appendMessage: (message) => {
				this.appendedSessionMessages.push(message);
				return `entry-${this.appendedSessionMessages.length}`;
			},
		};

		this.agent = {
			state: {
				tools: options?.tools ?? [],
				messages: options?.messages ?? [],
				systemPrompt: options?.systemPrompt ?? "Base system prompt",
			},
			transformContext: options?.transformContext,
			convertToLlm: options?.convertToLlm ?? (async (messages: AgentMessage[]) => messages as Message[]),
			streamFn:
				options?.streamFn ??
				(async () =>
					({
						result: async () => ({
							role: "assistant",
							content: [{ type: "text", text: "No worklog update" }],
							stopReason: "stop",
							timestamp: Date.now(),
						}),
					}) as never),
			getApiKey: undefined,
			onPayload: undefined,
			sessionId: id,
			thinkingBudgets: undefined,
			transport: "sse",
			maxRetryDelayMs: undefined,
			waitForIdle: options?.waitForIdle ?? (async () => {}),
			hasQueuedMessages: options?.hasQueuedMessages ?? (() => false),
			mailbox: { close: () => {} },
			onBackgroundToolStart: undefined,
			onBackgroundToolEnd: undefined,
		} as never;
	}

	getAllTools() {
		return [];
	}

	getLastAssistantText() {
		return this.lastAssistantText;
	}

	async bindExtensions(bindings: object) {
		this.boundExtensionCalls.push(bindings);
	}

	subscribe(listener: (event: AgentSessionEvent) => void) {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	}

	async sendCustomMessage(message: SessionCustomMessage, options?: unknown) {
		this.sentMessages.push({ message, options });
	}

	async prompt(message: string) {
		this.prompts.push(message);
	}

	async abort() {}

	dispose() {}

	emit(event: AgentSessionEvent) {
		for (const listener of this.listeners) {
			listener(event);
		}
	}
}

export function cleanupTempDir(path: string): void {
	rmSync(path, { recursive: true, force: true });
}
