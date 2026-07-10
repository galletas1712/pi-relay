// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState, type ReactNode } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { ExportDialog } from "./exportDialog.tsx";
import type { AssistantItem, TranscriptEntry, TranscriptItem } from "./types.ts";

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
	vi.restoreAllMocks();
	vi.unstubAllGlobals();
});

describe("ExportDialog", () => {
	it.each(["escape", "outside"] as const)(
		"starts on a stable heading, closes via %s without an action, and restores opener focus",
		async (closeMethod) => {
			const actions = actionSpies();
			const user = userEvent.setup({ pointerEventsCheck: 0 });
			render(
				<DialogLauncher label="Open export">
					{(close) => (
						<ExportDialog
							entries={exportEntries()}
							onClose={close}
							onError={actions.onError}
						/>
					)}
				</DialogLauncher>,
			);
			const opener = screen.getByRole("button", { name: "Open export" });
			await user.click(opener);
			const heading = await screen.findByRole("heading", { name: "Export messages" });

			expect(document.activeElement).toBe(heading);
			expect(screen.getByRole("dialog", { name: "Export messages" }).getAttribute("aria-describedby")).toBeTruthy();
			expect(screen.getByRole("button", { name: "close export dialog" })).toBeTruthy();
			expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(1);
			expect(document.querySelectorAll(".dialog-overlay")).toHaveLength(1);
			expect(document.querySelector(".modal-scrim")).toBeNull();

			if (closeMethod === "escape") await user.keyboard("{Escape}");
			else await user.click(document.querySelector(".dialog-overlay") as HTMLElement);

			await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
			expect(document.activeElement).toBe(opener);
			expect(actions.onError).not.toHaveBeenCalled();
		},
	);

	it("preserves selection controls and copies the selected transcript once", async () => {
		const actions = actionSpies();
		const user = userEvent.setup();
		const clipboardWrite = vi.spyOn(navigator.clipboard, "writeText").mockResolvedValue(undefined);
		render(
			<DialogLauncher label="Open export">
				{(close) => (
					<ExportDialog
						entries={exportEntries()}
						onClose={() => {
							actions.onClose();
							close();
						}}
						onError={actions.onError}
					/>
				)}
			</DialogLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open export" });
		await user.click(opener);

		expect(screen.getByText("1 of 2 selected")).toBeTruthy();
		const checkboxes = screen.getAllByRole<HTMLInputElement>("checkbox");
		expect(checkboxes.map((checkbox) => checkbox.checked)).toEqual([false, true]);

		await user.click(screen.getByRole("button", { name: "Select all" }));
		expect(checkboxes.map((checkbox) => checkbox.checked)).toEqual([true, true]);
		expect(screen.getByText("2 of 2 selected")).toBeTruthy();
		await user.click(checkboxes[0]!);
		expect(screen.getByText("1 of 2 selected")).toBeTruthy();
		await user.click(screen.getByRole("button", { name: "Copy to clipboard" }));

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(clipboardWrite).toHaveBeenCalledTimes(1);
		expect(clipboardWrite.mock.calls[0]?.[0]).toContain("Final answer.");
		expect(clipboardWrite.mock.calls[0]?.[0]).not.toContain("I will inspect.");
		expect(actions.onError).not.toHaveBeenCalled();
		expect(actions.onClose).toHaveBeenCalledTimes(1);
		expect(document.activeElement).toBe(opener);
	});

	it("downloads the selected transcript and closes without copying", async () => {
		const createObjectURL = vi.fn(() => "blob:export");
		const revokeObjectURL = vi.fn();
		vi.stubGlobal("URL", {
			...URL,
			createObjectURL,
			revokeObjectURL,
		});
		vi.spyOn(HTMLAnchorElement.prototype, "click").mockImplementation(() => {});
		const actions = actionSpies();
		const user = userEvent.setup();
		render(
			<DialogLauncher label="Open export">
				{(close) => (
					<ExportDialog
						entries={exportEntries()}
						onClose={() => {
							actions.onClose();
							close();
						}}
						onError={actions.onError}
					/>
				)}
			</DialogLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open export" });
		await user.click(opener);

		await user.click(screen.getByRole("button", { name: "Download Markdown" }));

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(createObjectURL).toHaveBeenCalledTimes(1);
		expect(actions.onClose).toHaveBeenCalledTimes(1);
		expect(actions.onError).not.toHaveBeenCalled();
		expect(document.activeElement).toBe(opener);
	});

	it("keeps a pending copy modal, disables duplicate actions, and reports a failure once", async () => {
		let rejectCopy!: (reason?: unknown) => void;
		const pendingCopy = new Promise<void>((_resolve, reject) => {
			rejectCopy = reject;
		});
		const actions = actionSpies();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		const clipboardWrite = vi.spyOn(navigator.clipboard, "writeText").mockImplementation(() => pendingCopy);
		render(
			<ExportDialog
				entries={exportEntries()}
				onClose={actions.onClose}
				onError={actions.onError}
			/>,
		);

		await user.dblClick(screen.getByRole("button", { name: "Copy to clipboard" }));
		expect(clipboardWrite).toHaveBeenCalledTimes(1);
		expect((screen.getByRole("button", { name: "Copying…" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "Download Markdown" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "close export dialog" }) as HTMLButtonElement).disabled).toBe(true);

		await user.keyboard("{Escape}");
		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);
		expect(screen.getByRole("dialog")).toBeTruthy();
		expect(actions.onClose).not.toHaveBeenCalled();

		const error = new Error("clipboard denied");
		rejectCopy(error);
		await waitFor(() => expect(actions.onError).toHaveBeenCalledWith(error));
		expect(actions.onError).toHaveBeenCalledTimes(1);
		expect(actions.onClose).not.toHaveBeenCalled();
		expect(screen.getByRole("button", { name: "Copy to clipboard" })).toBeTruthy();
	});

	it("preserves the empty state and disables selection and export actions", () => {
		const actions = actionSpies();
		render(
			<ExportDialog
				entries={[]}
				onClose={actions.onClose}
				onError={actions.onError}
			/>,
		);

		expect(screen.getByText("No assistant messages to export.")).toBeTruthy();
		expect(screen.getByText("0 of 0 selected")).toBeTruthy();
		for (const name of ["Select all", "Select last", "Clear", "Copy to clipboard", "Download Markdown"]) {
			expect((screen.getByRole("button", { name }) as HTMLButtonElement).disabled).toBe(true);
		}
	});
});

function DialogLauncher({
	label,
	children,
}: {
	label: string;
	children: (close: () => void) => ReactNode;
}) {
	const [open, setOpen] = useState(false);
	return (
		<>
			<button type="button" onClick={() => setOpen(true)}>{label}</button>
			{open ? children(() => setOpen(false)) : null}
		</>
	);
}

function actionSpies() {
	return {
		onClose: vi.fn(),
		onError: vi.fn(),
	};
}

function exportEntries(): TranscriptEntry[] {
	return [
		entry("start", { type: "turn_started", turn_id: 1 }),
		entry("user", { type: "user_message", content: [{ type: "text", text: "Please inspect." }] }),
		entry("assistant-progress", {
			type: "assistant_message",
			items: [text("I will inspect."), {
				type: "tool_call",
				id: "call-1",
				tool_name: "bash",
				args_json: "{\"command\":\"ls\"}",
			}],
		}),
		entry("tool-result", {
			type: "tool_result",
			tool_call_id: "call-1",
			tool_name: "bash",
			output: "ok",
			status: "Success",
		}),
		entry("assistant-final", { type: "assistant_message", items: [text("Final answer.")] }),
		entry("finish", { type: "turn_finished", turn_id: 1, outcome: "Graceful" }),
	];
}

function text(value: string): AssistantItem {
	return { type: "text", text: value };
}

function entry(id: string, item: TranscriptItem): TranscriptEntry {
	return {
		id,
		parent_id: null,
		timestamp_ms: 1,
		item,
	};
}
