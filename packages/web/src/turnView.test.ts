import { describe, expect, it } from "vitest";
import { assistantMessageText, buildTurnViews, terminalModelStep } from "./turnView.ts";
import type { AssistantItem, TranscriptEntry, TranscriptItem } from "./types.ts";

describe("buildTurnViews", () => {
	it("derives tool-request and final-answer model steps inside one turn", () => {
		const toolCall = { type: "tool_call" as const, id: "call_1", tool_name: "bash", args_json: "{\"command\":\"ls\"}" };
		const entries = [
			entry("start", { type: "turn_started", turn_id: 1 }),
			entry("user", { type: "user_message", content: [{ type: "text", text: "inspect" }] }),
			entry("assistant-tools", { type: "assistant_message", items: [text("I will inspect."), toolCall] }),
			entry("tool-start", { type: "tool_call_started", turn_id: 1, tool_call: toolCall }),
			entry("tool-result", { type: "tool_result", tool_call_id: "call_1", tool_name: "bash", output: "ok", status: "Success" }),
			entry("assistant-final", { type: "assistant_message", items: [text("Done.")] }),
			entry("finish", { type: "turn_finished", turn_id: 1, outcome: "Graceful" })
		];

		const [turn] = buildTurnViews(entries);

		expect(turn.turnId).toBe(1);
		expect(turn.userInputs.map((input) => input.id)).toEqual(["user"]);
		expect(turn.modelSteps.map((step) => [step.entry.id, step.phase])).toEqual([
			["assistant-tools", "tool_request"],
			["assistant-final", "final_answer"]
		]);
		expect(turn.modelSteps[0].toolResults.map((result) => result.id)).toEqual(["tool-result"]);
		expect(terminalModelStep(turn)?.entry.id).toBe("assistant-final");
	});

	it("keeps multiple user inputs in the containing turn instead of pairing nearest user", () => {
		const entries = [
			entry("start", { type: "turn_started", turn_id: 2 }),
			entry("user-1", { type: "user_message", content: [{ type: "text", text: "first" }] }),
			entry("user-2", { type: "user_message", content: [{ type: "text", text: "steer" }] }),
			entry("assistant-final", { type: "assistant_message", items: [text("answer")] }),
			entry("finish", { type: "turn_finished", turn_id: 2, outcome: "Graceful" })
		];

		const [turn] = buildTurnViews(entries);

		expect(turn.userInputs.map((input) => input.id)).toEqual(["user-1", "user-2"]);
		expect(turn.modelSteps).toHaveLength(1);
		expect(turn.modelSteps[0].phase).toBe("final_answer");
	});

	it("keeps mid-turn compaction inside the containing turn", () => {
		const entries = [
			entry("start", { type: "turn_started", turn_id: 7 }),
			entry("user", { type: "user_message", content: [{ type: "text", text: "first" }] }),
			entry("assistant-before-compact", { type: "assistant_message", items: [text("I will keep going.")] }),
			entry("compact", {
				type: "compaction_summary",
				source_session_id: "session",
				source_leaf_id: "assistant-before-compact",
				summary: "summary",
				tokens_before: 123,
				last_turn_id: 7,
				turn_started_at_ms: 1_700_000_000_000
			}),
			entry("assistant-final", { type: "assistant_message", items: [text("Done.")] }),
			entry("finish", { type: "turn_finished", turn_id: 7, outcome: "Graceful" })
		];

		const turns = buildTurnViews(entries);

		expect(turns).toHaveLength(1);
		const [turn] = turns;
		expect(turn.turnId).toBe(7);
		expect(turn.entries.map((candidate) => candidate.id)).toEqual([
			"start",
			"user",
			"assistant-before-compact",
			"assistant-final",
			"finish"
		]);
		expect(turn.modelSteps.map((step) => [step.entry.id, step.phase])).toEqual([
			["assistant-before-compact", "unknown"],
			["assistant-final", "final_answer"]
		]);
	});

	it("marks terminal assistant text in crashed turns as aborted", () => {
		const entries = [
			entry("start", { type: "turn_started", turn_id: 3 }),
			entry("user", { type: "user_message", content: [{ type: "text", text: "do it" }] }),
			entry("assistant", { type: "assistant_message", items: [text("partial")] }),
			entry("finish", { type: "turn_finished", turn_id: 3, outcome: "Crashed" })
		];

		const [turn] = buildTurnViews(entries);

		expect(turn.outcome).toBe("Crashed");
		expect(turn.modelSteps[0].phase).toBe("aborted");
	});

	it("does not inject paragraph breaks between adjacent provider text blocks", () => {
		const item: Extract<TranscriptItem, { type: "assistant_message" }> = {
			type: "assistant_message",
			items: [text("describing it as "), text("OpenAI's interface"), text(", along with the endpoint.")]
		};

		expect(assistantMessageText(item)).toBe("describing it as OpenAI's interface, along with the endpoint.");
	});
});

function text(value: string): AssistantItem {
	return { type: "text", text: value };
}

function entry(id: string, item: TranscriptItem): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 1,
		item
	};
}
