// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { ActionMenu } from "./actionMenu.tsx";
import { RenameSessionDialog } from "./entityDialogs.tsx";
import { SessionRow } from "./panels.tsx";
import type { SessionSummary } from "./types.ts";

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

afterEach(cleanup);

describe("ActionMenu", () => {
	it("exposes native menu semantics and restores trigger focus on Escape", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup();
		render(
			<ActionMenu
				triggerLabel="Open test actions"
				items={[{ id: "test", label: "Test action", onSelect }]}
			/>,
		);

		const trigger = screen.getByRole("button", { name: "Open test actions" });
		expect(trigger.getAttribute("aria-haspopup")).toBe("menu");
		expect(trigger.getAttribute("aria-expanded")).toBe("false");
		await user.click(trigger);
		expect(await screen.findByRole("menuitem", { name: "Test action" })).toBeTruthy();
		expect(trigger.getAttribute("aria-expanded")).toBe("true");

		await user.keyboard("{Escape}");
		await waitFor(() => {
			expect(screen.queryByRole("menu")).toBeNull();
			expect(document.activeElement).toBe(trigger);
		});
		expect(onSelect).not.toHaveBeenCalled();
	});

	it("runs a row action once without selecting the row", async () => {
		const onSelectRow = vi.fn();
		const onArchive = vi.fn();
		const user = userEvent.setup();
		render(
			<ul>
				<SessionRow
					session={session()}
					selected={false}
					onSelect={onSelectRow}
					onRename={vi.fn()}
					onArchiveToggle={onArchive}
					onDelete={vi.fn()}
				/>
			</ul>,
		);

		const trigger = screen.getByRole("button", {
			name: "Open session actions for Menu session",
		});
		await user.click(trigger);
		await user.click(await screen.findByRole("menuitem", { name: "Archive" }));

		await waitFor(() => expect(document.activeElement).toBe(trigger));
		expect(onArchive).toHaveBeenCalledTimes(1);
		expect(onSelectRow).not.toHaveBeenCalled();
	});

	it("unmounts the menu before a dialog takes focus and returns focus to the trigger", async () => {
		const user = userEvent.setup();
		render(<DialogActionHarness />);

		const trigger = screen.getByRole("button", { name: "Open dialog actions" });
		trigger.focus();
		await user.keyboard("{Enter}");
		await user.keyboard("{Enter}");

		const input = await screen.findByRole("textbox", { name: "Session title" });
		expect(screen.queryByRole("menu")).toBeNull();
		expect(document.activeElement).toBe(input);
		await user.keyboard("{Escape}");
		await waitFor(() => expect(document.activeElement).toBe(trigger));
	});
});

function DialogActionHarness() {
	const [open, setOpen] = useState(false);
	return (
		<>
			<ActionMenu
				triggerLabel="Open dialog actions"
				items={[{
					id: "rename",
					label: "Rename…",
					focusDestination: "dialog",
					onSelect: () => setOpen(true),
				}]}
			/>
			{open ? (
				<RenameSessionDialog
					value="Menu session"
					onChange={() => {}}
					onClose={() => setOpen(false)}
					onSubmit={() => {}}
				/>
			) : null}
		</>
	);
}

function session(): SessionSummary {
	return {
		session_id: "session-1",
		project_id: null,
		outer_cwd: "/workspace",
		workspaces: [],
		activity: "idle",
		active_leaf_id: null,
		provider: { kind: "openai", model: "gpt-test" },
		metadata: { title: "Menu session" },
		created_at: "2024-01-01T00:00:00Z",
		updated_at: "2024-01-01T00:00:00Z",
	};
}
