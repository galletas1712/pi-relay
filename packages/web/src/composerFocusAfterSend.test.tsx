// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { Composer, type ComposerHandle } from "./composer.tsx";

beforeAll(() => {
	class ResizeObserver {
		observe() {}
		unobserve() {}
		disconnect() {}
	}
	vi.stubGlobal("ResizeObserver", ResizeObserver);
});

afterEach(() => {
	cleanup();
	vi.unstubAllGlobals();
	window.localStorage.clear();
});

function stubPointer(pointer: "coarse" | "fine") {
	vi.stubGlobal("matchMedia", (query: string) => ({
		matches: query.includes(pointer),
		media: query,
		addEventListener: () => undefined,
		removeEventListener: () => undefined,
		addListener: () => undefined,
		removeListener: () => undefined,
		onchange: null,
		dispatchEvent: () => false,
	}));
}

function ComposerHarness() {
	const composerHandleRef = useRef<ComposerHandle | null>(null);
	return (
		<Composer
			selectedId="session-1"
			selectedIsSubagent={false}
			composerHandleRef={composerHandleRef}
			sending={false}
			canStop={false}
			stopping={false}
			queuedInputs={[]}
			onSubmit={() => true}
			onStop={() => undefined}
			onPromoteQueued={() => undefined}
			onUpdateQueued={() => undefined}
			onCancelQueued={() => undefined}
			onReorderQueued={() => undefined}
		/>
	);
}

describe("Composer focus after send", () => {
	it("blurs the composer on touch devices so the software keyboard closes", async () => {
		stubPointer("coarse");
		const user = userEvent.setup();
		render(<ComposerHarness />);
		const composer = screen.getByRole("textbox");
		await user.click(composer);
		await user.type(composer, "hello");
		await user.click(screen.getByRole("button", { name: "send message" }));

		await waitFor(() => expect(document.activeElement).not.toBe(composer));
	});

	it("keeps focus on pointer devices", async () => {
		stubPointer("fine");
		const user = userEvent.setup();
		render(<ComposerHarness />);
		const composer = screen.getByRole("textbox");
		await user.click(composer);
		await user.type(composer, "hello");
		await user.keyboard("{Control>}{Enter}{/Control}");

		await new Promise((resolve) => requestAnimationFrame(resolve));
		expect(document.activeElement).toBe(composer);
	});
});
