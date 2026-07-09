// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useMemo, useRef, useState, type RefObject } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { Composer, type ComposerHandle } from "./composer.tsx";
import { CompactHistoryPickerDialog } from "./historyPickerCompact.tsx";

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

describe("Composer dialog focus handoff", () => {
	it("does not steal focus from a slash-command dialog and receives focus back on Escape", async () => {
		const user = userEvent.setup();
		render(<ComposerDialogHarness />);
		const composer = screen.getByRole("textbox");
		await user.click(composer);
		await user.type(composer, "/switch");
		await user.keyboard("{Control>}{Enter}{/Control}");

		const heading = await screen.findByRole("heading", { name: "Switch branch" });
		await new Promise((resolve) => requestAnimationFrame(resolve));
		expect(document.activeElement).toBe(heading);

		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(document.activeElement).toBe(composer);
	});
});

function ComposerDialogHarness() {
	const [open, setOpen] = useState(false);
	const composerHandleRef = useRef<ComposerHandle | null>(null);
	const returnFocusFallbackRef = useMemo<RefObject<HTMLElement | null>>(
		() => ({
			get current() {
				return composerHandleRef.current?.focusTarget() ?? null;
			},
		}),
		[],
	);
	return (
		<>
			<Composer
				selectedId="session-1"
				selectedIsSubagent={false}
				composerHandleRef={composerHandleRef}
				sending={false}
				canStop={false}
				stopping={false}
				queuedInputs={[]}
				onSubmit={() => {
					setOpen(true);
					return true;
				}}
				onStop={() => undefined}
				onPromoteQueued={() => undefined}
				onUpdateQueued={() => undefined}
				onCancelQueued={() => undefined}
				onMoveQueued={() => undefined}
			/>
			{open ? (
				<CompactHistoryPickerDialog
					nodes={[]}
					activeLeafId={null}
					onClose={() => setOpen(false)}
					onSwitch={() => undefined}
					returnFocusFallbackRef={returnFocusFallbackRef}
				/>
			) : null}
		</>
	);
}
