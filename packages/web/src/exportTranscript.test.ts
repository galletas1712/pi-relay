import { describe, expect, it } from "vitest";
import { buildExportBlocks, defaultSelectedAssistantIds, formatExportMarkdown } from "./exportTranscript.ts";
import type { AssistantItem, TranscriptEntry, TranscriptItem } from "./types.ts";

describe("export transcript", () => {
	it("exports every user input from the containing turn once for a selected final answer", () => {
		const entries = [
			entry("start", { type: "turn_started", turn_id: 1 }),
			entry("user-1", user("first request")),
			entry("assistant-progress", { type: "assistant_message", items: [text("I will inspect."), toolCall()] }),
			entry("tool-result", { type: "tool_result", tool_call_id: "call_1", tool_name: "bash", output: "ok", status: "Success" }),
			entry("user-2", user("also check tests")),
			entry("assistant-final", { type: "assistant_message", items: [text("Final answer.")] }),
			entry("finish", { type: "turn_finished", turn_id: 1, outcome: "Graceful" })
		];

		const blocks = buildExportBlocks(entries);
		const selected = new Set(["assistant-final"]);
		const markdown = formatExportMarkdown(blocks, selected);

		expect(markdown).toContain("## User\n\nfirst request");
		expect(markdown).toContain("## User\n\nalso check tests");
		expect(markdown).toContain("## Assistant\n\nFinal answer.");
		expect(markdown).not.toContain("I will inspect.");
	});

	it("selects final answers by default instead of progress/tool-request steps", () => {
		const entries = [
			entry("start", { type: "turn_started", turn_id: 2 }),
			entry("user", user("run command")),
			entry("assistant-progress", { type: "assistant_message", items: [text("Running command."), toolCall()] }),
			entry("tool-result", { type: "tool_result", tool_call_id: "call_1", tool_name: "bash", output: "ok", status: "Success" }),
			entry("assistant-final", { type: "assistant_message", items: [text("Done.")] }),
			entry("finish", { type: "turn_finished", turn_id: 2, outcome: "Graceful" })
		];

		const selected = defaultSelectedAssistantIds(buildExportBlocks(entries));

		expect([...selected]).toEqual(["assistant-final"]);
	});

	it("omits compaction replay rows while preserving genuine identical user inputs", () => {
		const entries = [
			entry("start", { type: "turn_started", turn_id: 3 }),
			entry("original", user("same request")),
			entry("replayed", {
				type: "user_message",
				content: [{ type: "text", text: "same request" }],
				replayed_after_compaction: true,
			}),
			entry("genuine", user("same request")),
			entry("assistant-final", { type: "assistant_message", items: [text("Done.")] }),
			entry("finish", { type: "turn_finished", turn_id: 3, outcome: "Graceful" }),
		];

		const blocks = buildExportBlocks(entries);

		expect(blocks.filter((block) => block.type === "user").map((block) => block.entryId)).toEqual([
			"original",
			"genuine",
		]);
		expect(blocks.find((block) => block.type === "assistant")).toMatchObject({
			priorUserEntryIds: ["original", "genuine"],
		});
	});
});

function user(text: string): TranscriptItem {
	return { type: "user_message", content: [{ type: "text", text }] };
}

function text(value: string): AssistantItem {
	return { type: "text", text: value };
}

function toolCall(): AssistantItem {
	return { type: "tool_call", id: "call_1", tool_name: "bash", args_json: "{\"command\":\"ls\"}" };
}

function entry(id: string, item: TranscriptItem): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 1,
		item
	};
}
