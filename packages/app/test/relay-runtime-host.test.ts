import { describe, expect, it, vi } from "vitest";
import { FakeSession } from "../../orchestrator/test/test-helpers.js";
import { RelayRuntimeHost } from "../src/relay-runtime-host.js";

describe("RelayRuntimeHost", () => {
	it("attaches to a live child session instead of rebuilding the root runtime", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const emit = vi.fn(async () => undefined);
		root.extensionRunner = {
			hasHandlers: () => true,
			emit,
		} as never;
		const switchSession = vi.fn(async () => ({ cancelled: false }));
		const runtime = {
			session: root,
			services: { cwd: root.sessionManager.getCwd() },
			diagnostics: [],
			modelFallbackMessage: undefined,
			switchSession,
			newSession: vi.fn(),
			fork: vi.fn(),
			importFromJsonl: vi.fn(),
			dispose: vi.fn(),
		} as never;
		const orchestrator = {
			rootAgentId: "root",
			subscribeToChanges: vi.fn(() => () => {}),
			getRecord: (agentId: string) => {
				if (agentId === "root") {
					return { id: "root", status: "idle", session: root };
				}
				if (agentId === "child") {
					return { id: "child", status: "running", session: child };
				}
				throw new Error(`Unknown agent ${agentId}`);
			},
			findAgentIdBySessionFile: (sessionFile: string) => (sessionFile === child.sessionFile ? "child" : undefined),
		} as never;
		const host = new RelayRuntimeHost(runtime, {
			current: { orchestrator },
		});

		await host.switchSession(child.sessionFile!);

		expect(host.getAttachedAgentId()).toBe("child");
		expect(host.session).toBe(child);
		expect(switchSession).not.toHaveBeenCalled();
		expect(emit).toHaveBeenCalledWith({
			type: "session_before_switch",
			reason: "resume",
			targetSessionFile: child.sessionFile,
		});
	});

	it("blocks non-relay session switches while attached to a child", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const switchSession = vi.fn(async () => ({ cancelled: false }));
		const runtime = {
			session: root,
			services: { cwd: root.sessionManager.getCwd() },
			diagnostics: [],
			modelFallbackMessage: undefined,
			switchSession,
			newSession: vi.fn(),
			fork: vi.fn(),
			importFromJsonl: vi.fn(),
			dispose: vi.fn(),
		} as never;
		const orchestrator = {
			rootAgentId: "root",
			subscribeToChanges: vi.fn(() => () => {}),
			getRecord: (agentId: string) => {
				if (agentId === "root") {
					return { id: "root", status: "idle", session: root };
				}
				if (agentId === "child") {
					return { id: "child", status: "running", session: child };
				}
				throw new Error(`Unknown agent ${agentId}`);
			},
			findAgentIdBySessionFile: (sessionFile: string) => (sessionFile === child.sessionFile ? "child" : undefined),
		} as never;
		const host = new RelayRuntimeHost(runtime, {
			current: { orchestrator },
		});

		await host.switchSession(child.sessionFile!);
		const result = await host.switchSession("/tmp/other-session.jsonl");

		expect(result).toEqual(
			expect.objectContaining({
				cancelled: true,
				message: expect.stringContaining("root agent"),
			}),
		);
		expect(host.getAttachedAgentId()).toBe("child");
		expect(switchSession).not.toHaveBeenCalled();
	});

	it("falls back to root when the attached child has been disposed", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		let childDisposed = false;
		const runtime = {
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
		const orchestrator = {
			rootAgentId: "root",
			subscribeToChanges: vi.fn(() => () => {}),
			getRecord: (agentId: string) => {
				if (agentId === "root") {
					return { id: "root", status: "idle", session: root };
				}
				if (agentId === "child") {
					return { id: "child", status: childDisposed ? "disposed" : "running", session: child };
				}
				throw new Error(`Unknown agent ${agentId}`);
			},
			findAgentIdBySessionFile: (sessionFile: string) => (sessionFile === child.sessionFile ? "child" : undefined),
		} as never;
		const host = new RelayRuntimeHost(runtime, {
			current: { orchestrator },
		});

		await host.switchSession(child.sessionFile!);
		childDisposed = true;

		expect(host.session).toBe(root);
		expect(host.getAttachedAgentId()).toBe("root");
	});

	it("blocks root-level session management while attached to a child", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		const runtime = {
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
		const orchestrator = {
			rootAgentId: "root",
			subscribeToChanges: vi.fn(() => () => {}),
			getRecord: (agentId: string) => {
				if (agentId === "root") {
					return { id: "root", status: "idle", session: root };
				}
				if (agentId === "child") {
					return { id: "child", status: "running", session: child };
				}
				throw new Error(`Unknown agent ${agentId}`);
			},
			findAgentIdBySessionFile: (sessionFile: string) => (sessionFile === child.sessionFile ? "child" : undefined),
		} as never;
		const host = new RelayRuntimeHost(runtime, {
			current: { orchestrator },
		});

		await host.switchSession(child.sessionFile!);
		const result = await host.newSession();

		expect(result).toEqual(
			expect.objectContaining({
				cancelled: true,
				message: expect.stringContaining("root agent view"),
			}),
		);
	});

	it("notifies listeners when a disposed attached child falls back to root", async () => {
		const root = new FakeSession("root-session");
		const child = new FakeSession("child-session");
		let childStatus: "running" | "disposed" = "running";
		let onChange: (() => void) | undefined;
		const runtime = {
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
				if (agentId === "child") {
					return { id: "child", status: childStatus, session: child };
				}
				throw new Error(`Unknown agent ${agentId}`);
			},
			findAgentIdBySessionFile: (sessionFile: string) => (sessionFile === child.sessionFile ? "child" : undefined),
		} as never;
		const host = new RelayRuntimeHost(runtime, {
			current: { orchestrator },
		});
		const changeListener = vi.fn();
		host.subscribeToSessionChanges(changeListener);

		await host.switchSession(child.sessionFile!);
		childStatus = "disposed";
		onChange?.();

		expect(host.getAttachedAgentId()).toBe("root");
		expect(host.session).toBe(root);
		expect(changeListener).toHaveBeenCalledWith({
			message: "Attached agent exited; returned to root.",
			reason: "fallback",
		});
	});
});
