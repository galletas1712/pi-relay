import { beforeEach, describe, expect, it, vi } from "vitest";

const getModel = vi.fn();
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
const rootBaseToolDefinitionsFactory = vi.fn(() => [{ name: "read" }]);
const createRelayBaseToolDefinitionsFactory = vi.fn(() => rootBaseToolDefinitionsFactory);
const RELAY_BASE_TOOL_NAMES = ["read", "bash", "edit", "apply_patch", "write"];
const restore = vi.fn(async () => false);

vi.mock("@pi-relay/ai", () => ({
	getModel,
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
	RELAY_BASE_TOOL_NAMES,
	createRelayBaseToolDefinitionsFactory,
}));

describe("createRelayRuntime", () => {
	beforeEach(() => {
		vi.resetModules();
		getModel.mockClear();
		create.mockClear();
		continueRecent.mockClear();
		createAgentSessionRuntime.mockClear();
		createAgentSessionServices.mockClear();
		createAgentSessionFromServices.mockClear();
		createOrchestratorExtension.mockClear();
		createRelaySessionFactory.mockClear();
		createRelayBaseToolDefinitionsFactory.mockClear();
		createSpawnTool.mockClear();
		createMessageTool.mockClear();
		rootBaseToolDefinitionsFactory.mockClear();
		restore.mockClear();

		getModel.mockReturnValue(undefined);
		create.mockReturnValue({
			getSessionDir: () => "/tmp/sessions",
		});
		continueRecent.mockReturnValue({
			getSessionDir: () => "/tmp/sessions",
		});
		createAgentSessionServices.mockResolvedValue({
			diagnostics: [],
			settingsManager: { marker: "settings-manager" },
		});
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
			};
		});
	});

	it("continues the recent session and restores the orchestrator tree before returning", async () => {
		const { createRelayRuntime } = await import("../src/runtime.js");

		await createRelayRuntime({
			cwd: "/tmp/project",
			agentDir: "/tmp/agent",
		});

		expect(continueRecent).toHaveBeenCalledWith("/tmp/project");
		expect(createAgentSessionRuntime).toHaveBeenCalledTimes(1);
		expect(createAgentSessionServices).toHaveBeenCalledWith(
			expect.objectContaining({
				cwd: "/tmp/project",
				agentDir: "/tmp/agent",
				resourceLoaderOptions: expect.objectContaining({
					appendSystemPrompt: [
						expect.stringContaining("Use apply_patch for multi-file or diff-shaped changes to existing files."),
					],
				}),
			}),
		);
		expect(createAgentSessionFromServices).toHaveBeenCalledWith(
			expect.objectContaining({
				toolNames: RELAY_BASE_TOOL_NAMES,
				baseToolDefinitionsFactory: rootBaseToolDefinitionsFactory,
			}),
		);
		expect(createRelaySessionFactory).toHaveBeenCalledWith(
			expect.objectContaining({
				baseToolNames: RELAY_BASE_TOOL_NAMES,
				createSessionBaseToolDefinitionsFactory: expect.any(Function),
			}),
		);
		expect(createRelayBaseToolDefinitionsFactory).toHaveBeenCalledWith("/tmp/project", { marker: "settings-manager" });
		const relayFactoryArgs = createRelaySessionFactory.mock.calls[0]?.[0];
		expect(relayFactoryArgs.createSessionBaseToolDefinitionsFactory()).toBe(rootBaseToolDefinitionsFactory);
		expect(restore).toHaveBeenCalledTimes(1);
	});

	it("starts a fresh session for interactive runtime startup", async () => {
		const { createRelayInteractiveRuntime } = await import("../src/runtime.js");

		await createRelayInteractiveRuntime({
			cwd: "/tmp/project",
			agentDir: "/tmp/agent",
		});

		expect(create).toHaveBeenCalledWith("/tmp/project");
		expect(continueRecent).not.toHaveBeenCalled();
		expect(createAgentSessionRuntime).toHaveBeenCalledTimes(1);
	});

	it("records the selected engine modes on the runtime state ref", async () => {
		const { createRelayRuntimeFactory } = await import("../src/runtime.js");
		const stateRef: {
			current?: {
				engineConfig?: {
					orchestrator: string;
					session: string;
				};
			};
		} = {};
		const factory = createRelayRuntimeFactory("/tmp/agent", stateRef as never);

		await factory({
			cwd: "/tmp/project",
			sessionManager: {
				getSessionDir: () => "/tmp/sessions",
			},
		} as never);

		expect(stateRef.current?.engineConfig).toEqual({
			orchestrator: "legacy",
			session: "legacy",
		});
	});
});

describe("resolveRelayRuntimeEngineConfig", () => {
	it("defaults both engines to legacy when the env is unset", async () => {
		const { resolveRelayRuntimeEngineConfig } = await import("../src/runtime.js");

		expect(resolveRelayRuntimeEngineConfig({})).toEqual({
			orchestrator: "legacy",
			session: "legacy",
		});
	});

	it("accepts the planned migration engine modes", async () => {
		const { resolveRelayRuntimeEngineConfig } = await import("../src/runtime.js");

		expect(
			resolveRelayRuntimeEngineConfig({
				PI_RELAY_ORCH_ENGINE: "rust-shadow",
				PI_RELAY_SESSION_ENGINE: "ts-core",
			}),
		).toEqual({
			orchestrator: "rust-shadow",
			session: "ts-core",
		});
	});

	it("rejects unknown engine mode values early", async () => {
		const { resolveRelayRuntimeEngineConfig } = await import("../src/runtime.js");

		expect(() =>
			resolveRelayRuntimeEngineConfig({
				PI_RELAY_ORCH_ENGINE: "future-experiment",
			}),
		).toThrow(/Invalid PI_RELAY_ORCH_ENGINE/);
	});
});
