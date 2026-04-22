import { beforeEach, describe, expect, it, vi } from "vitest";

const create = vi.fn();
const continueRecent = vi.fn();
const createAgentSessionRuntime = vi.fn();
const createAgentSessionServices = vi.fn();
const createAgentSessionFromServices = vi.fn();
const getAgentDir = vi.fn(() => "/tmp/agent");
const createOrchestratorExtension = vi.fn(() => "extension-factory");
const createRelaySessionFactory = vi.fn(() => "session-factory");
const createSpawnTool = vi.fn(() => ({ name: "spawn" }));
const createMessageTool = vi.fn(() => ({ name: "message" }));
const createRelayBaseToolDefinitionsFactory = vi.fn(() => () => [{ name: "read" }]);
const restore = vi.fn(async () => false);
const runtimeDispose = vi.fn(async () => undefined);

vi.mock("@pi-relay/ai", () => ({
	getModel: vi.fn(),
}));

vi.mock("@pi-relay/coding-agent", () => ({
	SessionManager: {
		create,
		continueRecent,
	},
	createAgentSessionRuntime,
	createAgentSessionServices,
	createAgentSessionFromServices,
	getAgentDir,
}));

vi.mock("@pi-relay/orchestrator", () => ({
	createOrchestratorExtension,
	createRelaySessionFactory,
	createSpawnTool,
	createMessageTool,
	Orchestrator: vi.fn().mockImplementation(() => ({
		rootAgentId: "root",
		restore,
		spawnAgent: vi.fn(),
		routeMessage: vi.fn(),
		subscribeToChanges: vi.fn(() => () => {}),
	})),
}));

vi.mock("../src/tools/base-tools.js", () => ({
	RELAY_BASE_TOOL_NAMES: ["read", "bash", "edit", "apply_patch", "write"],
	createRelayBaseToolDefinitionsFactory,
}));

describe("relay runtime orchestrator bridge lifecycle", () => {
	beforeEach(() => {
		vi.resetModules();
		create.mockClear();
		continueRecent.mockClear();
		createAgentSessionRuntime.mockClear();
		createAgentSessionServices.mockClear();
		createAgentSessionFromServices.mockClear();
		createOrchestratorExtension.mockClear();
		createRelaySessionFactory.mockClear();
		createSpawnTool.mockClear();
		createMessageTool.mockClear();
		createRelayBaseToolDefinitionsFactory.mockClear();
		restore.mockClear();
		runtimeDispose.mockClear();

		create.mockReturnValue({
			getSessionDir: () => "/tmp/sessions",
		});
		continueRecent.mockReturnValue({
			getSessionDir: () => "/tmp/sessions",
		});
		createAgentSessionServices.mockImplementation(async () => ({
			diagnostics: [],
			settingsManager: {},
		}));
		createAgentSessionFromServices.mockResolvedValue({
			session: {
				sessionId: "root-session",
			},
		});
		createAgentSessionRuntime.mockImplementation(async (factory, options) => {
			const result = await factory({
				cwd: options.cwd,
				agentDir: options.agentDir,
				sessionManager: options.sessionManager,
			});
			return {
				session: result.session,
				services: result.services,
				diagnostics: result.diagnostics,
				switchSession: vi.fn(),
				newSession: vi.fn(),
				fork: vi.fn(),
				importFromJsonl: vi.fn(),
				dispose: runtimeDispose,
			};
		});
	});

	it("stops the previous shadow controller before rebuilding the runtime state", async () => {
		const { createRelayRuntimeFactory } = await import("../src/runtime.js");
		const firstController = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => undefined),
		};
		const secondController = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => undefined),
		};
		const bridgeFactory = vi
			.fn()
			.mockResolvedValueOnce(firstController)
			.mockResolvedValueOnce(secondController);
		const stateRef = {};
		const factory = createRelayRuntimeFactory("/tmp/agent", stateRef as never, {
			env: {
				PI_RELAY_ORCH_ENGINE: "rust-shadow",
			},
			orchestratorBridgeFactory: bridgeFactory,
		});

		await factory({
			cwd: "/tmp/project",
			sessionManager: {
				getSessionDir: () => "/tmp/sessions",
			},
		} as never);
		await factory({
			cwd: "/tmp/project",
			sessionManager: {
				getSessionDir: () => "/tmp/sessions",
			},
		} as never);

		expect(firstController.start).toHaveBeenCalledTimes(1);
		expect(firstController.stop).toHaveBeenCalledTimes(1);
		expect(secondController.start).toHaveBeenCalledTimes(1);
		expect(bridgeFactory).toHaveBeenCalledTimes(2);
		expect(stateRef.current?.orchestratorController?.shadowActive).toBe(true);
	});

	it("continues rebuilding when the previous shadow controller fails to stop", async () => {
		const { createRelayRuntimeFactory } = await import("../src/runtime.js");
		const firstController = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => {
				throw new Error("stop failed during rebuild");
			}),
		};
		const secondController = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => undefined),
		};
		const bridgeFactory = vi
			.fn()
			.mockResolvedValueOnce(firstController)
			.mockResolvedValueOnce(secondController);
		const stateRef: {
			current?: {
				orchestratorController?: {
					shadowActive?: boolean;
				};
			};
		} = {};

		const factory = createRelayRuntimeFactory("/tmp/agent", stateRef as never, {
			env: {
				PI_RELAY_ORCH_ENGINE: "rust-shadow",
			},
			orchestratorBridgeFactory: bridgeFactory,
		});

		const firstResult = await factory({
			cwd: "/tmp/project",
			sessionManager: {
				getSessionDir: () => "/tmp/sessions",
			},
		} as never);

		await expect(
			factory({
				cwd: "/tmp/project",
				sessionManager: {
					getSessionDir: () => "/tmp/sessions",
				},
			} as never),
		).resolves.toMatchObject({
			services: expect.any(Object),
		});

		expect(firstController.stop).toHaveBeenCalledTimes(1);
		expect(secondController.start).toHaveBeenCalledTimes(1);
		expect(firstResult.diagnostics).toContainEqual({
			type: "warning",
			message:
				"Failed to stop the Rust orchestrator bridge cleanly for PI_RELAY_ORCH_ENGINE=rust-shadow: stop failed during rebuild. TypeScript remains authoritative.",
		});
		expect(stateRef.current?.orchestratorController?.shadowActive).toBe(true);
	});

	it("stops the active shadow controller when the runtime is disposed", async () => {
		const { createRelayRuntime } = await import("../src/runtime.js");
		const controller = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => undefined),
		};

		const runtime = await createRelayRuntime({
			cwd: "/tmp/project",
			agentDir: "/tmp/agent",
			env: {
				PI_RELAY_ORCH_ENGINE: "rust-shadow",
			},
			orchestratorBridgeFactory: async () => controller,
		});

		await runtime.dispose();
		await runtime.dispose();

		expect(controller.start).toHaveBeenCalledTimes(1);
		expect(controller.stop).toHaveBeenCalledTimes(1);
		expect(runtimeDispose).toHaveBeenCalledTimes(1);
	});

	it("still disposes the authoritative runtime when shadow controller stop fails", async () => {
		const { createRelayRuntime } = await import("../src/runtime.js");
		const controller = {
			start: vi.fn(async () => undefined),
			flush: vi.fn(async () => undefined),
			stop: vi.fn(async () => {
				throw new Error("stop failed during dispose");
			}),
		};

		const runtime = await createRelayRuntime({
			cwd: "/tmp/project",
			agentDir: "/tmp/agent",
			env: {
				PI_RELAY_ORCH_ENGINE: "rust-shadow",
			},
			orchestratorBridgeFactory: async () => controller,
		});

		await expect(runtime.dispose()).resolves.toBeUndefined();

		expect(controller.stop).toHaveBeenCalledTimes(1);
		expect(runtimeDispose).toHaveBeenCalledTimes(1);
		expect(runtime.diagnostics).toContainEqual({
			type: "warning",
			message:
				"Failed to stop the Rust orchestrator bridge cleanly for PI_RELAY_ORCH_ENGINE=rust-shadow: stop failed during dispose. TypeScript remains authoritative.",
		});
	});
});
