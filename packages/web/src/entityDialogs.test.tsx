// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState, type ReactNode, type RefObject } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { isValidFocusReturnTarget } from "./dialog.tsx";
import {
	DeleteSessionDialog,
	ProjectDialog,
	RenameSessionDialog,
	type ProjectDialogState,
} from "./entityDialogs.tsx";
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

const session: SessionSummary = {
	session_id: "session-1",
	project_id: null,
	outer_cwd: "/workspace",
	workspaces: [],
	activity: "idle",
	active_leaf_id: null,
	provider: { kind: "openai", model: "gpt-test" },
	metadata: { title: "Dialog session" },
	created_at: "2024-01-01T00:00:00Z",
	updated_at: "2024-01-01T00:00:00Z",
};

function gitProjectState(mode: "create" | "edit" = "create"): ProjectDialogState {
	return {
		mode,
		projectId: mode === "edit" ? "project-1" : undefined,
		name: mode === "edit" ? "Existing project" : "",
		workspaces: [
			{
				kind: "git",
				workspace_dir: "pi-relay",
				remote_url: "https://example.com/pi-relay.git",
				remote_branch: "main",
			},
		],
		saving: false,
	};
}

describe("dialog focus return targets", () => {
	it.each([
		["inert", "inert", ""],
		["hidden", "hidden", ""],
		["aria-hidden", "aria-hidden", "true"],
		["disabled", "disabled", ""],
		["aria-disabled", "aria-disabled", "true"],
	] as const)("rejects a connected target inside a %s ancestor", (_name, attribute, value) => {
		const ancestor = document.createElement("div");
		const target = document.createElement("button");
		ancestor.setAttribute(attribute, value);
		ancestor.append(target);
		document.body.append(ancestor);

		expect(target.isConnected).toBe(true);
		expect(isValidFocusReturnTarget(target)).toBe(false);
		ancestor.remove();
	});

	it("accepts a connected target without an explicitly unavailable ancestor", () => {
		const target = document.createElement("button");
		document.body.append(target);

		expect(isValidFocusReturnTarget(target)).toBe(true);
		expect(isValidFocusReturnTarget(document.body)).toBe(false);
		target.remove();
	});
});

function DialogLauncher({
	children,
}: {
	children: (close: () => void) => ReactNode;
}) {
	const [open, setOpen] = useState(false);
	return (
		<>
			<button type="button" onClick={() => setOpen(true)}>Open dialog</button>
			{open ? children(() => setOpen(false)) : null}
		</>
	);
}

function OverlayDialogLauncher({
	children,
}: {
	children: (close: () => void, fallbackRef: RefObject<HTMLElement | null>) => ReactNode;
}) {
	const [open, setOpen] = useState(false);
	const [sidebarClosed, setSidebarClosed] = useState(false);
	const fallbackRef = useRef<HTMLButtonElement>(null);
	return (
		<>
			<button ref={fallbackRef} type="button" aria-label="open projects and sessions">
				Open sidebar
			</button>
			<aside inert={sidebarClosed}>
				<button
					type="button"
					onClick={() => {
						setOpen(true);
						setSidebarClosed(true);
					}}
				>
					Open overlay dialog
				</button>
			</aside>
			{open ? children(() => setOpen(false), fallbackRef) : null}
		</>
	);
}

function deferred<T>() {
	let resolve!: (value: T | PromiseLike<T>) => void;
	let reject!: (reason?: unknown) => void;
	const promise = new Promise<T>((resolvePromise, rejectPromise) => {
		resolve = resolvePromise;
		reject = rejectPromise;
	});
	return { promise, resolve, reject };
}

async function expectOverlayToggleFocus() {
	await waitFor(() => {
		expect(screen.queryByRole("dialog")).toBeNull();
		expect(screen.queryByRole("alertdialog")).toBeNull();
		expect(document.activeElement).toBe(screen.getByRole("button", { name: "open projects and sessions" }));
	});
}

describe("RenameSessionDialog", () => {
	it("focuses and selects the title, traps Tab, and passively closes with Escape without submitting", async () => {
		const onSubmit = vi.fn();
		const user = userEvent.setup();
		render(
			<DialogLauncher>
				{(close) => (
					<RenameHarness onClose={close} onSubmit={onSubmit} />
				)}
			</DialogLauncher>,
		);

		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);
		const input = await screen.findByRole<HTMLInputElement>("textbox", { name: "Session title" });
		expect(document.activeElement).toBe(input);
		expect(input.selectionStart).toBe(0);
		expect(input.selectionEnd).toBe("Dialog session".length);

		await user.tab();
		expect(document.activeElement).toBe(screen.getByRole("button", { name: "Cancel" }));
		await user.tab();
		const save = screen.getByRole("button", { name: "Save" });
		expect(document.activeElement).toBe(save);
		await user.tab();
		expect(document.activeElement).toBe(screen.getByRole("button", { name: "close rename dialog" }));

		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(document.activeElement).toBe(opener);
		expect(onSubmit).not.toHaveBeenCalled();
	});

	it("cancels without submitting and restores opener focus", async () => {
		const onSubmit = vi.fn();
		const user = userEvent.setup();
		render(
			<DialogLauncher>
				{(close) => <RenameHarness onClose={close} onSubmit={onSubmit} />}
			</DialogLauncher>,
		);

		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);
		await user.click(await screen.findByRole("button", { name: "Cancel" }));

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(document.activeElement).toBe(opener);
		expect(onSubmit).not.toHaveBeenCalled();
	});

	it("submits the edited title once and restores opener focus", async () => {
		const onSubmit = vi.fn();
		const user = userEvent.setup();
		render(
			<DialogLauncher>
				{(close) => <RenameHarness onClose={close} onSubmit={onSubmit} closeOnSubmit />}
			</DialogLauncher>,
		);

		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);
		const input = await screen.findByRole("textbox", { name: "Session title" });
		await user.clear(input);
		await user.type(input, "Renamed once");
		await user.dblClick(screen.getByRole("button", { name: "Save" }));

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(onSubmit).toHaveBeenCalledTimes(1);
		expect(onSubmit).toHaveBeenCalledWith("Renamed once");
		expect(document.activeElement).toBe(opener);
	});

	it("locks dismissal, editing, and duplicate submission while the rename request is pending", async () => {
		let resolveSubmit!: () => void;
		const pending = new Promise<void>((resolve) => {
			resolveSubmit = resolve;
		});
		const onSubmit = vi.fn(() => pending);
		const onClose = vi.fn();
		const user = userEvent.setup();
		render(
			<RenameSessionDialog
				value="Dialog session"
				onChange={() => {}}
				onClose={onClose}
				onSubmit={onSubmit}
			/>,
		);

		await user.dblClick(screen.getByRole("button", { name: "Save" }));
		expect(onSubmit).toHaveBeenCalledTimes(1);
		expect((screen.getByRole("button", { name: "Saving…" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "close rename dialog" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("textbox", { name: "Session title" }) as HTMLInputElement).disabled).toBe(true);
		await user.keyboard("{Escape}");
		expect(screen.getByRole("dialog")).toBeTruthy();
		expect(onClose).not.toHaveBeenCalled();

		resolveSubmit();
		await waitFor(() => {
			expect((screen.getByRole("button", { name: "Save" }) as HTMLButtonElement).disabled).toBe(false);
		});
	});

	it("stays modal and retryable after rejection, then cancel restores the overlay toggle", async () => {
		const firstAttempt = deferred<void>();
		const onSubmit = vi.fn()
			.mockImplementationOnce(() => firstAttempt.promise)
			.mockResolvedValueOnce(undefined);
		const user = userEvent.setup();
		render(
			<OverlayDialogLauncher>
				{(close, fallbackRef) => (
					<RenameSessionDialog
						value="Dialog session"
						onChange={() => {}}
						onClose={close}
						onSubmit={onSubmit}
						returnFocusFallbackRef={fallbackRef}
					/>
				)}
			</OverlayDialogLauncher>,
		);

		await user.click(screen.getByRole("button", { name: "Open overlay dialog" }));
		await user.dblClick(await screen.findByRole("button", { name: "Save" }));
		expect(onSubmit).toHaveBeenCalledTimes(1);
		expect(screen.getByRole("button", { name: "Saving…" })).toBeTruthy();

		firstAttempt.reject(new Error("rename failed"));
		const save = await screen.findByRole("button", { name: "Save" });
		await waitFor(() => expect(document.activeElement).toBe(save));
		expect(screen.getByRole("dialog", { name: "Rename session" }).contains(document.activeElement)).toBe(true);
		expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);

		await user.click(save);
		await waitFor(() => expect(onSubmit).toHaveBeenCalledTimes(2));
		await waitFor(() => expect(screen.getByRole("button", { name: "Save" })).toBeTruthy());
		await user.click(screen.getByRole("button", { name: "Cancel" }));
		await expectOverlayToggleFocus();
	});

	it("closes on an idle outside pointer interaction without submitting", async () => {
		const onSubmit = vi.fn();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		render(
			<DialogLauncher>
				{(close) => <RenameHarness onClose={close} onSubmit={onSubmit} />}
			</DialogLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);
		await screen.findByRole("dialog");
		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(onSubmit).not.toHaveBeenCalled();
		expect(document.activeElement).toBe(opener);
	});

});

function RenameHarness({
	onClose,
	onSubmit,
	closeOnSubmit = false,
}: {
	onClose: () => void;
	onSubmit: (value: string) => void;
	closeOnSubmit?: boolean;
}) {
	const [value, setValue] = useState("Dialog session");
	return (
		<RenameSessionDialog
			value={value}
			onChange={setValue}
			onClose={onClose}
			onSubmit={() => {
				onSubmit(value);
				if (closeOnSubmit) onClose();
			}}
		/>
	);
}

describe("ProjectDialog", () => {
	it.each(["create", "edit"] as const)("initially focuses the project name in %s mode", async (mode) => {
		const user = userEvent.setup();
		render(
			<DialogLauncher>
				{(close) => (
					<ProjectHarness initialState={gitProjectState(mode)} onClose={close} onSubmit={vi.fn()} />
				)}
			</DialogLauncher>,
		);

		await user.click(screen.getByRole("button", { name: "Open dialog" }));
		const input = await screen.findByRole("textbox", { name: "Project name" });
		expect(document.activeElement).toBe(input);
		expect(screen.getByRole("dialog", { name: mode === "create" ? "New project" : "Project settings" })).toBeTruthy();
	});

	it("preserves native validation, edits workspace fields, submits once, and restores focus", async () => {
		const onSubmit = vi.fn();
		const user = userEvent.setup();
		render(
			<DialogLauncher>
				{(close) => (
					<ProjectHarness
						initialState={gitProjectState()}
						onClose={close}
						onSubmit={onSubmit}
						closeOnSubmit
					/>
				)}
			</DialogLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);

		await user.click(screen.getByRole("button", { name: "Save" }));
		expect(onSubmit).not.toHaveBeenCalled();
		const name = screen.getByRole("textbox", { name: "Project name" });
		await user.type(name, "Created project");
		const workspaceName = screen.getByRole("textbox", { name: "Name" });
		await user.clear(workspaceName);
		await user.type(workspaceName, "new-workspace");
		await user.dblClick(screen.getByRole("button", { name: "Save" }));

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(onSubmit).toHaveBeenCalledTimes(1);
		expect(onSubmit.mock.calls[0]?.[0]).toMatchObject({
			name: "Created project",
			workspaces: [{ workspace_dir: "new-workspace" }],
		});
		expect(document.activeElement).toBe(opener);
	});

	it.each([
		["Cancel", "button"],
		["close project dialog", "button"],
		["Escape", "escape"],
		["outside pointer", "outside"],
	] as const)("supports idle %s close without saving and restores focus", async (_name, closeMethod) => {
		const onSubmit = vi.fn();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		render(
			<DialogLauncher>
				{(close) => (
					<ProjectHarness initialState={gitProjectState("edit")} onClose={close} onSubmit={onSubmit} />
				)}
			</DialogLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);
		await screen.findByRole("dialog");

		if (closeMethod === "escape") await user.keyboard("{Escape}");
		else if (closeMethod === "outside") await user.click(document.querySelector(".dialog-overlay") as HTMLElement);
		else await user.click(screen.getByRole("button", { name: _name }));

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(onSubmit).not.toHaveBeenCalled();
		expect(document.activeElement).toBe(opener);
	});

	it("locks close, Escape, outside dismissal, form controls, and repeated submit while saving", async () => {
		const pending = new Promise<void>(() => {});
		const onSubmit = vi.fn(() => pending);
		const onClose = vi.fn();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		const { rerender } = render(
			<ProjectDialog
				state={gitProjectState("edit")}
				onChange={vi.fn()}
				onClose={onClose}
				onSubmit={onSubmit}
			/>,
		);
		await user.dblClick(screen.getByRole("button", { name: "Save" }));
		expect(onSubmit).toHaveBeenCalledTimes(1);
		expect((screen.getByRole("button", { name: "Saving…" }) as HTMLButtonElement).disabled).toBe(true);

		rerender(
			<ProjectDialog
				state={{ ...gitProjectState("edit"), saving: true }}
				onChange={vi.fn()}
				onClose={onClose}
				onSubmit={onSubmit}
			/>,
		);
		expect((screen.getByRole("button", { name: "Saving…" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "close project dialog" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("textbox", { name: "Project name" }) as HTMLInputElement).disabled).toBe(true);

		await user.keyboard("{Escape}");
		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);
		fireEvent.click(screen.getByRole("button", { name: "Saving…" }));
		expect(screen.getByRole("dialog")).toBeTruthy();
		expect(onClose).not.toHaveBeenCalled();
		expect(onSubmit).toHaveBeenCalledTimes(1);
	});

	it("keeps rejected saves recoverable and focused in the modal, then restores overlay focus on close", async () => {
		const attempt = deferred<void>();
		const onSubmit = vi.fn(() => attempt.promise);
		const user = userEvent.setup();
		render(
			<OverlayDialogLauncher>
				{(close, fallbackRef) => (
					<ProjectHarness
						initialState={gitProjectState("edit")}
						onClose={close}
						onSubmit={onSubmit}
						returnFocusFallbackRef={fallbackRef}
					/>
				)}
			</OverlayDialogLauncher>,
		);

		await user.click(screen.getByRole("button", { name: "Open overlay dialog" }));
		await user.dblClick(await screen.findByRole("button", { name: "Save" }));
		expect(onSubmit).toHaveBeenCalledTimes(1);
		expect(screen.getByRole("button", { name: "Saving…" })).toBeTruthy();

		attempt.reject(new Error("project failed"));
		const save = await screen.findByRole("button", { name: "Save" });
		await waitFor(() => expect(document.activeElement).toBe(save));
		expect(screen.getByRole("dialog", { name: "Project settings" }).contains(document.activeElement)).toBe(true);
		expect((screen.getByRole("textbox", { name: "Project name" }) as HTMLInputElement).disabled).toBe(false);

		await user.click(screen.getByRole("button", { name: "Cancel" }));
		await expectOverlayToggleFocus();
	});
});

function ProjectHarness({
	initialState,
	onClose,
	onSubmit,
	closeOnSubmit = false,
	returnFocusFallbackRef,
}: {
	initialState: ProjectDialogState;
	onClose: () => void;
	onSubmit: (state: ProjectDialogState) => void | Promise<void>;
	closeOnSubmit?: boolean;
	returnFocusFallbackRef?: RefObject<HTMLElement | null>;
}) {
	const [state, setState] = useState(initialState);
	return (
		<ProjectDialog
			state={state}
			onChange={(patch) => setState((current) => ({ ...current, ...patch }))}
			onClose={onClose}
			onSubmit={() => {
				const result = onSubmit(state);
				if (closeOnSubmit) onClose();
				return result;
			}}
			returnFocusFallbackRef={returnFocusFallbackRef}
		/>
	);
}

describe("DeleteSessionDialog", () => {
	it("uses described alertdialog semantics, focuses safe Cancel, and blocks idle outside clicks", async () => {
		const onClose = vi.fn();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		render(
			<DeleteSessionDialog
				session={session}
				deleting={false}
				onClose={onClose}
				onConfirm={vi.fn()}
			/>,
		);

		const dialog = await screen.findByRole("alertdialog", { name: "Delete session" });
		const descriptionId = dialog.getAttribute("aria-describedby");
		expect(descriptionId).toBeTruthy();
		expect(document.getElementById(descriptionId ?? "")?.textContent).toMatch(
			/This removes the transcript.*cannot be undone/i,
		);
		expect(document.activeElement).toBe(screen.getByRole("button", { name: "Cancel" }));
		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);
		expect(screen.getByRole("alertdialog")).toBeTruthy();
		expect(onClose).not.toHaveBeenCalled();
	});

	it.each([
		["Escape", "escape"],
		["Cancel", "cancel"],
	] as const)("closes with %s without confirming and restores focus", async (_name, closeMethod) => {
		const onConfirm = vi.fn();
		const user = userEvent.setup();
		render(
			<DialogLauncher>
				{(close) => (
					<DeleteSessionDialog
						session={session}
						deleting={false}
						onClose={close}
						onConfirm={onConfirm}
					/>
				)}
			</DialogLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open dialog" });
		await user.click(opener);
		await screen.findByRole("alertdialog");

		if (closeMethod === "escape") await user.keyboard("{Escape}");
		else await user.click(screen.getByRole("button", { name: "Cancel" }));

		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(onConfirm).not.toHaveBeenCalled();
		expect(document.activeElement).toBe(opener);
	});

	it("invokes the destructive callback once, locks dismissal while busy, and restores focus after success", async () => {
		const onConfirm = vi.fn();
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		render(<DeleteHarness onConfirm={onConfirm} />);
		const opener = screen.getByRole("button", { name: "Open dialog" });
		const completeDeletion = screen.getByRole("button", { name: "Complete deletion" });
		await user.click(opener);
		await user.dblClick(await screen.findByRole("button", { name: "Delete" }));

		expect(onConfirm).toHaveBeenCalledTimes(1);
		expect((screen.getByRole("button", { name: "Deleting…" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getByRole("button", { name: "close delete dialog" }) as HTMLButtonElement).disabled).toBe(true);
		await user.keyboard("{Escape}");
		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);
		fireEvent.click(screen.getByRole("button", { name: "Deleting…" }));
		expect(screen.getByRole("alertdialog")).toBeTruthy();
		expect(onConfirm).toHaveBeenCalledTimes(1);

		// Radix correctly hides background controls from the accessibility tree
		// while modal; the harness uses this retained node to model server success.
		fireEvent.click(completeDeletion);
		await waitFor(() => expect(screen.queryByRole("alertdialog")).toBeNull());
		expect(document.activeElement).toBe(opener);
	});

	it("keeps a rejected deletion safe and focused, unlocks retry, and restores the overlay toggle on cancel", async () => {
		const attempt = deferred<void>();
		const onConfirm = vi.fn(() => attempt.promise);
		const user = userEvent.setup();
		render(
			<OverlayDialogLauncher>
				{(close, fallbackRef) => (
					<DeleteSessionDialog
						session={session}
						deleting={false}
						onClose={close}
						onConfirm={onConfirm}
						returnFocusFallbackRef={fallbackRef}
					/>
				)}
			</OverlayDialogLauncher>,
		);

		await user.click(screen.getByRole("button", { name: "Open overlay dialog" }));
		await user.dblClick(await screen.findByRole("button", { name: "Delete" }));
		expect(onConfirm).toHaveBeenCalledTimes(1);
		expect(screen.getByRole("button", { name: "Deleting…" })).toBeTruthy();

		attempt.reject(new Error("delete failed"));
		const confirm = await screen.findByRole("button", { name: "Delete" });
		await waitFor(() => expect(document.activeElement).toBe(confirm));
		expect(screen.getByRole("alertdialog", { name: "Delete session" }).contains(document.activeElement)).toBe(true);
		expect((screen.getByRole("button", { name: "Cancel" }) as HTMLButtonElement).disabled).toBe(false);

		await user.click(screen.getByRole("button", { name: "Cancel" }));
		await expectOverlayToggleFocus();
	});
});

function DeleteHarness({ onConfirm }: { onConfirm: () => void }) {
	const [open, setOpen] = useState(false);
	const [deleting, setDeleting] = useState(false);
	return (
		<>
			<button type="button" onClick={() => setOpen(true)}>Open dialog</button>
			<button type="button" onClick={() => setOpen(false)}>Complete deletion</button>
			{open ? (
				<DeleteSessionDialog
					session={session}
					deleting={deleting}
					onClose={() => setOpen(false)}
					onConfirm={() => {
						if (deleting) return;
						onConfirm();
						setDeleting(true);
					}}
				/>
			) : null}
		</>
	);
}
