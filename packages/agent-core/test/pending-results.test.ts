import { describe, expect, it } from "vitest";
import {
	annotateOrphanedPending,
	bgCompletionToLlmMessage,
	createPendingToolResult,
	formatBackgroundToolCompletion,
} from "../src/pending-results.js";
import type { AgentMessage } from "../src/types.js";

describe("pending-results", () => {
	it("creates a [PENDING] tool result with stable details", () => {
		const message = createPendingToolResult("tool-1", "bash", '{"command":"npm test"}');

		expect(message.role).toBe("toolResult");
		expect(message.toolCallId).toBe("tool-1");
		expect(message.content[0]).toMatchObject({
			type: "text",
		});
		expect((message.content[0] as { text: string }).text).toContain("[PENDING]");
		expect(message.details).toEqual({
			pending: true,
			argsPreview: '{"command":"npm test"}',
		});
	});

	it("formats background completion messages as custom messages", () => {
		const message = formatBackgroundToolCompletion({
			toolCallId: "tool-1",
			toolName: "bash",
			content: [{ type: "text", text: "done" }],
			details: { exitCode: 0 },
			isError: false,
			outputPath: "/tmp/tool.log",
		});

		expect(message.role).toBe("custom");
		expect(message.customType).toBe("bg_tool_completion");
		expect(message.display).toBe(true);
		expect(message.details).toEqual({
			toolCallId: "tool-1",
			toolName: "bash",
			isError: false,
			resultDetails: { exitCode: 0 },
			outputPath: "/tmp/tool.log",
		});
		expect(message.content).toContainEqual({
			type: "text",
			text: "Combined stdout/stderr: /tmp/tool.log",
		});
	});

	it("projects background completion messages to user-role LLM messages", () => {
		const projected = bgCompletionToLlmMessage(
			formatBackgroundToolCompletion({
				toolCallId: "tool-1",
				toolName: "bash",
				content: [{ type: "text", text: "done" }],
				isError: false,
			}),
		);

		expect(projected.role).toBe("user");
		expect(projected.content.some((block) => block.type === "text" && block.text.includes("done"))).toBe(true);
	});

	it("annotates orphaned pending results as interrupted without mutating completed ones", () => {
		const pending = createPendingToolResult("tool-1", "bash", '{"command":"npm test"}');
		const completed = createPendingToolResult("tool-2", "bash", '{"command":"npm lint"}');
		const completion = formatBackgroundToolCompletion({
			toolCallId: "tool-2",
			toolName: "bash",
			content: [{ type: "text", text: "lint finished" }],
			isError: false,
		});

		const annotated = annotateOrphanedPending([pending, completed, completion] satisfies AgentMessage[]);
		const [orphaned, preserved] = annotated;

		expect(orphaned).toMatchObject({
			role: "toolResult",
			toolCallId: "tool-1",
		});
		expect((orphaned as typeof pending).content[0]).toMatchObject({
			type: "text",
		});
		expect(((orphaned as typeof pending).content[0] as { text: string }).text).toContain("[INTERRUPTED]");
		expect(preserved).toBe(completed);
	});
});
