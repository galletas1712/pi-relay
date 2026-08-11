// Unit tests for the event-sourced projection: ordering, dedupe, gap
// detection, transcript/tool/subagent projections, and rebuild semantics.
import { describe, expect, it } from "vitest";
import {
	appendLocalUserMessage,
	applyContractEvent,
	applyModelPatch,
	emptySessionProjection,
	markAttached,
	modelFromSnapshot,
	rebuildFromState,
	seqContiguity,
	type CommsBlock,
	type MessageBlock,
	type ToolExecBlock,
} from "./eventStore.ts";
import type { ContractEventEnvelope } from "./types.ts";

const SID = "s1";
let seqCounter = 0;

function ev(event: string, data: Record<string, unknown>, seq?: number): ContractEventEnvelope {
	return {
		event,
		sessionId: SID,
		seq: seq ?? ++seqCounter,
		data,
		at: new Date().toISOString(),
	};
}

function attached() {
	seqCounter = 0;
	return markAttached(emptySessionProjection(SID), 0);
}

describe("applyContractEvent ordering / dedupe / gap detection", () => {
	it("applies events in seq order and advances the watermark", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.state", { state: "running" }));
		s = applyContractEvent(s, ev("session.state", { state: "idle" }));
		expect(s.watermark).toBe(2);
		expect(s.state).toBe("idle");
		expect(s.gap).toBeNull();
	});

	it("drops duplicates at or below the watermark", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.state", { state: "running" }));
		const after = applyContractEvent(s, { ...ev("session.state", { state: "closed" }), seq: 1 });
		expect(after).toBe(s); // unchanged identity: no notification downstream
		expect(after.state).toBe("running");
	});

	it("drops replayed events that straddle the watermark", () => {
		let s = attached();
		s = applyContractEvent(s, ev("message.delta", { kind: "start", role: "assistant" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "text", delta: "hello" }));
		// re-attach replay from 0 would resend seq 1..2 flagged replayed
		const replayed = applyContractEvent(s, { ...ev("message.delta", { kind: "text", delta: "hello" }), seq: 2, replayed: true });
		expect(replayed).toBe(s);
		const msg = s.blocks[0] as MessageBlock;
		expect(msg.text).toBe("hello");
	});

	it("flags a discontinuity above watermark+1 but still applies the event", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.state", { state: "running" }));
		s = applyContractEvent(s, ev("session.state", { state: "idle" }, 5));
		expect(s.gap).toMatchObject({ expected: 2, got: 5 });
		expect(s.watermark).toBe(5);
		expect(s.state).toBe("idle");
	});

	it("does not flag the first event after attach at a non-zero head", () => {
		let s = markAttached(emptySessionProjection(SID), 40);
		s = applyContractEvent(s, { ...ev("session.state", { state: "idle" }), seq: 1 });
		expect(s.gap).toBeNull();
	});

	it("ignores events for other sessions", () => {
		const s = attached();
		const foreign = applyContractEvent(s, { ...ev("session.state", { state: "running" }), sessionId: "other" });
		expect(foreign).toBe(s);
	});
});

describe("transcript projection", () => {
	it("assembles assistant messages from start/delta/end", () => {
		let s = attached();
		s = applyContractEvent(s, ev("message.delta", { kind: "start", role: "assistant" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "thinking", delta: "hmm " }));
		s = applyContractEvent(s, ev("message.delta", { kind: "thinking", delta: "ok" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "text", delta: "B1-" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "text", delta: "DONE" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "end", role: "assistant" }));
		expect(s.blocks).toHaveLength(1);
		const msg = s.blocks[0] as MessageBlock;
		expect(msg.text).toBe("B1-DONE");
		expect(msg.thinking).toBe("hmm ok");
		expect(msg.complete).toBe(true);
	});

	it("opens a block for a delta that arrives without a start (late attach)", () => {
		let s = attached();
		s = applyContractEvent(s, ev("message.delta", { kind: "text", delta: "mid-stream" }, 41));
		const msg = s.blocks[0] as MessageBlock;
		expect(msg.text).toBe("mid-stream");
		expect(msg.complete).toBe(false);
	});

	it("projects tool.exec start/update/end keyed by toolCallId (ipython cell)", () => {
		let s = attached();
		const args = JSON.stringify({ code: "print(2+2)" });
		s = applyContractEvent(s, ev("tool.exec", { phase: "start", toolName: "ipython", toolCallId: "tc1", args }));
		s = applyContractEvent(s, ev("tool.exec", { phase: "update", toolName: "ipython", toolCallId: "tc1", partialResult: "…" }));
		s = applyContractEvent(s, ev("tool.exec", { phase: "end", toolName: "ipython", toolCallId: "tc1", result: "4", isError: false }));
		expect(s.blocks).toHaveLength(1);
		const tool = s.blocks[0] as ToolExecBlock;
		expect(tool.toolName).toBe("ipython");
		expect(tool.args).toBe(args);
		expect(tool.result).toBe("4");
		expect(tool.done).toBe(true);
		expect(tool.isError).toBe(false);
	});

	it("keeps two tool calls with distinct ids as separate blocks", () => {
		let s = attached();
		s = applyContractEvent(s, ev("tool.exec", { phase: "start", toolName: "ipython", toolCallId: "a" }));
		s = applyContractEvent(s, ev("tool.exec", { phase: "start", toolName: "ipython", toolCallId: "b" }));
		s = applyContractEvent(s, ev("tool.exec", { phase: "end", toolName: "ipython", toolCallId: "a", result: "1" }));
		const tools = s.blocks as ToolExecBlock[];
		expect(tools).toHaveLength(2);
		expect(tools[0].done).toBe(true);
		expect(tools[1].done).toBe(false);
	});

	it("absorbs toolResult message envelopes without creating blocks", () => {
		let s = attached();
		s = applyContractEvent(s, ev("message.delta", { kind: "start", role: "toolResult" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "end", role: "toolResult" }));
		expect(s.blocks).toHaveLength(0);
	});

	it("matches a stream user-message start to the optimistic local echo", () => {
		let s = attached();
		s = appendLocalUserMessage(s, "k1", "run the cell");
		s = applyContractEvent(s, ev("message.delta", { kind: "start", role: "user" }));
		s = applyContractEvent(s, ev("message.delta", { kind: "end", role: "user" }));
		expect(s.blocks).toHaveLength(1);
		const msg = s.blocks[0] as MessageBlock;
		expect(msg.text).toBe("run the cell");
		expect(msg.local).toBe(false);
		expect(msg.complete).toBe(true);
	});
});

describe("subagent lifecycle projection", () => {
	it("tracks admitted -> completed per rlm_child_id without phase dupes", () => {
		let s = attached();
		s = applyContractEvent(s, ev("subagent.lifecycle", { phase: "admitted", rlm_child_id: "c1", session_name: "child", session_id: "cs1" }));
		s = applyContractEvent(s, ev("subagent.lifecycle", { phase: "completed", rlm_child_id: "c1", session_name: "child", session_id: "cs1", status: "completed" }));
		// duplicate admitted (file+spool double-absorb upstream is deduped; live dupes dedupe here)
		s = applyContractEvent(s, ev("subagent.lifecycle", { phase: "admitted", rlm_child_id: "c1", session_name: "child" }));
		expect(s.subagents).toHaveLength(1);
		const sub = s.subagents[0];
		expect(sub.status).toBe("completed");
		expect(sub.phases).toEqual(["admitted", "completed"]);
		expect(sub.childSessionId).toBe("cs1");
	});
});

describe("rebuild after event_gap", () => {
	it("re-baselines the watermark at the server head and records the rebuild", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.state", { state: "running" }));
		const rebuilt = rebuildFromState(s, { state: "idle", headSeq: 97 }, "now");
		expect(rebuilt.watermark).toBe(97);
		expect(rebuilt.headSeq).toBe(97);
		expect(rebuilt.state).toBe("idle");
		expect(rebuilt.rebuiltAt).toBe("now");
		// blocks survive: the trimmed range is unrecoverable but prior content stays
		expect(rebuilt.blocks).toEqual(s.blocks);
	});

	it("M8 (G2): adopts bridge-rebuilt transcript blocks when the snapshot carries them", () => {
		const s = attached();
		const blocks = [
			{ kind: "message" as const, id: "u1", role: "user", text: "hello", thinking: "", complete: true },
			{ kind: "tool" as const, toolCallId: "t1", toolName: "ipython", args: "{}", result: "ok", done: true },
		];
		const rebuilt = rebuildFromState(s, { state: "idle", headSeq: 42, transcript: { blocks } }, "now");
		expect(rebuilt.blocks).toEqual(blocks);
		expect(rebuilt.watermark).toBe(42);
	});
});

describe("seqContiguity", () => {
	it("accepts a gapless sequence with dupes removed", () => {
		expect(seqContiguity([1, 2, 3, 4, 5])).toEqual({ contiguous: true, firstGapAt: null });
	});
	it("ignores duplicates below the watermark", () => {
		expect(seqContiguity([1, 2, 2, 3])).toEqual({ contiguous: true, firstGapAt: null });
	});
	it("detects the first hole", () => {
		expect(seqContiguity([1, 2, 4])).toEqual({ contiguous: false, firstGapAt: 3 });
	});
});

describe("M11a model surface", () => {
	it("session.model sets the full model state", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.model", { provider: "openai", modelId: "gpt-5.6-sol", name: "GPT 5.6 Sol", thinkingLevel: "high" }));
		expect(s.model).toEqual({ provider: "openai", modelId: "gpt-5.6-sol", name: "GPT 5.6 Sol", thinkingLevel: "high" });
	});

	it("session.model merges partial fields (thinking_level_changed carries only the level)", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.model", { provider: "openai", modelId: "gpt-5.6-sol", name: null, thinkingLevel: "high" }));
		s = applyContractEvent(s, ev("session.model", { provider: null, modelId: null, name: null, thinkingLevel: "low" }));
		expect(s.model).toEqual({ provider: "openai", modelId: "gpt-5.6-sol", name: null, thinkingLevel: "low" });
	});

	it("modelFromSnapshot parses the live provider/id string and thinking level", () => {
		expect(modelFromSnapshot({ live: { model: "openai/gpt-5.6-sol", thinkingLevel: "medium" } })).toEqual({
			provider: "openai",
			modelId: "gpt-5.6-sol",
			name: null,
			thinkingLevel: "medium",
		});
	});

	it("modelFromSnapshot falls back to the session-file model when the host is down", () => {
		expect(
			modelFromSnapshot({ live: null, model: { provider: "nvidia-inference", modelId: "glm-4.7", thinkingLevel: "off" } }),
		).toEqual({ provider: "nvidia-inference", modelId: "glm-4.7", name: null, thinkingLevel: "off" });
		expect(modelFromSnapshot({ live: null, model: null })).toBeNull();
		expect(modelFromSnapshot({})).toBeNull();
	});

	it("rebuildFromState adopts the snapshot model surface", () => {
		let s = attached();
		s = rebuildFromState(s, { state: "idle", headSeq: 9, live: { model: "openai/gpt-5.6-sol", thinkingLevel: "high" } }, "now");
		expect(s.model?.provider).toBe("openai");
		expect(s.model?.thinkingLevel).toBe("high");
	});

	it("applyModelPatch patches optimistically without losing other fields", () => {
		let s = attached();
		s = applyContractEvent(s, ev("session.model", { provider: "openai", modelId: "gpt-5.6-sol", name: "x", thinkingLevel: "high" }));
		s = applyModelPatch(s, { thinkingLevel: "max" });
		expect(s.model).toMatchObject({ provider: "openai", modelId: "gpt-5.6-sol", thinkingLevel: "max" });
	});
});

describe("M11a comms annotations", () => {
	it("appends an outbound comms block from prime-comms terminal transitions", () => {
		let s = attached();
		s = applyContractEvent(
			s,
			ev("comms.message", {
				fromSessionId: SID,
				fromName: "root",
				role: "child",
				receiverName: "worker-1",
				targetSessionId: "childsession123",
				message: "do the thing",
				deliveryStatus: "delivered",
			}),
		);
		expect(s.blocks).toHaveLength(1);
		const b = s.blocks[0] as CommsBlock;
		expect(b.kind).toBe("comms");
		expect(b.direction).toBe("out");
		expect(b.to).toBe("worker-1");
		expect(b.text).toBe("do the thing");
		expect(b.deliveryStatus).toBe("delivered");
	});

	it("appends an inbound comms block (bridge agent_message mapping)", () => {
		let s = attached();
		s = applyContractEvent(
			s,
			ev("comms.message", {
				direction: "in",
				fromSessionId: "parentsess",
				fromName: "parent",
				role: "parent",
				message: "here is your task",
				deliveryStatus: "delivered",
			}),
		);
		const b = s.blocks[0] as CommsBlock;
		expect(b.direction).toBe("in");
		expect(b.from).toBe("parent");
		expect(b.to).toBe("this session");
	});

	it("skips comms.message events with nothing displayable", () => {
		let s = attached();
		const after = applyContractEvent(s, ev("comms.message", { deliveryStatus: "persisted" }));
		expect(after.blocks).toHaveLength(0);
	});

	it("comms blocks survive dedupe like any other event (watermark discipline)", () => {
		let s = attached();
		s = applyContractEvent(s, ev("comms.message", { receiverName: "w", message: "m", deliveryStatus: "delivered" }));
		const dupe = applyContractEvent(s, { ...ev("comms.message", { receiverName: "w", message: "m" }), seq: 1 });
		expect(dupe).toBe(s);
	});
});
