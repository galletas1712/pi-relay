import { beforeEach, describe, expect, it, vi } from "vitest";

const createAgentSessionFromServices = vi.fn();
const createSessionManager = vi.fn();
const openSessionManager = vi.fn();

vi.mock("@pi-relay/coding-agent", () => ({
	createAgentSessionFromServices,
	SessionManager: {
		create: createSessionManager,
		open: openSessionManager,
	},
}));

describe("createRelaySessionFactory", () => {
	beforeEach(() => {
		createAgentSessionFromServices.mockReset();
		createSessionManager.mockReset();
		openSessionManager.mockReset();

		createAgentSessionFromServices.mockResolvedValue({
			session: { sessionId: "child-session" },
		});
		createSessionManager.mockReturnValue({ kind: "create-session-manager" });
		openSessionManager.mockReturnValue({ kind: "open-session-manager" });
	});

	it("recreates child sessions from explicit base tool names instead of builtin provenance", async () => {
		const { createRelaySessionFactory } = await import("../src/session-factory.js");
		const childBaseToolDefinitionsFactory = vi.fn(() => [{ name: "read" }, { name: "bash" }]);
		const createSessionBaseToolDefinitionsFactory = vi.fn(() => childBaseToolDefinitionsFactory);
		const factory = createRelaySessionFactory({
			services: { cwd: "/tmp/project" } as never,
			defaultSessionDir: "/tmp/sessions",
			baseToolNames: ["read", "bash", "edit"],
			createSessionBaseToolDefinitionsFactory,
		});

		await factory({
			mode: "spawn",
			agentId: "child-agent",
			parentId: "root-agent",
			config: {
				role: "planner",
				prompt: "Investigate the issue",
				tools: ["bash", "edit"],
			},
			customTools: [{ name: "spawn" } as never],
			parentSession: {
				agent: {
					state: {
						tools: [{ name: "write" }, { name: "edit" }, { name: "bash" }, { name: "read" }],
					},
				},
				model: { id: "claude-sonnet-4-5" },
				thinkingLevel: "high",
				sessionFile: "/tmp/root.jsonl",
			} as never,
		});

		expect(createSessionManager).toHaveBeenCalledWith("/tmp/project", "/tmp/sessions");
		expect(createSessionBaseToolDefinitionsFactory).toHaveBeenCalledTimes(1);
		expect(createAgentSessionFromServices).toHaveBeenCalledWith(
			expect.objectContaining({
				toolNames: ["edit", "bash"],
				baseToolDefinitionsFactory: childBaseToolDefinitionsFactory,
			}),
		);
	});

	it("opens restore sessions and carries forward all active base tools when no override is provided", async () => {
		const { createRelaySessionFactory } = await import("../src/session-factory.js");
		const childBaseToolDefinitionsFactory = vi.fn(() => [{ name: "read" }]);
		const createSessionBaseToolDefinitionsFactory = vi.fn(() => childBaseToolDefinitionsFactory);
		const factory = createRelaySessionFactory({
			services: { cwd: "/tmp/project" } as never,
			defaultSessionDir: "/tmp/sessions",
			baseToolNames: ["read", "bash", "edit"],
			createSessionBaseToolDefinitionsFactory,
		});

		await factory({
			mode: "restore",
			agentId: "child-agent",
			parentId: "root-agent",
			sessionFile: "/tmp/child.jsonl",
			config: {
				role: "planner",
				prompt: "Investigate the issue",
			},
			customTools: [],
			parentSession: {
				agent: {
					state: {
						tools: [{ name: "read" }, { name: "write" }, { name: "edit" }],
					},
				},
				model: { id: "claude-sonnet-4-5" },
				thinkingLevel: "medium",
				sessionFile: "/tmp/root.jsonl",
			} as never,
		});

		expect(openSessionManager).toHaveBeenCalledWith("/tmp/child.jsonl", "/tmp/sessions");
		expect(createAgentSessionFromServices).toHaveBeenCalledWith(
			expect.objectContaining({
				toolNames: ["read", "edit"],
				baseToolDefinitionsFactory: childBaseToolDefinitionsFactory,
			}),
		);
	});
});
