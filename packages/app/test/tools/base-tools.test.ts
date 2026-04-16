import { beforeEach, describe, expect, it, vi } from "vitest";

const createApplyPatchToolDefinition = vi.fn(() => ({ name: "apply_patch" }));
const createBashToolDefinition = vi.fn(() => ({ name: "bash" }));
const createEditToolDefinition = vi.fn(() => ({ name: "edit" }));
const createFileAccessTracker = vi.fn(() => ({ kind: "tracker" }));
const createReadToolDefinition = vi.fn(() => ({ name: "read" }));
const createWriteToolDefinition = vi.fn(() => ({ name: "write" }));

vi.mock("@mariozechner/pi-coding-agent", () => ({
	createApplyPatchToolDefinition,
	createBashToolDefinition,
	createEditToolDefinition,
	createFileAccessTracker,
	createReadToolDefinition,
	createWriteToolDefinition,
}));

describe("relay base tools", () => {
	beforeEach(() => {
		createApplyPatchToolDefinition.mockClear();
		createBashToolDefinition.mockClear();
		createEditToolDefinition.mockClear();
		createFileAccessTracker.mockClear();
		createReadToolDefinition.mockClear();
		createWriteToolDefinition.mockClear();
	});

	it("creates the relay base tool bundle from one shared tracker", async () => {
		const { RELAY_BASE_TOOL_NAMES, createRelayBaseToolDefinitionsFactory } = await import(
			"../../src/tools/base-tools.js"
		);
		const settingsManager = {
			getImageAutoResize: vi.fn(() => true),
			getShellCommandPrefix: vi.fn(() => ["direnv", "exec", ".", "--"]),
		};

		const factory = createRelayBaseToolDefinitionsFactory("/tmp/project", settingsManager as never);
		const definitions = factory();

		expect(RELAY_BASE_TOOL_NAMES).toEqual(["read", "bash", "edit", "apply_patch", "write"]);
		expect(definitions.map((definition) => definition.name)).toEqual([...RELAY_BASE_TOOL_NAMES]);
		expect(createFileAccessTracker).toHaveBeenCalledTimes(1);
		expect(createReadToolDefinition).toHaveBeenCalledWith(
			"/tmp/project",
			expect.objectContaining({ autoResizeImages: true, tracker: { kind: "tracker" } }),
		);
		expect(createBashToolDefinition).toHaveBeenCalledWith(
			"/tmp/project",
			expect.objectContaining({ commandPrefix: ["direnv", "exec", ".", "--"] }),
		);
		expect(createEditToolDefinition).toHaveBeenCalledWith(
			"/tmp/project",
			expect.objectContaining({ tracker: { kind: "tracker" } }),
		);
		expect(createApplyPatchToolDefinition).toHaveBeenCalledWith(
			"/tmp/project",
			expect.objectContaining({ tracker: { kind: "tracker" } }),
		);
		expect(createWriteToolDefinition).toHaveBeenCalledWith(
			"/tmp/project",
			expect.objectContaining({ tracker: { kind: "tracker" } }),
		);
	});

	it("re-reads settings each time the bundle is rebuilt", async () => {
		const { createRelayBaseToolDefinitionsFactory } = await import("../../src/tools/base-tools.js");
		const settingsManager = {
			getImageAutoResize: vi.fn().mockReturnValueOnce(true).mockReturnValueOnce(false),
			getShellCommandPrefix: vi
				.fn()
				.mockReturnValueOnce(["direnv", "exec", ".", "--"])
				.mockReturnValueOnce(["mise", "x", "--"]),
		};

		const factory = createRelayBaseToolDefinitionsFactory("/tmp/project", settingsManager as never);
		factory();
		factory();

		expect(createFileAccessTracker).toHaveBeenCalledTimes(1);
		expect(createReadToolDefinition).toHaveBeenNthCalledWith(
			1,
			"/tmp/project",
			expect.objectContaining({ autoResizeImages: true }),
		);
		expect(createReadToolDefinition).toHaveBeenNthCalledWith(
			2,
			"/tmp/project",
			expect.objectContaining({ autoResizeImages: false }),
		);
		expect(createBashToolDefinition).toHaveBeenNthCalledWith(
			1,
			"/tmp/project",
			expect.objectContaining({ commandPrefix: ["direnv", "exec", ".", "--"] }),
		);
		expect(createBashToolDefinition).toHaveBeenNthCalledWith(
			2,
			"/tmp/project",
			expect.objectContaining({ commandPrefix: ["mise", "x", "--"] }),
		);
	});
});
