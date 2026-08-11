// M11c: empty-bubble fix — the bridge→legacy adapter must never emit an
// assistant_message entry with no text item (thinking is dropped by the
// projection; tool calls live on their own tool_result entries). Covers both
// the rebuild path (synthesizeEntriesFromBlocks) and the live event path
// (LegacySessionSynthesizer.flushAssistant).
import { describe, expect, it } from "vitest";
import { initialCounters, synthesizeEntriesFromBlocks } from "./legacyTranscript.ts";
import { turnCardSeeMoreEligible } from "../selectedSessionCache/turns.ts";
import { LegacySessionSynthesizer, type LiveSeed } from "./legacyEvents.ts";
import type { ContractEventEnvelope, TranscriptBlock } from "./types.ts";
import type { EventFrame, TranscriptEntry } from "../types.ts";

function messageBlock(id: string, role: "user" | "assistant", text: string, thinking = "") {
	return { kind: "message" as const, id, role, text, thinking, complete: true, ts: "2026-08-10T17:00:00.000Z" };
}

function toolBlock(toolCallId: string, result?: string) {
	return { kind: "tool" as const, toolCallId, toolName: "ipython", args: "{\"code\":\"print(1)\"}", result, done: result !== undefined, ts: "2026-08-10T17:00:01.000Z" };
}

describe("M11c synthesizeEntriesFromBlocks (rebuild path)", () => {
	it("skips textless assistant blocks; their tool blocks still emit tool_result entries", () => {
		const r = synthesizeEntriesFromBlocks(
			"sess",
			[
				messageBlock("u1", "user", "go"),
				messageBlock("a1", "assistant", "\n\n", "plan the call"), // thinking-only after projection
				toolBlock("call_1", "1\n"),
				messageBlock("a2", "assistant", "", ""), // fully empty (bridge would skip; defensive here)
				toolBlock("call_2", "2\n"),
				messageBlock("a3", "assistant", "done"),
			],
			initialCounters(),
			{ closeFinalTurn: true },
		);
		const assistants = r.entries.filter((e) => e.item.type === "assistant_message");
		expect(assistants.map((e) => e.id)).toEqual(["a3"]);
		expect(assistants.every((e) => e.item.type === "assistant_message" && e.item.items.length > 0)).toBe(true);
		const results = r.entries.filter((e) => e.item.type === "tool_result");
		expect(results.map((e) => (e.item as Extract<TranscriptEntry["item"], { type: "tool_result" }>).tool_call_id)).toEqual(["call_1", "call_2"]);
		// orphan results carry the call args so the grouped render shows the input
		expect((results[0].item as Extract<TranscriptEntry["item"], { type: "tool_result" }>).args_json).toBe("{\"code\":\"print(1)\"}");
		// turn card: agent count = text-bearing assistants only (See-more grammar)
		expect(r.cards).toHaveLength(1);
		expect(r.cards[0].assistant_message?.id).toBe("a3");
		expect(r.cards[0].status).toBe("completed");
	});

	it("owner grammar: a turn of 4 tool-calls + 1 text = 1 agent message → no See-more toggle", () => {
		const blocks: TranscriptBlock[] = [messageBlock("u1", "user", "go")];
		for (let i = 1; i <= 4; i++) {
			blocks.push(messageBlock(`a${i}`, "assistant", "", `thinking ${i}`), toolBlock(`call_${i}`, `${i}`));
		}
		blocks.push(messageBlock("a5", "assistant", "done"));
		const r = synthesizeEntriesFromBlocks("sess", blocks, initialCounters(), { closeFinalTurn: true });
		expect(r.entries.filter((e) => e.item.type === "assistant_message")).toHaveLength(1);
		expect(r.cards).toHaveLength(1);
		expect(r.cards[0].agent_message_count).toBe(1);
		expect(turnCardSeeMoreEligible(r.cards[0])).toBe(false);
	});

	it("still folds tool calls into text-bearing assistant entries", () => {
		const r = synthesizeEntriesFromBlocks(
			"sess",
			[messageBlock("u1", "user", "go"), messageBlock("a1", "assistant", "working", "thinks"), toolBlock("call_1", "ok"), messageBlock("a2", "assistant", "done")],
			initialCounters(),
			{ closeFinalTurn: true },
		);
		const a1 = r.entries.find((e) => e.id === "a1");
		expect(a1?.item.type).toBe("assistant_message");
		if (a1?.item.type !== "assistant_message") throw new Error("unreachable");
		expect(a1.item.items.map((i) => i.type)).toEqual(["text", "tool_call"]);
	});
});

function env(seq: number, event: string, data: Record<string, unknown>): ContractEventEnvelope {
	return { event, sessionId: "sess", seq, at: "2026-08-10T17:00:00.000Z", data } as ContractEventEnvelope;
}

function makeSynthesizer(): LegacySessionSynthesizer {
	const seed: LiveSeed = { counters: initialCounters(), leafId: null, turnOpen: false, queued: [] };
	const synth = new LegacySessionSynthesizer("sess", seed, 0);
	synth.registerEcho("go");
	return synth;
}

function appendedEntries(frames: EventFrame[]): TranscriptEntry[] {
	return frames
		.filter((f) => f.event === "transcript.appended" && f.data?.entry)
		.map((f) => f.data.entry as TranscriptEntry);
}

describe("M11c LegacySessionSynthesizer (live path)", () => {
	it("does not create an assistant entry whose text completes empty; tool results still land", () => {
		const synth = makeSynthesizer();
		let seq = 0;
		// user turn opens
		const openFrames = synth.handle(env(++seq, "message.delta", { kind: "start", role: "user" })).frames;
		expect(appendedEntries(openFrames).some((e) => e.item.type === "user_message")).toBe(true);
		// assistant message with a tool call and NO text
		synth.handle(env(++seq, "message.delta", { kind: "start", role: "assistant" }));
		synth.handle(env(++seq, "message.delta", { kind: "toolcall", toolCall: JSON.stringify({ id: "call_1", name: "ipython", arguments: { code: "print(1)" } }) }));
		const endFrames = synth.handle(env(++seq, "message.delta", { kind: "end", role: "assistant" })).frames;
		expect(appendedEntries(endFrames)).toEqual([]); // no assistant entry
		// tool result arrives as a standalone tool_result entry (args preserved
		// from the exec start so the group body can show the cell code)
		synth.handle(env(++seq, "tool.exec", { phase: "start", toolName: "ipython", toolCallId: "call_1", args: JSON.stringify({ code: "print(1)" }) }));
		const toolFrames = synth.handle(env(++seq, "tool.exec", { phase: "end", toolName: "ipython", toolCallId: "call_1", isError: false, result: JSON.stringify({ content: [{ type: "text", text: "1" }] }) })).frames;
		const toolEntries = appendedEntries(toolFrames);
		expect(toolEntries).toHaveLength(1);
		expect(toolEntries[0].item.type).toBe("tool_result");
		if (toolEntries[0].item.type !== "tool_result") throw new Error("unreachable");
		expect(toolEntries[0].item.args_json).toBe(JSON.stringify({ code: "print(1)" }));
		// idle closes the turn cleanly (no drift from the empty assistant)
		const idle = synth.handle(env(++seq, "session.state", { state: "idle" }));
		expect(idle.drift).toBe(false);
		expect(appendedEntries(idle.frames).some((e) => e.item.type === "turn_finished")).toBe(true);
	});

	it("text-bearing assistant messages still emit (with tool calls folded)", () => {
		const synth = makeSynthesizer();
		let seq = 0;
		synth.handle(env(++seq, "message.delta", { kind: "start", role: "user" }));
		synth.handle(env(++seq, "message.delta", { kind: "start", role: "assistant" }));
		synth.handle(env(++seq, "message.delta", { kind: "text", delta: "working" }));
		synth.handle(env(++seq, "message.delta", { kind: "toolcall", toolCall: JSON.stringify({ id: "call_1", name: "ipython", arguments: {} }) }));
		const endFrames = synth.handle(env(++seq, "message.delta", { kind: "end", role: "assistant" })).frames;
		const entries = appendedEntries(endFrames);
		expect(entries).toHaveLength(1);
		if (entries[0].item.type !== "assistant_message") throw new Error("expected assistant entry");
		expect(entries[0].item.items.map((i) => i.type)).toEqual(["text", "tool_call"]);
	});

	it("assistant end without start still flags drift (not masked by the empty-flush rule)", () => {
		const synth = makeSynthesizer();
		const r = synth.handle(env(1, "message.delta", { kind: "end", role: "assistant" }));
		const idle = synth.handle(env(2, "session.state", { state: "idle" }));
		expect(r.frames).toEqual([]);
		expect(idle.drift).toBe(true);
	});
});
