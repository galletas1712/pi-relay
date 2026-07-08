// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState, type ReactNode, type RefObject } from "react";
import { renderToStaticMarkup } from "react-dom/server";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { ActionMenu } from "./actionMenu.tsx";
import { DeleteSessionDialog, ProjectDialog, RenameSessionDialog } from "./entityDialogs.tsx";
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
		it("opens a dialog action from the keyboard and returns focus to the menu trigger", async () => {
			const user = userEvent.setup();
			render(
				<DialogActionHarness
					label="Rename…"
					onLaunch={() => {}}
					dialog={(onClose) => (
						<RenameSessionDialog
							value="Keyboard session"
							onChange={() => {}}
							onClose={onClose}
							onSubmit={() => {}}
						/>
					)}
				/>,
			);

			const trigger = screen.getByRole("button", { name: "Open dialog actions" });
			trigger.focus();
			await user.keyboard("{Enter}");
			await user.keyboard("{Enter}");

			await waitFor(() => {
				expect(document.activeElement).toBe(screen.getByRole("textbox", { name: "Session title" }));
			});
			await user.keyboard("{Escape}");
			await waitFor(() => expect(document.activeElement).toBe(trigger));
		});

		it.each([
			{
				name: "Rename",
				menuLabel: "Rename…",
				dialog: (onClose: () => void) => (
					<RenameSessionDialog
						value="Menu session"
						onChange={() => {}}
						onClose={onClose}
						onSubmit={() => {}}
					/>
				),
				destination: () => screen.getByRole("textbox", { name: "Session title" }),
			},
			{
				name: "Delete",
				menuLabel: "Delete…",
				dialog: (onClose: () => void) => (
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
						onClose={onClose}
						onConfirm={() => {}}
					/>
				),
				destination: () => screen.getByRole("button", { name: "Cancel" }),
			},
			{
				name: "Project settings",
				menuLabel: "Project settings…",
				dialog: (onClose: () => void) => (
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
						onClose={onClose}
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

			await user.keyboard("{Escape}");
			await waitFor(() => {
				expect(screen.queryByRole("dialog")).toBeNull();
				expect(screen.queryByRole("alertdialog")).toBeNull();
				expect(document.activeElement).toBe(trigger);
			});
		});

		it.each([
			["Rename", "Rename…"],
			["Project", "Project settings…"],
			["Delete", "Delete…"],
		] as const)(
			"returns focus to the visible overlay-sidebar toggle after cancelling %s",
			async (dialogKind, menuLabel) => {
				const user = userEvent.setup();
				render(<ResponsiveDialogActionHarness panelMode="compact" dialogKind={dialogKind} />);

				const trigger = screen.getByRole("button", { name: "Open sidebar actions" });
				await user.click(trigger);
				await user.click(await screen.findByRole("menuitem", { name: menuLabel }));

				const sidebar = screen.getByTestId("responsive-sidebar");
				await waitFor(() => {
					expect(sidebar.hasAttribute("inert")).toBe(true);
					expect(trigger.isConnected).toBe(true);
				});
				await user.click(await screen.findByRole("button", { name: "Cancel" }));

				const fallback = screen.getByRole("button", { name: "open projects and sessions" });
				await waitFor(() => {
					expect(screen.queryByRole("dialog")).toBeNull();
					expect(screen.queryByRole("alertdialog")).toBeNull();
					expect(document.activeElement).toBe(fallback);
				});
				expect(document.activeElement).not.toBe(trigger);
				expect(document.activeElement).not.toBe(document.body);
			},
		);

		it.each([
			["compact", "Rename", "Rename…", "Save"],
			["compact", "Project", "Project settings…", "Save"],
			["compact", "Delete", "Delete…", "Delete"],
			["medium", "Rename", "Rename…", "Save"],
			["medium", "Project", "Project settings…", "Save"],
			["medium", "Delete", "Delete…", "Delete"],
		] as const)(
			"returns focus to the visible sidebar toggle after successful %s-overlay %s",
			async (panelMode, dialogKind, menuLabel, submitLabel) => {
				const user = userEvent.setup();
				render(<ResponsiveDialogActionHarness panelMode={panelMode} dialogKind={dialogKind} />);

				const trigger = screen.getByRole("button", { name: "Open sidebar actions" });
				await user.click(trigger);
				await user.click(await screen.findByRole("menuitem", { name: menuLabel }));
				expect(screen.getByTestId("responsive-sidebar").hasAttribute("inert")).toBe(true);
				await user.click(await screen.findByRole("button", { name: submitLabel }));

				const fallback = screen.getByRole("button", { name: "open projects and sessions" });
				await waitFor(() => {
					expect(screen.queryByRole("dialog")).toBeNull();
					expect(screen.queryByRole("alertdialog")).toBeNull();
					expect(document.activeElement).toBe(fallback);
				});
				expect(document.activeElement).not.toBe(document.body);
			},
		);

		it("returns focus to the original trigger in a wide static sidebar", async () => {
			const user = userEvent.setup();
			render(<ResponsiveDialogActionHarness panelMode="wide" dialogKind="Rename" />);

			const trigger = screen.getByRole("button", { name: "Open sidebar actions" });
			await user.click(trigger);
			await user.click(await screen.findByRole("menuitem", { name: "Rename…" }));
			expect(screen.getByTestId("responsive-sidebar").hasAttribute("inert")).toBe(false);
			await user.click(await screen.findByRole("button", { name: "Cancel" }));

			await waitFor(() => expect(document.activeElement).toBe(trigger));
		});

		it.each([
			["Rename", "Rename…"],
			["Project", "Project settings…"],
		] as const)("returns focus to the original wide-sidebar trigger after successful %s", async (dialogKind, menuLabel) => {
			const user = userEvent.setup();
			render(<ResponsiveDialogActionHarness panelMode="wide" dialogKind={dialogKind} />);

			const trigger = screen.getByRole("button", { name: "Open sidebar actions" });
			await user.click(trigger);
			await user.click(await screen.findByRole("menuitem", { name: menuLabel }));
			await user.click(await screen.findByRole("button", { name: "Save" }));

			await waitFor(() => {
				expect(screen.queryByRole("dialog")).toBeNull();
				expect(document.activeElement).toBe(trigger);
			});
		});

		it("uses a stable wide-sidebar fallback after delete success removes the original row", async () => {
			const user = userEvent.setup();
			render(<ResponsiveDialogActionHarness panelMode="wide" dialogKind="Delete" />);

			const trigger = screen.getByRole("button", { name: "Open sidebar actions" });
			const fallback = screen.getByRole("button", { name: "New session" });
			await user.click(trigger);
			await user.click(await screen.findByRole("menuitem", { name: "Delete…" }));
			await user.click(await screen.findByRole("button", { name: "Delete" }));

			await waitFor(() => {
				expect(screen.queryByRole("alertdialog")).toBeNull();
				expect(trigger.isConnected).toBe(false);
				expect(document.activeElement).toBe(fallback);
			});
			expect(document.activeElement).not.toBe(document.body);
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
	dialog: (onClose: () => void) => ReactNode;
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
			{dialogOpen ? dialog(() => setDialogOpen(false)) : null}
		</>
	);
}

type ResponsiveDialogKind = "Rename" | "Project" | "Delete";

function ResponsiveDialogActionHarness({
	panelMode,
	dialogKind,
}: {
	panelMode: "compact" | "medium" | "wide";
	dialogKind: ResponsiveDialogKind;
}) {
	const overlay = panelMode !== "wide";
	const [dialogOpen, setDialogOpen] = useState(false);
	const [sidebarClosed, setSidebarClosed] = useState(false);
	const [rowPresent, setRowPresent] = useState(true);
	const mobileToggleRef = useRef<HTMLButtonElement>(null);
	const newSessionRef = useRef<HTMLButtonElement>(null);
	const fallbackRef = overlay ? mobileToggleRef : newSessionRef;
	const close = () => setDialogOpen(false);

	return (
		<>
			{overlay ? (
				<button ref={mobileToggleRef} type="button" aria-label="open projects and sessions">
					Open sidebar
				</button>
			) : null}
			<aside data-testid="responsive-sidebar" inert={overlay && sidebarClosed}>
				{!overlay ? <button ref={newSessionRef} type="button">New session</button> : null}
				{rowPresent ? (
					<ActionMenu
						triggerLabel="Open sidebar actions"
						items={[
							{
								id: "dialog",
								label:
									dialogKind === "Rename"
										? "Rename…"
										: dialogKind === "Project"
											? "Project settings…"
											: "Delete…",
								focusDestination: "dialog",
								onSelect: () => {
									setDialogOpen(true);
									if (overlay) setSidebarClosed(true);
								},
							},
						]}
					/>
				) : null}
			</aside>
			{dialogOpen
				? responsiveDialog(dialogKind, fallbackRef, close, () => {
						setRowPresent(false);
						close();
				  })
				: null}
		</>
	);
}

function responsiveDialog(
	dialogKind: ResponsiveDialogKind,
	returnFocusFallbackRef: RefObject<HTMLElement | null>,
	onClose: () => void,
	onDelete: () => void,
) {
	if (dialogKind === "Rename") {
		return (
			<RenameSessionDialog
				value="Responsive session"
				onChange={() => {}}
				onClose={onClose}
				onSubmit={onClose}
				returnFocusFallbackRef={returnFocusFallbackRef}
			/>
		);
	}
	if (dialogKind === "Project") {
		return (
			<ProjectDialog
				state={{
					mode: "edit",
					projectId: "project-1",
					name: "Responsive project",
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
				onClose={onClose}
				onSubmit={onClose}
				returnFocusFallbackRef={returnFocusFallbackRef}
			/>
		);
	}
	return (
		<DeleteSessionDialog
			session={{
				session_id: "session-responsive",
				project_id: null,
				outer_cwd: "/workspace",
				workspaces: [],
				activity: "idle",
				active_leaf_id: null,
				provider: { kind: "openai", model: "gpt-test" },
				metadata: { title: "Responsive session" },
				created_at: "2024-01-01T00:00:00Z",
				updated_at: "2024-01-01T00:00:00Z",
			}}
			deleting={false}
			onClose={onClose}
			onConfirm={onDelete}
			returnFocusFallbackRef={returnFocusFallbackRef}
		/>
	);
}
