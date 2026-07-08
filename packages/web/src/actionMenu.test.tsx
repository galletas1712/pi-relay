// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState, type ReactNode } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { DeleteSessionDialog, ProjectDialog, RenameSessionDialog } from "./App.tsx";
import { ActionMenu } from "./actionMenu.tsx";
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

afterEach(() => {
	cleanup();
});

describe("ActionMenu", () => {
	it("renders one named native trigger with Radix menu state semantics", () => {
		const html = renderToStaticMarkup(
			<ActionMenu
				triggerLabel="Open actions for Test"
				items={[{ id: "test", label: "Test action", onSelect: vi.fn() }]}
			/>,
		);

		expect(html).toContain('class="action-menu-trigger"');
		expect(html).toContain('type="button"');
		expect(html).toContain('aria-label="Open actions for Test"');
		expect(html).toContain('aria-haspopup="menu"');
		expect(html).toContain('aria-expanded="false"');
		expect(html).toContain('data-state="closed"');
		expect(html).not.toContain('role="button"');
	});

	it("supports controlled open state without exposing the headless primitive", () => {
		const html = renderToStaticMarkup(
			<ActionMenu
				triggerLabel="Open actions for Test"
				items={[{ id: "test", label: "Test action", onSelect: vi.fn() }]}
				open
				onOpenChange={vi.fn()}
			/>,
		);

		expect(html).toContain('aria-expanded="true"');
		expect(html).toContain('data-state="open"');
	});

	it("closes with Escape without invoking an action and restores focus to the trigger", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup();
		render(
			<ActionMenu
				triggerLabel="Open test actions"
				items={[{ id: "test", label: "Test action", onSelect }]}
			/>,
		);

		const trigger = screen.getByRole("button", { name: "Open test actions" });
		await user.click(trigger);
		expect(await screen.findByRole("menu")).toBeTruthy();

		await user.keyboard("{Escape}");

		await waitFor(() => {
			expect(screen.queryByRole("menu")).toBeNull();
			expect(document.activeElement).toBe(trigger);
		});
		expect(onSelect).not.toHaveBeenCalled();
	});

	it("closes on an outside interaction without invoking an action and restores focus to the trigger", async () => {
		const onSelect = vi.fn();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		render(
			<>
				<ActionMenu
					triggerLabel="Open test actions"
					items={[{ id: "test", label: "Test action", onSelect }]}
				/>
				<button type="button">Outside target</button>
			</>,
		);

		const trigger = screen.getByRole("button", { name: "Open test actions" });
		const outsideTarget = screen.getByRole("button", { name: "Outside target" });
		await user.click(trigger);
		expect(await screen.findByRole("menu")).toBeTruthy();

		await user.click(outsideTarget);

		await waitFor(() => {
			expect(screen.queryByRole("menu")).toBeNull();
			expect(document.activeElement).toBe(trigger);
		});
		expect(onSelect).not.toHaveBeenCalled();
	});

	it("invokes an ordinary row action once, restores trigger focus, and never selects the row", async () => {
		const onSelectRow = vi.fn();
		const onArchive = vi.fn();
		const user = userEvent.setup();
		const session: SessionSummary = {
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
		render(
			<ul>
				<SessionRow
					session={session}
					selected={false}
					onSelect={onSelectRow}
					onRename={vi.fn()}
					onArchiveToggle={onArchive}
					onDelete={vi.fn()}
				/>
			</ul>,
		);

		const trigger = screen.getByRole("button", { name: "Open session actions for Menu session" });
		await user.click(trigger);
		expect(onSelectRow).not.toHaveBeenCalled();

		await user.click(await screen.findByRole("menuitem", { name: "Archive" }));

		await waitFor(() => {
			expect(screen.queryByRole("menu")).toBeNull();
			expect(document.activeElement).toBe(trigger);
		});
		expect(onArchive).toHaveBeenCalledTimes(1);
		expect(onSelectRow).not.toHaveBeenCalled();
	});

	describe("dialog focus handoff", () => {
		it.each([
			{
				name: "Rename",
				menuLabel: "Rename…",
				dialog: (
					<RenameSessionDialog
						value="Menu session"
						onChange={() => {}}
						onClose={() => {}}
						onSubmit={() => {}}
					/>
				),
				destination: () => screen.getByRole("textbox", { name: "Session title" }),
			},
			{
				name: "Delete",
				menuLabel: "Delete…",
				dialog: (
					<DeleteSessionDialog
						session={{
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
						}}
						deleting={false}
						onClose={() => {}}
						onConfirm={() => {}}
					/>
				),
				destination: () => screen.getByRole("button", { name: "Cancel" }),
			},
			{
				name: "Project settings",
				menuLabel: "Project settings…",
				dialog: (
					<ProjectDialog
						state={{
							mode: "edit",
							projectId: "project-1",
							name: "Menu project",
							workspaces: [
								{
									kind: "git",
									workspace_dir: "pi-relay",
									remote_url: "https://example.com/pi-relay.git",
									remote_branch: "main",
								},
							],
							saving: false,
						}}
						onChange={() => {}}
						onClose={() => {}}
						onSubmit={() => {}}
					/>
				),
				destination: () => screen.getByRole("textbox", { name: "Project name" }),
			},
		])("mounts $name only after menu cleanup and lets its intended initial focus win", async ({ menuLabel, dialog, destination }) => {
			const launchedWhileMenuMounted = vi.fn(() => document.querySelector('[role="menu"]') !== null);
			const user = userEvent.setup();
			render(
				<DialogActionHarness
					label={menuLabel}
					onLaunch={launchedWhileMenuMounted}
					dialog={dialog}
				/>,
			);

			const trigger = screen.getByRole("button", { name: "Open dialog actions" });
			await user.click(trigger);
			await user.click(await screen.findByRole("menuitem", { name: menuLabel }));

			await waitFor(() => {
				expect(launchedWhileMenuMounted).toHaveBeenCalledTimes(1);
				expect(screen.queryByRole("menu")).toBeNull();
				expect(document.activeElement).toBe(destination());
			});
			expect(launchedWhileMenuMounted).toHaveReturnedWith(false);
			expect(document.activeElement).not.toBe(document.body);
			expect(document.activeElement).not.toBe(trigger);
		});
	});
});

function DialogActionHarness({
	label,
	onLaunch,
	dialog,
}: {
	label: string;
	onLaunch: () => unknown;
	dialog: ReactNode;
}) {
	const [dialogOpen, setDialogOpen] = useState(false);
	return (
		<>
			<ActionMenu
				triggerLabel="Open dialog actions"
				items={[
					{
						id: "dialog",
						label,
						focusDestination: "dialog",
						onSelect: () => {
							onLaunch();
							setDialogOpen(true);
						},
					},
				]}
			/>
			{dialogOpen ? dialog : null}
		</>
	);
}
