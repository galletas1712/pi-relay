import { describe, expect, it, vi } from "vitest";
import { createOrchestratorExtension } from "../src/extension.js";
import { Orchestrator } from "../src/orchestrator.js";
import { FakeSession, waitForMicrotasks } from "./test-helpers.js";

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
			},
		);
		await waitForMicrotasks();

		expect(sendUserMessage).not.toHaveBeenCalled();
	});

	it("rewrites the system prompt with role-aware orchestrator guidance", async () => {
		const orchestrator = {
			getAgentIdBySessionId: vi.fn(() => "child-agent"),
			getRecord: vi.fn(() => ({
				role: "runtime-inspector",
				parentId: "root",
			})),
		} satisfies Partial<Orchestrator>;
		const { handlers } = buildHandlers(orchestrator);

		const beforeAgentStart = handlers.get("before_agent_start");
		expect(beforeAgentStart).toBeDefined();
		const result = await beforeAgentStart?.(
			{ systemPrompt: "Base prompt" },
			{
				sessionManager: {
					getSessionId: () => "child-session",
				},
			},
		);

		expect(result).toEqual(
			expect.objectContaining({
				systemPrompt: expect.stringContaining("Some tools support a `__background` parameter."),
			}),
		);
		expect(String(result?.systemPrompt)).toContain("Your role in the current agent tree: runtime-inspector.");
		expect(String(result?.systemPrompt)).toContain("`spawn`: create a child agent for an independent subtask");
		expect(String(result?.systemPrompt)).toContain(
			"If you need several independent tool calls for the same turn, emit them together in one assistant response",
		);
		expect(String(result?.systemPrompt)).toContain(
			"If you decide to delegate several independent subtasks, emit all of the `spawn` calls in the same assistant response so the children start together.",
		);
		expect(String(result?.systemPrompt)).toContain(
			"Do not produce extra summaries or coordination messages just because a child reported progress.",
		);
		expect(String(result?.systemPrompt)).toContain("Prefer one final report near the end over many small status pings.");
		expect(String(result?.systemPrompt)).toContain(
			"When you have solid findings, a concrete decision, or a completed result your parent is likely to need, send one concise `report` before finishing.",
		);
		expect(String(result?.systemPrompt)).toContain(
			"Use `report` when the update should change parent behavior now: reprioritize work, redirect a sibling, stop duplicate effort, or react to a blocker/risk.",
		);
		expect(String(result?.systemPrompt)).toContain(
			"Do not rely on `IDLE` to carry your substantive result to your parent.",
		);
		expect(String(result?.systemPrompt)).toContain(
			"If several direct children are still active, wait for the remaining children instead of summarizing each finished child separately",
		);
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
			},
		);
		expect(dispose).not.toHaveBeenCalled();

		await sessionShutdown?.(
			{ type: "session_shutdown" },
			{
				sessionManager: {
					getSessionId: () => "root-session",
				},
			},
		);
		expect(dispose).toHaveBeenCalledTimes(1);
	});

	it("registers an agents command that switches the active TUI attachment", async () => {
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
			},
		);
		expect(cleanup).toHaveBeenCalledTimes(1);
	});
});
