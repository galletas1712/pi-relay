import { beforeEach, describe, expect, it, vi } from "vitest";

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

vi.mock("@mariozechner/pi-coding-agent", () => ({
	SessionManager: {
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
		restore,
		spawnAgent: vi.fn(),
		routeMessage: vi.fn(),
	})),
}));

vi.mock("../src/tools/base-tools.js", () => ({
	RELAY_BASE_TOOL_NAMES,
	createRelayBaseToolDefinitionsFactory,
}));

describe("createRelayRuntime", () => {
	beforeEach(() => {
		vi.resetModules();
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
});
