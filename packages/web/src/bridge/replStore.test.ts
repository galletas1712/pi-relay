// Unit tests for the M9 repl projection: lifecycle merge, provenance, output
// interleave, watermark dedupe, terminal stickiness, late-attach synthesis,
// and history trimming.
import { describe, expect, it } from "vitest";
import { applyReplEvent, emptyReplProjection, replBusyCounts, type ReplProjection } from "./replStore.ts";
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

function fresh(): ReplProjection {
	seqCounter = 0;
	return emptyReplProjection(SID);
}

describe("applyReplEvent", () => {
	it("projects a full user cell lifecycle queued -> running -> done", () => {
		let p = fresh();
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_abc", provenance: "user", status: "queued", code: "x = 1", queued_at: "t0" }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_abc", status: "running", started_at: "t1" }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_abc", status: "done", finished_at: "t2", duration_ms: 42 }));
		expect(p.watermark).toBe(3);
		expect(p.cells).toHaveLength(1);
		const cell = p.cells[0];
		expect(cell.status).toBe("done");
		expect(cell.provenance).toBe("user");
		expect(cell.code).toBe("x = 1"); // merged from the queued frame
		expect(cell.durationMs).toBe(42);
		expect(cell.queuedAt).toBe("t0");
	});

	it("interleaves outputs on the right cell in arrival order", () => {
		let p = fresh();
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "m_t1", provenance: "model", status: "running", tool_call_id: "t1" }));
		p = applyReplEvent(p, ev("repl.output", { cell_id: "m_t1", stream: "stdout", data: "hello" }));
		p = applyReplEvent(p, ev("repl.output", { cell_id: "m_t1", stream: "display", data: "42", mime_type: "text/plain" }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "m_t1", status: "done" }));
		const cell = p.cells[0];
		expect(cell.toolCallId).toBe("t1");
		expect(cell.outputs.map((o) => o.stream)).toEqual(["stdout", "display"]);
		expect(cell.outputs[1].mimeType).toBe("text/plain");
	});

	it("drops duplicate/replayed seqs at or below the watermark", () => {
		let p = fresh();
		const e1 = ev("repl.cell", { cell_id: "u_1", provenance: "user", status: "queued", code: "1" }, 10);
		p = applyReplEvent(p, e1);
		const before = p;
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_1", status: "queued" }, 10));
		p = applyReplEvent(p, ev("repl.output", { cell_id: "u_1", stream: "stdout", data: "x" }, 9));
		expect(p).toBe(before);
	});

	it("terminal cells do not regress on out-of-order non-terminal frames", () => {
		let p = fresh();
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_1", status: "queued", code: "1" }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_1", status: "done", duration_ms: 1 }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_1", status: "running" }));
		expect(p.cells[0].status).toBe("done");
	});

	it("synthesizes a running cell when output arrives first (late attach)", () => {
		let p = fresh();
		p = applyReplEvent(p, ev("repl.output", { cell_id: "u_late", stream: "stdout", data: "partial" }));
		expect(p.cells).toHaveLength(1);
		expect(p.cells[0].status).toBe("running");
		expect(p.cells[0].outputs[0].data).toBe("partial");
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_late", provenance: "user", status: "done" }));
		expect(p.cells[0].provenance).toBe("user");
		expect(p.cells[0].status).toBe("done");
	});

	it("keeps error status + traceback output and truncation flags", () => {
		let p = fresh();
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "u_e", status: "queued", code: "boom()" }));
		p = applyReplEvent(p, ev("repl.output", { cell_id: "u_e", stream: "error", data: "Traceback\nValueError" }));
		p = applyReplEvent(
			p,
			ev("repl.cell", { cell_id: "u_e", status: "error", error: { ename: "ValueError", evalue: "boom" }, stdout_truncated: true }),
		);
		const cell = p.cells[0];
		expect(cell.status).toBe("error");
		expect(cell.error?.ename).toBe("ValueError");
		expect(cell.outputs[0].stream).toBe("error");
		expect(cell.stdoutTruncated).toBe(true);
	});

	it("ignores events for other sessions and non-repl events", () => {
		let p = fresh();
		const other = { ...ev("repl.cell", { cell_id: "u_x", status: "queued" }), sessionId: "s2" };
		p = applyReplEvent(p, other);
		expect(p.cells).toHaveLength(0);
		p = applyReplEvent(p, ev("message.delta", { text: "hi" }));
		expect(p.watermark).toBe(0);
	});

	it("trims oldest finished cells beyond the cap", () => {
		let p = fresh();
		for (let i = 0; i < 305; i++) {
			p = applyReplEvent(p, ev("repl.cell", { cell_id: `u_${i}`, status: "queued", code: `${i}` }));
			p = applyReplEvent(p, ev("repl.cell", { cell_id: `u_${i}`, status: "done" }));
		}
		expect(p.cells.length).toBeLessThanOrEqual(300);
		// newest cell survives
		expect(p.cells.some((c) => c.cellId === "u_304")).toBe(true);
	});

	it("counts queued/running for the busy indicator", () => {
		let p = fresh();
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "a", status: "queued" }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "b", status: "queued" }));
		p = applyReplEvent(p, ev("repl.cell", { cell_id: "b", status: "running" }));
		expect(replBusyCounts(p)).toEqual({ queued: 1, running: 1 });
	});
});
