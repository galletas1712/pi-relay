// @vitest-environment jsdom

import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { createRef } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { Composer, type ComposerHandle, SlashMenu } from "./composer.tsx";
import {
	composerTextNeedsConnection,
	ConnectionRecoveryBanner,
	ConnectionRetryController,
	firstDisabledReason,
	remoteActionBlockedReason,
	WAITING_FOR_CONNECTION,
} from "./connectionRecovery.tsx";
import { COMMANDS } from "./slash.ts";

beforeAll(() => {
	class ResizeObserver {
		observe() {}
		unobserve() {}
		disconnect() {}
	}
	vi.stubGlobal("ResizeObserver", ResizeObserver);
	HTMLElement.prototype.scrollIntoView ??= () => {};
	HTMLElement.prototype.hasPointerCapture ??= () => false;
	HTMLElement.prototype.setPointerCapture ??= () => {};
	HTMLElement.prototype.releasePointerCapture ??= () => {};
});

afterEach(() => {
	cleanup();
	window.localStorage.clear();
});

describe("connection policy", () => {
	it.each(["connecting", "closed", "error"] as const)("blocks remote actions while %s", (status) => {
		expect(remoteActionBlockedReason(status)).toBe(WAITING_FOR_CONNECTION);
	});

	it("allows open actions, prioritizes connection errors, and classifies local slash commands", () => {
		expect(remoteActionBlockedReason("open")).toBeNull();
		expect(firstDisabledReason(WAITING_FOR_CONNECTION, "Saving…")).toBe(WAITING_FOR_CONNECTION);
		expect(firstDisabledReason(null, "Saving…")).toBe("Saving…");
		for (const [text, expected] of [
			["plain text", true],
			["/compact", true],
			["/system", true],
			["/help", false],
			["/export", false],
			["/switch", true],
		] as const) {
			expect(composerTextNeedsConnection(text)).toBe(expected);
		}
		expect(composerTextNeedsConnection("/switch", { cachedHistoryAvailable: true })).toBe(false);
	});
});

describe("connection recovery primitives", () => {
	it("renders unavailable and pending Retry states, then hides when open", () => {
		const { rerender } = render(
			<ConnectionRecoveryBanner
				status="closed"
				hasConnected
				retrying={false}
				onRetry={() => undefined}
			/>,
		);
		expect(screen.getByText("Connection closed")).toBeTruthy();
		expect(screen.getByRole("button", { name: "Retry connection" })).toBeTruthy();

		rerender(
			<ConnectionRecoveryBanner status="closed" hasConnected retrying onRetry={() => undefined} />,
		);
		const pending = screen.getByRole("button", { name: "Retrying…" }) as HTMLButtonElement;
		expect(pending.disabled).toBe(true);
		expect(pending.getAttribute("aria-busy")).toBe("true");

		rerender(
			<ConnectionRecoveryBanner status="open" hasConnected retrying={false} onRetry={() => undefined} />,
		);
		expect(screen.queryByRole("status")).toBeNull();
	});

	it("deduplicates Retry and fences a late failure after a later open", async () => {
		const attempt = deferred<void>();
		const connect = vi.fn(() => attempt.promise);
		const onFailure = vi.fn();
		const onSettled = vi.fn();
		const controller = new ConnectionRetryController();

		const first = controller.retry(connect, onFailure, onSettled);
		expect(controller.retry(connect, onFailure, onSettled)).toBe(first);
		expect(connect).toHaveBeenCalledTimes(1);

		controller.opened();
		attempt.reject(new Error("stale failure"));
		await first;
		expect(onFailure).not.toHaveBeenCalled();
		expect(onSettled).not.toHaveBeenCalled();
		expect(controller.retry(connect, onFailure, onSettled)).not.toBe(first);
	});
});

describe("representative component gates", () => {
	it("keeps drafting and local slash commands available while remote sends stay blocked", async () => {
		const user = userEvent.setup();
		const handle = createRef<ComposerHandle>();
		render(
			<Composer
				selectedId="session-1"
				selectedIsSubagent={false}
				composerHandleRef={handle}
				sending={false}
				canStop
				stopping={false}
				queuedInputs={[]}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				onSubmit={() => true}
				onStop={() => undefined}
				onPromoteQueued={() => undefined}
				onUpdateQueued={() => undefined}
				onCancelQueued={() => undefined}
				onMoveQueued={() => undefined}
			/>,
		);
		const composer = screen.getByRole("textbox") as HTMLTextAreaElement;
		await user.type(composer, "offline draft");
		expect(composer.value).toBe("offline draft");
		expect((screen.getByRole("button", { name: "send message" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "stop active turn" }) as HTMLButtonElement).disabled).toBe(true);
		cleanup();

		render(
			<SlashMenu
				commands={COMMANDS}
				visible
				selectedIndex={0}
				mutationBlockedReason={WAITING_FOR_CONNECTION}
				cachedHistoryAvailable
				onSetIndex={() => undefined}
				onSelect={() => undefined}
			/>,
		);
		expect((screen.getByRole("option", { name: /help/i }) as HTMLButtonElement).disabled).toBe(false);
		expect((screen.getByRole("option", { name: /export/i }) as HTMLButtonElement).disabled).toBe(false);
		expect((screen.getByRole("option", { name: /switch/i }) as HTMLButtonElement).disabled).toBe(false);
		expect((screen.getByRole("option", { name: /compact/i }) as HTMLButtonElement).disabled).toBe(true);
	});
});

function deferred<T>() {
	let resolve!: (value: T | PromiseLike<T>) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}
