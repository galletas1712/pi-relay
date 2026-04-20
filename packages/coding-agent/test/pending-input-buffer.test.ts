import { describe, expect, test } from "vitest";
import { PendingInputBuffer } from "../src/modes/interactive/pending-input-buffer.js";

describe("PendingInputBuffer", () => {
	test("starts empty", () => {
		const buf = new PendingInputBuffer();
		expect(buf.length).toBe(0);
		expect(buf.tryShift()).toBeUndefined();
		expect(buf.snapshot()).toEqual([]);
	});

	test("push then tryShift returns FIFO order", () => {
		const buf = new PendingInputBuffer();
		buf.push("first");
		buf.push("second");
		buf.push("third");
		expect(buf.length).toBe(3);
		expect(buf.tryShift()).toBe("first");
		expect(buf.tryShift()).toBe("second");
		expect(buf.tryShift()).toBe("third");
		expect(buf.tryShift()).toBeUndefined();
		expect(buf.length).toBe(0);
	});

	test("tryShift on empty buffer returns undefined without throwing", () => {
		const buf = new PendingInputBuffer();
		expect(buf.tryShift()).toBeUndefined();
		expect(buf.tryShift()).toBeUndefined();
	});

	test("preserves empty-string and whitespace text verbatim", () => {
		const buf = new PendingInputBuffer();
		buf.push("");
		buf.push("   ");
		buf.push("\n");
		expect(buf.tryShift()).toBe("");
		expect(buf.tryShift()).toBe("   ");
		expect(buf.tryShift()).toBe("\n");
	});

	test("clear drops all buffered items", () => {
		const buf = new PendingInputBuffer();
		buf.push("a");
		buf.push("b");
		buf.clear();
		expect(buf.length).toBe(0);
		expect(buf.tryShift()).toBeUndefined();
	});

	test("snapshot is non-destructive", () => {
		const buf = new PendingInputBuffer();
		buf.push("a");
		buf.push("b");
		const snap = buf.snapshot();
		expect(snap).toEqual(["a", "b"]);
		expect(buf.length).toBe(2);
		// Subsequent mutation doesn't affect the returned snapshot.
		buf.push("c");
		expect(snap).toEqual(["a", "b"]);
	});

	test("drain-then-repopulate simulates getUserInput re-arming across multiple awaits", () => {
		// This is the exact usage in InteractiveMode.getUserInput:
		//   - mid-turn submits call push(text)
		//   - next getUserInput call does tryShift() and, if defined, resolves
		//     immediately without waiting for a physical keypress
		const buf = new PendingInputBuffer();

		// Turn 1: user submits during a session switch
		buf.push("switch-time submission");
		// Turn 2: main loop re-arms getUserInput
		expect(buf.tryShift()).toBe("switch-time submission");
		// Next getUserInput call waits for real input (no buffered text)
		expect(buf.tryShift()).toBeUndefined();

		// Later turn: another mid-turn submission
		buf.push("another switch");
		buf.push("and another");
		expect(buf.tryShift()).toBe("another switch");
		expect(buf.tryShift()).toBe("and another");
		expect(buf.tryShift()).toBeUndefined();
	});
});
