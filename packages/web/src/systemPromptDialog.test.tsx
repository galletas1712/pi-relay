// @vitest-environment jsdom

import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useState, type ReactNode } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import {
	SystemPromptDialog,
	type SystemPromptDialogState,
} from "./systemPromptDialog.tsx";

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

const contentState: SystemPromptDialogState = {
	loading: false,
	template: "Template instructions",
	rendered: "Rendered **instructions**",
	view: "rendered",
	error: null,
};

describe("SystemPromptDialog", () => {
	it.each(["escape", "outside"] as const)(
		"starts on a stable heading, closes via %s without changing content, and restores opener focus",
		async (closeMethod) => {
			const onChangeView = vi.fn();
			const user = userEvent.setup({ pointerEventsCheck: 0 });
			render(
				<DialogLauncher>
					{(close) => (
						<SystemPromptDialog
							state={contentState}
							onChangeView={onChangeView}
							onClose={close}
						/>
					)}
				</DialogLauncher>,
			);
			const opener = screen.getByRole("button", { name: "Open system prompt" });
			await user.click(opener);
			const heading = await screen.findByRole("heading", { name: "PI.md" });

			expect(document.activeElement).toBe(heading);
			expect(screen.getByRole("dialog", { name: "PI.md" }).getAttribute("aria-describedby")).toBeTruthy();
			expect(screen.getByRole("button", { name: "close system prompt dialog" })).toBeTruthy();
			expect(screen.queryByRole("tablist")).toBeNull();
			expect(screen.getByRole("group", { name: "PI.md view" })).toBeTruthy();
			expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(1);
			expect(document.querySelectorAll(".dialog-overlay")).toHaveLength(1);
			expect(document.querySelector(".modal-scrim")).toBeNull();

			if (closeMethod === "escape") await user.keyboard("{Escape}");
			else await user.click(document.querySelector(".dialog-overlay") as HTMLElement);

			await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
			expect(document.activeElement).toBe(opener);
			expect(onChangeView).not.toHaveBeenCalled();
		},
	);

	it("keeps heading focus through loading and renders loading, error, and content states", async () => {
		const loadingState: SystemPromptDialogState = {
			loading: true,
			template: "",
			rendered: null,
			view: "rendered",
			error: null,
		};
		const { rerender } = render(
			<SystemPromptDialog
				state={loadingState}
				onChangeView={() => undefined}
				onClose={() => undefined}
			/>,
		);
		const heading = await screen.findByRole("heading", { name: "PI.md" });
		expect(document.activeElement).toBe(heading);
		expect(screen.getByText("Loading PI.md…")).toBeTruthy();

		rerender(
			<SystemPromptDialog
				state={{ ...loadingState, loading: false, view: "template", error: "PI.md failed" }}
				onChangeView={() => undefined}
				onClose={() => undefined}
			/>,
		);
		expect(screen.getByText("PI.md failed")).toBeTruthy();
		expect(document.activeElement).toBe(heading);

		rerender(
			<SystemPromptDialog
				state={contentState}
				onChangeView={() => undefined}
				onClose={() => undefined}
			/>,
		);
		expect(screen.getByText("instructions", { exact: false })).toBeTruthy();
		expect(document.activeElement).toBe(heading);
	});

	it("preserves rendered/template switching and pressed-button state", async () => {
		const user = userEvent.setup();
		render(<SystemPromptHarness />);

		const rendered = screen.getByRole("button", { name: "Rendered" });
		const template = screen.getByRole("button", { name: "Template" });
		expect(rendered.getAttribute("aria-pressed")).toBe("true");
		expect(template.getAttribute("aria-pressed")).toBe("false");
		expect(screen.getByText("instructions", { exact: false })).toBeTruthy();

		await user.click(template);
		expect(rendered.getAttribute("aria-pressed")).toBe("false");
		expect(template.getAttribute("aria-pressed")).toBe("true");
		expect(screen.getByText("Template instructions")).toBeTruthy();

		await user.click(rendered);
		expect(rendered.getAttribute("aria-pressed")).toBe("true");
		expect(screen.getByText("instructions", { exact: false })).toBeTruthy();
	});

	it("disables an unavailable rendered view without falsely exposing tabs", () => {
		render(
			<SystemPromptDialog
				state={{
					loading: false,
					template: "Template only",
					rendered: null,
					view: "template",
					error: null,
				}}
				onChangeView={() => undefined}
				onClose={() => undefined}
			/>,
		);

		expect((screen.getByRole("button", { name: "Rendered" }) as HTMLButtonElement).disabled).toBe(true);
		expect(screen.getByText("Template only")).toBeTruthy();
		expect(screen.queryByRole("tab")).toBeNull();
		expect(screen.queryByRole("tabpanel")).toBeNull();
	});
});

function DialogLauncher({ children }: { children: (close: () => void) => ReactNode }) {
	const [open, setOpen] = useState(false);
	return (
		<>
			<button type="button" onClick={() => setOpen(true)}>Open system prompt</button>
			{open ? children(() => setOpen(false)) : null}
		</>
	);
}

function SystemPromptHarness() {
	const [state, setState] = useState(contentState);
	return (
		<SystemPromptDialog
			state={state}
			onChangeView={(view) => setState((current) => ({ ...current, view }))}
			onClose={() => undefined}
		/>
	);
}
