import { describe, expect, it, vi } from "vitest";
import { createOrchestratorExtension } from "../src/extension.js";
import { Orchestrator } from "../src/orchestrator.js";
import { FakeSession, waitForMicrotasks } from "./test-helpers.js";

describe("createOrchestratorExtension", () => {
	function buildHandlers(orchestrator: Partial<Orchestrator>) {
		const handlers = new Map<string, Function>();
		const sendUserMessage = vi.fn(async () => {});
		const extension = createOrchestratorExtension({ current: orchestrator as Orchestrator });
		extension({
			on(event, handler) {
				handlers.set(event, handler);
			},
			sendUserMessage,
		} as never);
		return { handlers, sendUserMessage };
	}

	it("sends the root restore nudge on session_start", async () => {
		const root = new FakeSession("root-session");
		const orchestrator = new Orchestrator({
			rootSession: root,
			sessionFactory: vi.fn(),
		});
		(orchestrator as { pendingRootResumeSessionId?: string }).pendingRootResumeSessionId = "root-session";

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

		expect(sendUserMessage).toHaveBeenCalledWith("[Session restored]");
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
			"Do not produce extra summaries or coordination messages just because a child reported progress.",
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
});
