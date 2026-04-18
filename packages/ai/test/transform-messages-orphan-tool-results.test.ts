import { describe, expect, it } from "vitest";
import { transformMessages } from "../src/providers/transform-messages.js";
import type { AssistantMessage, Message, Model, ToolResultMessage } from "../src/types.js";

function makeAnthropicModel(): Model<"anthropic-messages"> {
	return {
		id: "claude-opus-4-7",
		name: "Claude Opus 4.7",
		api: "anthropic-messages",
		provider: "anthropic",
		baseUrl: "https://api.anthropic.com",
		reasoning: true,
		input: ["text"],
		cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0 },
		contextWindow: 200000,
		maxTokens: 64000,
	};
}

function makeAssistant(content: AssistantMessage["content"], stopReason: AssistantMessage["stopReason"]): AssistantMessage {
	return {
		role: "assistant",
		content,
		api: "anthropic-messages",
		provider: "anthropic",
		model: "claude-opus-4-7",
		usage: {
			input: 0,
			output: 0,
			cacheRead: 0,
			cacheWrite: 0,
			totalTokens: 0,
			cost: { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 },
		},
		stopReason,
		timestamp: Date.now(),
	};
}

function toolResult(id: string, text: string): ToolResultMessage {
	return {
		role: "toolResult",
		toolCallId: id,
		toolName: "bash",
		content: [{ type: "text", text }],
		timestamp: Date.now(),
	} as ToolResultMessage;
}

describe("transformMessages drops orphan tool results", () => {
	it("drops a toolResult whose matching assistant was aborted mid-stream", () => {
		// Repro of the 400 "unexpected tool_use_id": assistant finalized a tool_use
		// then the stream aborted; orchestrator.restore() later appended a synthetic
		// toolResult. We drop the aborted assistant but must also drop that orphan.
		const messages: Message[] = [
			{ role: "user", content: "run ls", timestamp: Date.now() },
			makeAssistant(
				[{ type: "toolCall", id: "toolu_abort1", name: "bash", arguments: { cmd: "ls" } }],
				"aborted",
			),
			toolResult("toolu_abort1", "[INTERRUPTED] bash did not produce a result before the session ended."),
			{ role: "user", content: "follow up", timestamp: Date.now() },
		];

		const result = transformMessages(messages, makeAnthropicModel());

		expect(result.some((m) => m.role === "assistant")).toBe(false);
		expect(result.some((m) => m.role === "toolResult" && m.toolCallId === "toolu_abort1")).toBe(false);
		expect(result.filter((m) => m.role === "user")).toHaveLength(2);
	});

	it("drops a toolResult whose matching assistant was marked error", () => {
		const messages: Message[] = [
			{ role: "user", content: "run ls", timestamp: Date.now() },
			makeAssistant(
				[{ type: "toolCall", id: "toolu_err1", name: "bash", arguments: { cmd: "ls" } }],
				"error",
			),
			toolResult("toolu_err1", "irrelevant"),
			{ role: "user", content: "follow up", timestamp: Date.now() },
		];

		const result = transformMessages(messages, makeAnthropicModel());

		expect(result.some((m) => m.role === "toolResult" && m.toolCallId === "toolu_err1")).toBe(false);
	});

	it("keeps toolResult when its assistant was stopped normally", () => {
		const messages: Message[] = [
			{ role: "user", content: "run ls", timestamp: Date.now() },
			makeAssistant(
				[{ type: "toolCall", id: "toolu_ok1", name: "bash", arguments: { cmd: "ls" } }],
				"tool_use",
			),
			toolResult("toolu_ok1", "file1\nfile2"),
			{ role: "user", content: "thanks", timestamp: Date.now() },
		];

		const result = transformMessages(messages, makeAnthropicModel());

		expect(result.some((m) => m.role === "assistant")).toBe(true);
		const kept = result.find((m) => m.role === "toolResult" && m.toolCallId === "toolu_ok1") as
			| ToolResultMessage
			| undefined;
		expect(kept).toBeDefined();
		expect((kept?.content[0] as { text: string }).text).toBe("file1\nfile2");
	});

	it("drops a toolResult whose tool_use never appeared in the message log at all", () => {
		// Defensive: guards against session drift where a toolResult ends up in
		// history with no preceding assistant tool_use of that id.
		const messages: Message[] = [
			{ role: "user", content: "hi", timestamp: Date.now() },
			toolResult("toolu_phantom", "orphan with no matching assistant"),
			{ role: "user", content: "still here?", timestamp: Date.now() },
		];

		const result = transformMessages(messages, makeAnthropicModel());

		expect(result.some((m) => m.role === "toolResult")).toBe(false);
		expect(result.filter((m) => m.role === "user")).toHaveLength(2);
	});

	it("still inserts synthetic results for orphan tool_calls that were kept", () => {
		// Previous behavior must still hold: kept assistant with an unanswered
		// tool_use gets a synthetic "No result provided" result before the next turn.
		const messages: Message[] = [
			{ role: "user", content: "run ls", timestamp: Date.now() },
			makeAssistant(
				[{ type: "toolCall", id: "toolu_unanswered", name: "bash", arguments: { cmd: "ls" } }],
				"tool_use",
			),
			{ role: "user", content: "never mind", timestamp: Date.now() },
		];

		const result = transformMessages(messages, makeAnthropicModel());

		const synthetic = result.find((m) => m.role === "toolResult" && m.toolCallId === "toolu_unanswered") as
			| ToolResultMessage
			| undefined;
		expect(synthetic).toBeDefined();
		expect(synthetic?.isError).toBe(true);
	});
});
