import { describe, expect, it } from "vitest";
import { Mailbox } from "../src/mailbox.js";

describe("Mailbox", () => {
	it("drains matching items and preserves non-matching items", async () => {
		const mailbox = new Mailbox<number>();
		mailbox.enqueue(1);
		mailbox.enqueue(2);
		mailbox.enqueue(3);

		expect(await mailbox.drain((value) => value % 2 === 1)).toEqual([1, 3]);
		expect(mailbox.tryDrain(() => true)).toEqual([2]);
	});

	it("blocks until a matching item is enqueued", async () => {
		const mailbox = new Mailbox<number>();
		const drained = mailbox.drain((value) => value === 2);
		mailbox.enqueue(1);
		setTimeout(() => {
			mailbox.enqueue(2);
		}, 10);

		await expect(drained).resolves.toEqual([2]);
		expect(mailbox.tryDrain(() => true)).toEqual([1]);
	});

	it("returns matching items in FIFO order among matches", () => {
		const mailbox = new Mailbox<string>();
		mailbox.enqueue("a");
		mailbox.enqueue("x");
		mailbox.enqueue("b");
		mailbox.enqueue("y");

		expect(mailbox.tryDrain((value) => value === "a" || value === "b")).toEqual(["a", "b"]);
		expect(mailbox.tryDrain(() => true)).toEqual(["x", "y"]);
	});

	it("hasMatching is accurate and does not remove items", () => {
		const mailbox = new Mailbox<number>();
		mailbox.enqueue(1);
		mailbox.enqueue(2);

		expect(mailbox.hasMatching((value) => value === 2)).toBe(true);
		expect(mailbox.hasMatching((value) => value === 3)).toBe(false);
		expect(mailbox.tryDrain(() => true)).toEqual([1, 2]);
	});

	it("wakes a blocked drain when closed", async () => {
		const mailbox = new Mailbox<number>();
		const drained = mailbox.drain((value) => value > 0);
		setTimeout(() => {
			mailbox.close();
		}, 10);

		await expect(drained).resolves.toEqual([]);
		expect(mailbox.closed).toBe(true);
		expect(mailbox.enqueue(1)).toBe(false);
	});

	it("returns [] when tryDrain finds no matches and when draining a closed empty mailbox", async () => {
		const mailbox = new Mailbox<number>();
		expect(mailbox.tryDrain((value) => value === 1)).toEqual([]);

		mailbox.close();
		await expect(mailbox.drain((value) => value === 1)).resolves.toEqual([]);
	});

	it("throws on concurrent drain calls", async () => {
		const mailbox = new Mailbox<number>();
		const firstDrain = mailbox.drain((value) => value === 1);

		await expect(mailbox.drain((value) => value === 2)).rejects.toThrow("Concurrent drain() calls are not supported");

		mailbox.enqueue(1);
		await expect(firstDrain).resolves.toEqual([1]);
	});

	it("throws on concurrent drain calls even when the second predicate already matches buffered data", async () => {
		const mailbox = new Mailbox<number>();
		mailbox.enqueue(2);
		const firstDrain = mailbox.drain((value) => value === 1);

		await expect(mailbox.drain((value) => value === 2)).rejects.toThrow("Concurrent drain() calls are not supported");

		mailbox.enqueue(1);
		await expect(firstDrain).resolves.toEqual([1]);
		expect(mailbox.tryDrain(() => true)).toEqual([2]);
	});

	it("aborts a waiting drain", async () => {
		const mailbox = new Mailbox<number>();
		const controller = new AbortController();
		const drained = mailbox.drain((value) => value === 1, controller.signal);
		controller.abort();

		await expect(drained).rejects.toMatchObject({ name: "AbortError" });
	});

	it("clear removes both matching and non-matching items", () => {
		const mailbox = new Mailbox<number>();
		mailbox.enqueue(1);
		mailbox.enqueue(2);

		mailbox.clear();
		expect(mailbox.size).toBe(0);
		expect(mailbox.tryDrain(() => true)).toEqual([]);
	});

	it("passes the full item into the predicate", () => {
		const mailbox = new Mailbox<{ kind: string; value: number }>();
		mailbox.enqueue({ kind: "steering", value: 1 });
		mailbox.enqueue({ kind: "tool_result", value: 2 });

		expect(mailbox.tryDrain((item) => item.kind === "tool_result")).toEqual([{ kind: "tool_result", value: 2 }]);
		expect(mailbox.tryDrain(() => true)).toEqual([{ kind: "steering", value: 1 }]);
	});
});
