import { describe, expect, it, vi } from "vitest";
import { createMessageTool } from "../src/tools/message.js";
import { createReportTool } from "../src/tools/report.js";
import { createSpawnTool } from "../src/tools/spawn.js";

describe("orchestration tools", () => {
	it("spawn returns the new agent id", async () => {
		const runtime = {
			spawnAgent: vi.fn(async () => "child-1234"),
		};
		const tool = createSpawnTool(runtime, "root");
		const result = await tool.execute("tool-1", { role: "explore", prompt: "look around" }, undefined, undefined, {} as never);
		expect(runtime.spawnAgent).toHaveBeenCalledWith("root", {
			role: "explore",
			prompt: "look around",
			tools: undefined,
		});
		expect(result.content[0]?.type).toBe("text");
	});

	it("message delivers to one or many children", async () => {
		const runtime = {
			routeMessage: vi.fn(async () => {}),
		};
		const tool = createMessageTool(runtime, "root");
		await tool.execute("tool-1", { to: "child-a", content: "focus" }, undefined, undefined, {} as never);
		await tool.execute("tool-2", { to: ["child-a", "child-b"], content: "status?" }, undefined, undefined, {} as never);
		expect(runtime.routeMessage).toHaveBeenNthCalledWith(1, "root", "child-a", "focus");
		expect(runtime.routeMessage).toHaveBeenNthCalledWith(2, "root", "child-a", "status?");
		expect(runtime.routeMessage).toHaveBeenNthCalledWith(3, "root", "child-b", "status?");
	});

	it("report sends progress to the parent", async () => {
		const runtime = {
			handleReport: vi.fn(async () => {}),
		};
		const tool = createReportTool(runtime, "child-a");
		const result = await tool.execute("tool-1", { content: "found it" }, undefined, undefined, {} as never);
		expect(runtime.handleReport).toHaveBeenCalledWith("child-a", "found it");
		expect(result.content[0]?.type).toBe("text");
	});
});
