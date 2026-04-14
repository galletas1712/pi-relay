import { beforeEach, describe, expect, it, vi } from "vitest";
import { FakeSession } from "./test-helpers.js";

const createAgentSessionFromServices = vi.fn(async () => ({ session: { id: "child-session" } }));
const create = vi.fn();
const open = vi.fn();

vi.mock("@mariozechner/pi-coding-agent", () => ({
	createAgentSessionFromServices,
	SessionManager: {
		create,
		open,
	},
}));

describe("createRelaySessionFactory", () => {
	beforeEach(() => {
		vi.resetModules();
		createAgentSessionFromServices.mockClear();
		create.mockReset();
		open.mockReset();
	});

	it("persists a spawned child session header before the child starts running", async () => {
		const ensurePersisted = vi.fn();
		create.mockReturnValue({
			ensurePersisted,
		});

		const { createRelaySessionFactory } = await import("../src/session-factory.js");
		const parentSession = new FakeSession("root-session");
		const factory = createRelaySessionFactory({
			services: { cwd: "/tmp/project" } as never,
			defaultSessionDir: "/tmp/sessions",
		});

		await factory({
			mode: "spawn",
			agentId: "child",
			parentId: "root",
			config: {
				role: "explorer",
				prompt: "inspect src",
			},
			customTools: [],
			parentSession,
			sessionDir: "/tmp/agents",
		});

		expect(create).toHaveBeenCalledWith("/tmp/project", "/tmp/agents");
		expect(ensurePersisted).toHaveBeenCalledTimes(1);
	});
});
