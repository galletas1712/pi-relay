import { describe, expect, it, vi } from "vitest";
import { FakeSession } from "../../orchestrator/test/test-helpers.js";
import { RelayRuntimeHost } from "../src/relay-runtime-host.js";

function createRuntime(root: FakeSession) {
	return {
		session: root,
		services: { cwd: root.sessionManager.getCwd() },
		diagnostics: [],
		modelFallbackMessage: undefined,
		switchSession: vi.fn(async () => ({ cancelled: false })),
		newSession: vi.fn(),
		fork: vi.fn(),
		importFromJsonl: vi.fn(),
		dispose: vi.fn(),
	} as never;
}

function createRootOnlyOrchestrator(root: FakeSession, cleanup: ReturnType<typeof vi.fn>) {
	return {
		rootAgentId: "root",
		subscribeToChanges: vi.fn(() => cleanup),
		getRecord: (agentId: string) => {
			if (agentId === "root") {
				return { id: "root", status: "idle", session: root };
			}
			throw new Error(`Unknown agent ${agentId}`);
		},
		findAgentIdBySessionFile: () => undefined,
	} as never;
}

describe("RelayRuntimeHost lifecycle", () => {
	it("replaces orchestrator subscriptions when the runtime swaps orchestrator instances", async () => {
		const root = new FakeSession("root-session");
		const runtime = createRuntime(root);
		const firstCleanup = vi.fn();
		const secondCleanup = vi.fn();
		const firstOrchestrator = createRootOnlyOrchestrator(root, firstCleanup);
		const secondOrchestrator = createRootOnlyOrchestrator(root, secondCleanup);
		const stateRef = {
			current: {
				orchestrator: firstOrchestrator,
			},
		};
		const host = new RelayRuntimeHost(runtime, stateRef);

		host.subscribeToSessionChanges(() => undefined);
		stateRef.current = {
			orchestrator: secondOrchestrator,
		};
		host.subscribeToSessionChanges(() => undefined);

		expect(firstCleanup).toHaveBeenCalledTimes(1);
		expect(secondOrchestrator.subscribeToChanges).toHaveBeenCalledTimes(1);

		await host.dispose();
		expect(secondCleanup).toHaveBeenCalledTimes(1);
		expect(runtime.dispose).toHaveBeenCalledTimes(1);
	});

	it("falls back to root when the attached child disappears from the orchestrator", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		let childPresent = true;
		let onChange: (() => void) | undefined;
		const runtime = createRuntime(root);
		const orchestrator = {
			rootAgentId: "root",
			subscribeToChanges: vi.fn((listener: () => void) => {
				onChange = listener;
				return () => {
					onChange = undefined;
				};
			}),
			getRecord: (agentId: string) => {
				if (agentId === "root") {
					return { id: "root", status: "idle", session: root };
				}
				if (agentId === "child" && childPresent) {
					return { id: "child", status: "running", session: child };
				}
				throw new Error(`Unknown agent ${agentId}`);
			},
			findAgentIdBySessionFile: (sessionFile: string) => (sessionFile === child.sessionFile ? "child" : undefined),
		} as never;
		const host = new RelayRuntimeHost(runtime, {
			current: { orchestrator },
		});
		const listener = vi.fn();
		host.subscribeToSessionChanges(listener);

		await host.switchSession(child.sessionFile!);
		childPresent = false;
		onChange?.();

		expect(host.getAttachedAgentId()).toBe("root");
		expect(host.session).toBe(root);
		expect(listener).toHaveBeenCalledWith({
			message: "Attached agent exited; returned to root.",
			reason: "fallback",
		});
	});
});
