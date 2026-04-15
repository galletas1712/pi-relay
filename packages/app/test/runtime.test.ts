import { beforeEach, describe, expect, it, vi } from "vitest";

const continueRecent = vi.fn();
const createAgentSessionRuntime = vi.fn();
const createAgentSessionServices = vi.fn();
const createAgentSessionFromServices = vi.fn();
const getAgentDir = vi.fn(() => "/tmp/agent");
const readTool = { name: "read" };
const bashTool = { name: "bash" };
const editTool = { name: "edit" };
const applyPatchTool = { name: "apply_patch" };
const writeTool = { name: "write" };
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
	getAgentDir,
	readTool,
	bashTool,
	editTool,
	applyPatchTool,
	writeTool,
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
				tools: [readTool, bashTool, editTool, applyPatchTool, writeTool],
			}),
		);
		expect(restore).toHaveBeenCalledTimes(1);
	});
});
