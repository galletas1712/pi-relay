import type { Model } from "@pi-relay/ai";
import { describe, expect, it, vi } from "vitest";
import {
	applyForkModelChoice,
	createOrchestratorExtension,
	resolveModelByReference,
	resolveThinkingLevel,
} from "../src/extension.js";
import { Orchestrator } from "../src/orchestrator.js";
import { FakeSession, waitForMicrotasks } from "./test-helpers.js";

function fakeReasoningModel(id: string, provider = "openai"): Model<any> {
	return {
		id,
		name: id,
		api: "openai-responses",
		provider,
		baseUrl: "https://api.openai.com/v1",
		reasoning: true,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 200_000,
		maxTokens: 8_192,
	} as Model<any>;
}

describe("createOrchestratorExtension", () => {
	function buildHandlers(orchestrator: Partial<Orchestrator>) {
		const handlers = new Map<string, Function>();
		const commands = new Map<string, unknown>();
		const sendUserMessage = vi.fn(async () => {});
		const extension = createOrchestratorExtension({ current: orchestrator as Orchestrator });
		extension({
			on(event, handler) {
				handlers.set(event, handler);
			},
			registerCommand(name, command) {
				commands.set(name, command);
			},
			sendUserMessage,
		} as never);
		return { handlers, commands, sendUserMessage };
	}

	it("does not inject a synthetic restore prompt on session_start", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
		});

		const { handlers, sendUserMessage } = buildHandlers(orchestrator);

		const sessionStart = handlers.get("session_start");
		expect(sessionStart).toBeDefined();
		await sessionStart?.(
			{ type: "session_start", reason: "resume" },
			{
				sessionManager: {
					getSessionId: () => "root-session",
				},
				setSubtreeUsageProvider: vi.fn(),
			},
		);
		await waitForMicrotasks();

		expect(sendUserMessage).not.toHaveBeenCalled();
	});

	it("only disposes the orchestrator when the root session shuts down", async () => {
		const dispose = vi.fn(async () => {});
		const orchestrator = {
			isDisposing: false,
			rootAgentId: "root",
			dispose,
			getAgentIdBySessionId: vi.fn((sessionId: string) => (sessionId === "root-session" ? "root" : "child-agent")),
		} satisfies Partial<Orchestrator>;
		const { handlers } = buildHandlers(orchestrator);

		const sessionShutdown = handlers.get("session_shutdown");
		expect(sessionShutdown).toBeDefined();

		await sessionShutdown?.(
			{ type: "session_shutdown" },
			{
				sessionManager: {
					getSessionId: () => "child-session",
				},
				setSubtreeUsageProvider: vi.fn(),
			},
		);
		expect(dispose).not.toHaveBeenCalled();

		await sessionShutdown?.(
			{ type: "session_shutdown" },
			{
				sessionManager: {
					getSessionId: () => "root-session",
				},
				setSubtreeUsageProvider: vi.fn(),
			},
		);
		expect(dispose).toHaveBeenCalledTimes(1);
	});

	it("registers an agents command that switches the active TUI attachment", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("plan-session");
		child.isStreaming = true;
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
		});
		orchestrator.getRecord("root").childIds.push("plan-agent");
		(orchestrator as { records: Map<string, unknown> }).records.set("plan-agent", {
			id: "plan-agent",
			session: child,
			status: "running",
			parentId: "root",
			childIds: [],
			role: "planner",
			config: { role: "planner", prompt: "" },
			reactivating: false,
			worklogFile: "/tmp/plan.worklog.md",
			createdAt: Date.now(),
			lastStatusChange: Date.now(),
			lastWorklogTurn: 0,
			lastWorklogMessageCount: 0,
			turnCount: 0,
			pendingRestoreIdleNotice: false,
			orphanedPendingToolCallIds: [],
		});
		(orchestrator as { sessionIdToAgentId: Map<string, string> }).sessionIdToAgentId.set("plan-session", "plan-agent");
		root.lastAssistantText = "root summary";
		child.lastAssistantText = "child summary";

		const switchSession = vi.fn(async () => ({ cancelled: false }));
		const notify = vi.fn();
		const select = vi.fn(async () => "  plan-agent [running] planner — child summary");
		const { commands } = buildHandlers(orchestrator);
		const agentsCommand = commands.get("agents") as {
			handler: (args: string, ctx: unknown) => Promise<void>;
		};

		await agentsCommand.handler("", {
			switchSession,
			ui: {
				notify,
				select,
			},
			sessionManager: {
				getSessionId: () => "root-session",
			},
		});

		expect(select).toHaveBeenCalledTimes(1);
		expect(switchSession).toHaveBeenCalledWith(child.sessionFile);
		expect(notify).toHaveBeenCalledWith("Attached to plan-agent (planner).", "info");
	});

	it("does not announce an attach when the switch was cancelled", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("plan-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
		});
		orchestrator.getRecord("root").childIds.push("plan-agent");
		(orchestrator as { records: Map<string, unknown> }).records.set("plan-agent", {
			id: "plan-agent",
			session: child,
			status: "running",
			parentId: "root",
			childIds: [],
			role: "planner",
			config: { role: "planner", prompt: "" },
			reactivating: false,
			worklogFile: "/tmp/plan.worklog.md",
			createdAt: Date.now(),
			lastStatusChange: Date.now(),
			lastWorklogTurn: 0,
			lastWorklogMessageCount: 0,
			turnCount: 0,
			pendingRestoreIdleNotice: false,
			orphanedPendingToolCallIds: [],
		});
		(orchestrator as { sessionIdToAgentId: Map<string, string> }).sessionIdToAgentId.set("plan-session", "plan-agent");

		const switchSession = vi.fn(async () => ({ cancelled: true }));
		const notify = vi.fn();
		const { commands } = buildHandlers(orchestrator);
		const agentsCommand = commands.get("agents") as {
			handler: (args: string, ctx: unknown) => Promise<void>;
		};

		await agentsCommand.handler("plan-agent", {
			switchSession,
			ui: {
				notify,
				select: vi.fn(),
			},
			sessionManager: {
				getSessionId: () => "root-session",
			},
		});

		expect(switchSession).toHaveBeenCalledWith(child.sessionFile);
		expect(notify).not.toHaveBeenCalled();
	});

	it("installs and refreshes the relay widget for the active session", async () => {
		const cleanup = vi.fn();
		let refreshWidget: (() => void) | undefined;
		let summaries = [
			{
				id: "root",
				parentId: null,
				role: "root",
				status: "idle",
				depth: 0,
				childCount: 1,
				sessionFile: "/tmp/root.jsonl",
				lastOutput: "root summary",
			},
			{
				id: "child",
				parentId: "root",
				role: "planner",
				status: "running",
				depth: 1,
				childCount: 0,
				sessionFile: "/tmp/child.jsonl",
				lastOutput: "child summary",
			},
		];
		const orchestrator = {
			rootAgentId: "root",
			subscribeToChanges: vi.fn((listener: () => void) => {
				refreshWidget = listener;
				return cleanup;
			}),
			getAgentIdBySessionId: vi.fn((sessionId: string) => (sessionId === "child-session" ? "child" : "root")),
			getAgentSummaries: vi.fn(() => summaries),
		} satisfies Partial<Orchestrator>;
		const uiRef: { cleanup?: () => void; sessionId?: string } = {};
		const handlers = new Map<string, Function>();
		createOrchestratorExtension({ current: orchestrator as Orchestrator }, uiRef)({
			on(event, handler) {
				handlers.set(event, handler);
			},
			registerCommand() {},
			sendUserMessage: vi.fn(async () => {}),
		} as never);

		const setWidget = vi.fn();
		await handlers.get("session_start")?.(
			{ type: "session_start", reason: "startup" },
			{
				hasUI: true,
				ui: { setWidget },
				sessionManager: { getSessionId: () => "child-session" },
				setSubtreeUsageProvider: vi.fn(),
			},
		);

		expect(setWidget).toHaveBeenCalledWith(
			"relay-agents",
			expect.arrayContaining(["Relay Agents", "Use /agents to switch"]),
			{ placement: "belowEditor" },
		);

		refreshWidget?.();
		expect(setWidget).toHaveBeenCalledTimes(2);
		summaries = [summaries[0]!];
		refreshWidget?.();
		expect(setWidget).toHaveBeenLastCalledWith("relay-agents", undefined);

		await handlers.get("session_shutdown")?.(
			{ type: "session_shutdown" },
			{
				sessionManager: { getSessionId: () => "child-session" },
				setSubtreeUsageProvider: vi.fn(),
			},
		);
		expect(cleanup).toHaveBeenCalledTimes(1);
	});

	it("registers a worklog-model command that clears the override on 'none'", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({ rootSession: root, sessionFactory: vi.fn() });
		orchestrator.setForkModel(fakeReasoningModel("gpt-5.4"));
		orchestrator.setForkThinkingLevel("medium");
		const setProvider = vi.fn();
		const clearFork = vi.fn();
		const settingsManager = {
			setWorklogForkModelAndProvider: setProvider,
			setWorklogForkThinkingLevel: vi.fn(),
			clearWorklogForkModel: clearFork,
		} as never;
		const handlers = new Map<string, Function>();
		const commands = new Map<string, unknown>();
		createOrchestratorExtension(
			{ current: orchestrator },
			{},
			{ getSettingsManager: () => settingsManager },
		)({
			on(event, handler) {
				handlers.set(event, handler);
			},
			registerCommand(name, command) {
				commands.set(name, command);
			},
			sendUserMessage: vi.fn(async () => {}),
		} as never);

		const cmd = commands.get("worklog-model") as {
			handler: (args: string, ctx: unknown) => Promise<void>;
		};
		const notify = vi.fn();
		await cmd.handler("none", {
			hasUI: true,
			ui: { notify, select: vi.fn() },
			modelRegistry: { refresh: vi.fn(), getAvailable: vi.fn(() => []) },
		});

		expect(orchestrator.getForkModel()).toBeUndefined();
		expect(orchestrator.getForkThinkingLevel()).toBeUndefined();
		expect(clearFork).toHaveBeenCalledTimes(1);
		expect(setProvider).not.toHaveBeenCalled();
		expect(notify).toHaveBeenCalledWith(
			"Worklog fork: cleared override (will use session model).",
			"info",
		);
	});

	it("registers a worklog-model command that sets provider/id + thinking level from args", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({ rootSession: root, sessionFactory: vi.fn() });
		const settingsManager = {
			setWorklogForkModelAndProvider: vi.fn(),
			setWorklogForkThinkingLevel: vi.fn(),
			clearWorklogForkModel: vi.fn(),
		} as never;
		const model = fakeReasoningModel("gpt-5.4", "openai");
		const handlers = new Map<string, Function>();
		const commands = new Map<string, unknown>();
		createOrchestratorExtension(
			{ current: orchestrator },
			{},
			{ getSettingsManager: () => settingsManager },
		)({
			on(event, handler) {
				handlers.set(event, handler);
			},
			registerCommand(name, command) {
				commands.set(name, command);
			},
			sendUserMessage: vi.fn(async () => {}),
		} as never);

		const cmd = commands.get("worklog-model") as {
			handler: (args: string, ctx: unknown) => Promise<void>;
		};
		const notify = vi.fn();
		await cmd.handler("openai/gpt-5.4 medium", {
			hasUI: true,
			ui: { notify, select: vi.fn() },
			modelRegistry: { refresh: vi.fn(), getAvailable: vi.fn(() => [model]) },
		});

		expect(orchestrator.getForkModel()?.id).toBe("gpt-5.4");
		expect(orchestrator.getForkThinkingLevel()).toBe("medium");
		expect(settingsManager.setWorklogForkModelAndProvider).toHaveBeenCalledWith("openai", "gpt-5.4");
		expect(settingsManager.setWorklogForkThinkingLevel).toHaveBeenCalledWith("medium");
		expect(notify).toHaveBeenCalledWith("Worklog fork: openai/gpt-5.4 (medium).", "info");
	});

	it("registers a worklog-model command that falls back to medium when no level is given", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({ rootSession: root, sessionFactory: vi.fn() });
		const model = fakeReasoningModel("gpt-5.4");
		const handlers = new Map<string, Function>();
		const commands = new Map<string, unknown>();
		createOrchestratorExtension({ current: orchestrator }, {})({
			on(event, handler) {
				handlers.set(event, handler);
			},
			registerCommand(name, command) {
				commands.set(name, command);
			},
			sendUserMessage: vi.fn(async () => {}),
		} as never);

		const cmd = commands.get("worklog-model") as {
			handler: (args: string, ctx: unknown) => Promise<void>;
		};
		await cmd.handler("openai/gpt-5.4", {
			hasUI: true,
			ui: { notify: vi.fn(), select: vi.fn() },
			modelRegistry: { refresh: vi.fn(), getAvailable: vi.fn(() => [model]) },
		});
		expect(orchestrator.getForkThinkingLevel()).toBe("medium");
	});

	it("opens the interactive picker when no argument is passed", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({ rootSession: root, sessionFactory: vi.fn() });
		const model = fakeReasoningModel("gpt-5.4");
		const handlers = new Map<string, Function>();
		const commands = new Map<string, unknown>();
		createOrchestratorExtension({ current: orchestrator }, {})({
			on(event, handler) {
				handlers.set(event, handler);
			},
			registerCommand(name, command) {
				commands.set(name, command);
			},
			sendUserMessage: vi.fn(async () => {}),
		} as never);

		const cmd = commands.get("worklog-model") as {
			handler: (args: string, ctx: unknown) => Promise<void>;
		};
		const select = vi
			.fn()
			.mockResolvedValueOnce("openai/gpt-5.4")
			.mockResolvedValueOnce("high");
		await cmd.handler("", {
			hasUI: true,
			ui: { notify: vi.fn(), select },
			modelRegistry: { refresh: vi.fn(), getAvailable: vi.fn(() => [model]) },
		});
		expect(select).toHaveBeenCalledTimes(2);
		expect(orchestrator.getForkModel()?.id).toBe("gpt-5.4");
		expect(orchestrator.getForkThinkingLevel()).toBe("high");
	});
});

describe("worklog-model helpers", () => {
	it("resolveModelByReference: exact provider/id wins", () => {
		const models = [fakeReasoningModel("gpt-5.4", "openai"), fakeReasoningModel("gpt-5.4", "azure-openai-responses")];
		const match = resolveModelByReference("azure-openai-responses/gpt-5.4", models);
		expect(match?.provider).toBe("azure-openai-responses");
	});

	it("resolveModelByReference: ambiguous bare id returns undefined", () => {
		const models = [fakeReasoningModel("gpt-5.4", "openai"), fakeReasoningModel("gpt-5.4", "azure-openai-responses")];
		expect(resolveModelByReference("gpt-5.4", models)).toBeUndefined();
	});

	it("resolveModelByReference: unique bare id matches", () => {
		const models = [fakeReasoningModel("gpt-5.4", "openai")];
		expect(resolveModelByReference("gpt-5.4", models)?.provider).toBe("openai");
	});

	it("resolveThinkingLevel: explicit match wins", () => {
		const model = fakeReasoningModel("gpt-5.4");
		expect(resolveThinkingLevel(model, "high")).toBe("high");
	});

	it("resolveThinkingLevel: defaults to medium when no explicit level", () => {
		const model = fakeReasoningModel("gpt-5.4");
		expect(resolveThinkingLevel(model, undefined)).toBe("medium");
	});

	it("resolveThinkingLevel: falls back to first supported when requested isn't supported", () => {
		const model = fakeReasoningModel("gpt-5.4");
		expect(resolveThinkingLevel(model, "totally-bogus")).toBe("medium");
	});

	it("applyForkModelChoice: mirrors choice into orchestrator AND settings", () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({ rootSession: root, sessionFactory: vi.fn() });
		const setProvider = vi.fn();
		const setLevel = vi.fn();
		const clearFork = vi.fn();
		const settings = {
			setWorklogForkModelAndProvider: setProvider,
			setWorklogForkThinkingLevel: setLevel,
			clearWorklogForkModel: clearFork,
		} as never;
		const model = fakeReasoningModel("gpt-5.4");
		applyForkModelChoice(orchestrator, model, "medium", settings);
		expect(orchestrator.getForkModel()?.id).toBe("gpt-5.4");
		expect(orchestrator.getForkThinkingLevel()).toBe("medium");
		expect(setProvider).toHaveBeenCalledWith("openai", "gpt-5.4");
		expect(setLevel).toHaveBeenCalledWith("medium");

		applyForkModelChoice(orchestrator, undefined, undefined, settings);
		expect(orchestrator.getForkModel()).toBeUndefined();
		expect(orchestrator.getForkThinkingLevel()).toBeUndefined();
		expect(clearFork).toHaveBeenCalledTimes(1);
	});

	it("applyForkModelChoice: works without a settings manager (session-only change)", () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({ rootSession: root, sessionFactory: vi.fn() });
		const model = fakeReasoningModel("gpt-5.4");
		applyForkModelChoice(orchestrator, model, "medium");
		expect(orchestrator.getForkModel()?.id).toBe("gpt-5.4");
		expect(orchestrator.getForkThinkingLevel()).toBe("medium");
	});
});
