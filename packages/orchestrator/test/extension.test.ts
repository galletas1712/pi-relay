import { describe, expect, it, vi } from "vitest";
import { createOrchestratorExtension } from "../src/extension.js";
import { Orchestrator } from "../src/orchestrator.js";

describe("createOrchestratorExtension", () => {
	function buildHandlers(orchestrator: Partial<Orchestrator>) {
		const handlers = new Map<string, Function>();
		const extension = createOrchestratorExtension({ current: orchestrator as Orchestrator });
		extension({
			on(event, handler) {
				handlers.set(event, handler);
			},
		} as never);
		return handlers;
	}

	it("only disposes the orchestrator when the root session shuts down", async () => {
		const dispose = vi.fn(async () => {});
		const orchestrator = {
			isDisposing: false,
			rootAgentId: "root",
			dispose,
			getAgentIdBySessionId: vi.fn((sessionId: string) => (sessionId === "root-session" ? "root" : "child-agent")),
		} satisfies Partial<Orchestrator>;
		const handlers = buildHandlers(orchestrator);

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
