import { describe, expect, test } from "vitest";
import { InteractiveMode } from "../src/modes/interactive/interactive-mode.js";
import { PendingInputBuffer } from "../src/modes/interactive/pending-input-buffer.js";

type GetUserInputThis = {
	pendingInputBuffer: PendingInputBuffer;
	onInputCallback?: (text: string) => void;
};

const prototype = InteractiveMode.prototype as unknown as {
	getUserInput(this: GetUserInputThis): Promise<string>;
};

describe("InteractiveMode.getUserInput wired to PendingInputBuffer", () => {
	test("resolves synchronously with buffered text when the buffer is non-empty", async () => {
		const ctx: GetUserInputThis = {
			pendingInputBuffer: new PendingInputBuffer(),
		};
		ctx.pendingInputBuffer.push("replayed-text");

		const result = await prototype.getUserInput.call(ctx);
		expect(result).toBe("replayed-text");
		// The callback must NOT be installed when a buffered value resolved the promise,
		// otherwise the next user keystroke would double-resolve.
		expect(ctx.onInputCallback).toBeUndefined();
		expect(ctx.pendingInputBuffer.length).toBe(0);
	});

	test("drains buffered items in FIFO order across successive calls", async () => {
		const ctx: GetUserInputThis = {
			pendingInputBuffer: new PendingInputBuffer(),
		};
		ctx.pendingInputBuffer.push("one");
		ctx.pendingInputBuffer.push("two");

		expect(await prototype.getUserInput.call(ctx)).toBe("one");
		expect(ctx.onInputCallback).toBeUndefined();
		expect(await prototype.getUserInput.call(ctx)).toBe("two");
		expect(ctx.onInputCallback).toBeUndefined();
	});

	test("installs onInputCallback when the buffer is empty, then resolves when the callback fires", async () => {
		const ctx: GetUserInputThis = {
			pendingInputBuffer: new PendingInputBuffer(),
		};

		const promise = prototype.getUserInput.call(ctx);

		// The buffer was empty: the implementation should arm onInputCallback so
		// a later physical Enter press (onSubmit calling onInputCallback(text))
		// resolves the promise.
		expect(ctx.onInputCallback).toBeDefined();
		expect(ctx.pendingInputBuffer.length).toBe(0);

		// Simulate the onSubmit handler's normal (non-buffered) path firing the callback.
		ctx.onInputCallback!("typed-text");

		expect(await promise).toBe("typed-text");
		// Per the existing implementation, the callback self-clears on resolve.
		expect(ctx.onInputCallback).toBeUndefined();
	});

	test("after draining buffer and re-arming, a push during the wait does NOT preempt the installed callback", async () => {
		// This pins the intentional behavior: the buffer is only consulted at the
		// START of getUserInput. Once a callback is armed, subsequent pushes don't
		// race-override it. (A push should only happen when onInputCallback is
		// unset; the onSubmit handler guards on that branch.)
		const ctx: GetUserInputThis = {
			pendingInputBuffer: new PendingInputBuffer(),
		};

		const promise = prototype.getUserInput.call(ctx);
		expect(ctx.onInputCallback).toBeDefined();

		// A push arrives — this would be a bug in the caller (the onSubmit guard
		// is meant to prevent this), but we want the promise to still resolve via
		// the callback path, not magically swap to the buffer.
		ctx.pendingInputBuffer.push("orphaned-push");
		ctx.onInputCallback!("callback-text");

		expect(await promise).toBe("callback-text");
		// The push stays in the buffer, ready for the next getUserInput call.
		expect(ctx.pendingInputBuffer.length).toBe(1);
	});
});
