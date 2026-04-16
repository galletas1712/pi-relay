import { beforeEach, describe, expect, it, vi } from "vitest";

const continueRecent = vi.fn();
const createAgentSessionRuntime = vi.fn();
const createAgentSessionServices = vi.fn();
const createAgentSessionFromServices = vi.fn();
const createApplyPatchToolDefinition = vi.fn(() => ({ name: "apply_patch" }));
const createBashToolDefinition = vi.fn(() => ({ name: "bash" }));
const createEditToolDefinition = vi.fn(() => ({ name: "edit" }));
const createFileAccessTracker = vi.fn(() => ({ kind: "tracker" }));
const createReadToolDefinition = vi.fn(() => ({ name: "read" }));
const createWriteToolDefinition = vi.fn(() => ({ name: "write" }));
const getAgentDir = vi.fn(() => "/tmp/agent");
const createOrchestratorExtension = vi.fn(() => "extension-factory");
const createRelaySessionFactory = vi.fn(() => "session-factory");
const createSpawnTool = vi.fn(() => ({ name: "spawn" }));
const createMessageTool = vi.fn(() => ({ name: "message" }));
const restore = vi.fn(async () => false);

vi.mock("@mariozechner/pi-coding-agent", () => ({
	SessionManager: {
		continueRecent,
	},
	createAgentSessionRuntime,
	createAgentSessionServices,
	createAgentSessionFromServices,
	createApplyPatchToolDefinition,
	createBashToolDefinition,
	createEditToolDefinition,
	createFileAccessTracker,
	createReadToolDefinition,
	createWriteToolDefinition,
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

describe("createRelayRuntime", () => {
	beforeEach(() => {
		vi.resetModules();
		continueRecent.mockReset();
		createAgentSessionRuntime.mockReset();
		createAgentSessionServices.mockReset();
		createAgentSessionFromServices.mockReset();
		createOrchestratorExtension.mockReset();
		createRelaySessionFactory.mockReset();
		createSpawnTool.mockReset();
		createMessageTool.mockReset();
		restore.mockClear();

		continueRecent.mockReturnValue({
			getSessionDir: () => "/tmp/sessions",
		});
		createAgentSessionServices.mockResolvedValue({
			diagnostics: [],
			settingsManager: {
				getImageAutoResize: vi.fn(() => true),
				getShellCommandPrefix: vi.fn(() => ["direnv", "exec", ".", "--"]),
			},
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
				toolNames: ["read", "bash", "edit", "apply_patch", "write"],
				baseToolDefinitionsFactory: expect.any(Function),
			}),
		);
		expect(createRelaySessionFactory).toHaveBeenCalledWith(
			expect.objectContaining({
				baseToolNames: ["read", "bash", "edit", "apply_patch", "write"],
				createSessionBaseToolDefinitionsFactory: expect.any(Function),
			}),
		);
		const rootCreateArgs = createAgentSessionFromServices.mock.calls[0]?.[0];
		const rootDefinitions = rootCreateArgs.baseToolDefinitionsFactory();
		expect(createFileAccessTracker).toHaveBeenCalledTimes(1);
		expect(rootDefinitions.map((definition: { name: string }) => definition.name)).toEqual([
			"read",
			"bash",
			"edit",
			"apply_patch",
			"write",
		]);
		expect(createReadToolDefinition).toHaveBeenCalledWith("/tmp/project", expect.objectContaining({ autoResizeImages: true }));
		expect(createBashToolDefinition).toHaveBeenCalledWith(
			"/tmp/project",
			expect.objectContaining({ commandPrefix: ["direnv", "exec", ".", "--"] }),
		);
		expect(restore).toHaveBeenCalledTimes(1);
	});
});
