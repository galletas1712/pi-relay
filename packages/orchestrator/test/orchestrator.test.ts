import { describe, expect, it, vi } from "vitest";
import { Orchestrator } from "../src/orchestrator.js";
import type { AgentSessionEvent } from "@mariozechner/pi-coding-agent";
import type { AgentSessionFactoryOptions, AgentSessionHandle } from "../src/types.js";

class FakeSession implements AgentSessionHandle {
	agent = {
		state: { tools: [] },
		onBackgroundToolStart: undefined,
		onBackgroundToolEnd: undefined,
		waitForIdle: async () => {},
		mailbox: { close: () => {} },
	} as never;
	model = undefined;
	thinkingLevel = "off" as const;
	isStreaming = false;
	isRetrying = false;
	isCompacting = false;
	sessionManager;
	sessionId;
	sessionFile;
	private readonly listeners = new Set<(event: AgentSessionEvent) => void>();
	readonly sentMessages: Array<{ message: unknown; options: unknown }> = [];
	readonly prompts: string[] = [];
	lastAssistantText?: string;

	constructor(id: string) {
		this.sessionId = id;
		this.sessionFile = `/tmp/${id}.jsonl`;
		this.sessionManager = {
			getCwd: () => "/tmp",
			getSessionDir: () => "/tmp",
			getSessionId: () => id,
			getSessionFile: () => this.sessionFile,
		};
	}

	getAllTools() {
		return [];
	}

	getLastAssistantText() {
		return this.lastAssistantText;
	}

	async bindExtensions() {}

	subscribe(listener: (event: AgentSessionEvent) => void) {
		this.listeners.add(listener);
		return () => this.listeners.delete(listener);
	}

	async sendCustomMessage(message: unknown, options?: unknown) {
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

describe("Orchestrator", () => {
	it("spawns child sessions through the injected factory", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const factory = vi.fn(async (_options: AgentSessionFactoryOptions) => ({ session: child }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect the repo",
		});

		expect(factory).toHaveBeenCalledTimes(1);
		expect(orchestrator.getRecord(childId).parentId).toBe("root");
		expect(child.prompts).toEqual(["inspect the repo"]);
	});

	it("routes directives only to direct children", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const factory = vi.fn(async () => ({ session: child }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		const childId = await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		await orchestrator.routeMessage("root", childId, "check src");
		expect(child.sentMessages).toHaveLength(1);
		await expect(orchestrator.routeMessage(childId, "root", "bad")).rejects.toThrow("not a direct child");
	});

	it("notifies the parent when a child becomes idle", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		child.lastAssistantText = "finished scanning files";
		const factory = vi.fn(async () => ({ session: child }));
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: factory,
		});

		await orchestrator.spawnAgent("root", {
			role: "explore",
			prompt: "inspect",
		});

		child.emit({ type: "agent_end", messages: [] });
		await Promise.resolve();

		expect(root.sentMessages).toHaveLength(1);
		expect(root.sentMessages[0]?.options).toEqual({ triggerTurn: true });
	});
});
