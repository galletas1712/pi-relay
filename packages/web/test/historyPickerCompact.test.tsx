// @vitest-environment jsdom

import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { useRef, useState, type ReactNode, type RefObject } from "react";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import { CompactHistoryPickerDialog } from "../src/historyPickerCompact.tsx";
import type { TranscriptTreeNode } from "../src/types.ts";

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

const baseTime = Date.UTC(2026, 0, 1, 12, 0, 0);

function node(
	id: string,
	parentId: string | null,
	sequence: number,
	itemType: TranscriptTreeNode["item_type"],
	displayHint: string
): TranscriptTreeNode {
	return {
		id,
		parent_id: parentId,
		timestamp_ms: baseTime + sequence,
		sequence,
		item_type: itemType,
		turn_id: itemType === "turn_finished" ? sequence : null,
		outcome: itemType === "turn_finished" ? "Graceful" : null,
		can_switch_to: itemType === "turn_finished",
		edit_target_leaf_id: null,
		display_hint: displayHint
	};
}

function branchFixture(): TranscriptTreeNode[] {
	return [
		node("root", null, 1, "turn_finished", "root turn"),
		node("active", "root", 2, "turn_finished", "active branch"),
		node("alternate", "root", 3, "user_message", "alternate branch"),
		node("alternate-child", "alternate", 4, "turn_finished", "hidden alternate turn")
	];
}

function cssRule(source: string, selector: string): string {
	const escapedSelector = selector.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
	return source.match(new RegExp(`${escapedSelector}\\s*\\{([^}]*)\\}`))?.[1] ?? "";
}

function pixelDeclaration(rule: string, property: string): number {
	const value = rule.match(new RegExp(`${property}:\\s*(\\d+)px`))?.[1];
	return value ? Number(value) : 0;
}

describe("compact history branch disclosure", () => {
	it("labels fork targets as fork targets", () => {
		render(
			<CompactHistoryPickerDialog
				mode="fork"
				nodes={branchFixture()}
				activeLeafId="active"
				onClose={() => undefined}
				onSelect={() => undefined}
			/>
		);

		expect(screen.getByRole("list", { name: "fork targets" })).toBeTruthy();
		expect(screen.queryByRole("list", { name: "switch targets" })).toBeNull();
		expect(screen.getByText(/current workspace—not historical files—will be cloned/i)).toBeTruthy();
		expect(screen.getAllByText(/fork ·/).length).toBeGreaterThan(0);
		expect(screen.queryByText(/switch ·/)).toBeNull();
		expect(screen.getByText(/edit ·/)).toBeTruthy();
	});

	it("uses list/disclosure semantics with native keyboard traversal and activation", async () => {
		const onClose = vi.fn();
		const onSelect = vi.fn();
		const user = userEvent.setup();
		render(
			<CompactHistoryPickerDialog
				nodes={branchFixture()}
				activeLeafId="active"
				onClose={onClose}
				onSelect={onSelect}
			/>
		);
		const disclosure = screen.getByRole("button", {
			name: "Expand branch for User message: alternate branch, 1 hidden descendant",
		});
		const switchTarget = screen.getByRole("button", { name: "Switch to User message: alternate branch" });
		const collapsedBranch = disclosure.closest(".history-tree-item");
		const branchId = disclosure.getAttribute("aria-controls");

		expect(disclosure).not.toBe(switchTarget);
		expect(disclosure.getAttribute("aria-expanded")).toBe("false");
		expect(branchId).toBeTruthy();
		expect(document.getElementById(branchId!)?.hidden).toBe(true);
		expect(collapsedBranch?.classList.contains("collapsed")).toBe(true);
		expect(screen.queryByRole("button", { name: /hidden alternate turn/ })).toBeNull();
		expect(screen.getAllByRole("list").length).toBeGreaterThan(0);
		expect(screen.queryByRole("tree")).toBeNull();
		expect(screen.queryByRole("treeitem")).toBeNull();

		disclosure.focus();
		await user.keyboard(" ");
		expect(disclosure.getAttribute("aria-expanded")).toBe("true");
		expect(document.getElementById(branchId!)?.hidden).toBe(false);
		expect(screen.getByRole("button", { name: /hidden alternate turn/ })).toBeTruthy();
		expect(onSelect).not.toHaveBeenCalled();
		expect(onClose).not.toHaveBeenCalled();

		await user.tab();
		expect(document.activeElement).toBe(switchTarget);
		await user.keyboard("{Enter}");
		expect(onSelect).toHaveBeenCalledTimes(1);
		expect(onSelect.mock.calls[0]?.[0]).toMatchObject({
			id: "alternate",
			preview: "alternate branch",
		});
		expect(onClose).not.toHaveBeenCalled();
	});

	it("keeps stable heading focus while history loads, closes with Escape, and restores opener focus", async () => {
		const user = userEvent.setup();
		const { rerender } = render(
			<HistoryLauncher nodes={[]} loading>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={[]}
						activeLeafId={null}
						loading
						onClose={close}
						onSelect={vi.fn()}
					/>
				)}
			</HistoryLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open history" });
		await user.click(opener);
		const heading = await screen.findByRole("heading", { name: "Switch branch" });
		expect(document.activeElement).toBe(heading);
		expect(screen.getByText("Loading history index…")).toBeTruthy();
		expect(screen.getByRole("dialog", { name: "Switch branch" }).getAttribute("aria-describedby")).toBeTruthy();
		expect(document.querySelectorAll('[role="dialog"]')).toHaveLength(1);
		expect(document.querySelectorAll(".dialog-overlay")).toHaveLength(1);
		expect(document.querySelector(".modal-scrim")).toBeNull();

		rerender(
			<HistoryLauncher nodes={branchFixture()} loading={false}>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={branchFixture()}
						activeLeafId="active"
						onClose={close}
						onSelect={vi.fn()}
					/>
				)}
			</HistoryLauncher>,
		);
		expect(screen.getByRole("heading", { name: "Switch branch" })).toBe(heading);
		expect(document.activeElement).toBe(heading);
		expect(screen.getByRole("button", { name: /Switch to.*active branch/ }).getAttribute("aria-current")).toBe("true");

		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(document.activeElement).toBe(opener);
	});

	it("scrolls the current branch target into view once without moving heading focus", async () => {
		const rectSpy = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function () {
			if (this.getAttribute("aria-current") === "true") return rect(150, 180);
			if (this.classList.contains("history-options")) return rect(0, 100);
			return rect(0, 0);
		});
		const { rerender } = render(
			<CompactHistoryPickerDialog
				nodes={[]}
				activeLeafId="active"
				loading
				onClose={() => undefined}
				onSelect={() => undefined}
			/>,
		);
		const scroller = document.querySelector<HTMLElement>(".history-options");
		if (!scroller) throw new Error("missing history scroller");
		scroller.scrollTop = 10;
		rerender(
			<CompactHistoryPickerDialog
				nodes={branchFixture()}
				activeLeafId="active"
				onClose={() => undefined}
				onSelect={() => undefined}
			/>,
		);

		expect(scroller.scrollTop).toBe(90);
		expect(document.activeElement).toBe(screen.getByRole("heading", { name: "Switch branch" }));
		scroller.scrollTop = 25;
		rerender(
			<CompactHistoryPickerDialog
				nodes={[...branchFixture()]}
				activeLeafId="active"
				onClose={() => undefined}
				onSelect={() => undefined}
			/>,
		);
		expect(scroller.scrollTop).toBe(25);
		rectSpy.mockRestore();
	});

	it("falls back to the bottom when no current target exists", () => {
		const { rerender } = render(
			<CompactHistoryPickerDialog
				nodes={[]}
				activeLeafId={null}
				loading
				onClose={() => undefined}
				onSelect={() => undefined}
			/>,
		);
		const scroller = document.querySelector<HTMLElement>(".history-options");
		if (!scroller) throw new Error("missing history scroller");
		Object.defineProperties(scroller, {
			clientHeight: { configurable: true, value: 100 },
			scrollHeight: { configurable: true, value: 500 },
		});
		rerender(
			<CompactHistoryPickerDialog
				nodes={branchFixture()}
				activeLeafId={null}
				onClose={() => undefined}
				onSelect={() => undefined}
			/>,
		);

		expect(scroller.scrollTop).toBe(400);
	});

	it("reinitializes on close/reopen and waits for a late active row", async () => {
		const rectSpy = vi.spyOn(HTMLElement.prototype, "getBoundingClientRect").mockImplementation(function () {
			if (this.getAttribute("aria-current") === "true") return rect(140, 170);
			if (this.classList.contains("history-options")) return rect(0, 100);
			return rect(0, 0);
		});
		const partial = branchFixture().filter((node) => node.id !== "active");
		const user = userEvent.setup();
		const view = render(
			<HistoryLauncher nodes={[]} loading={false}>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={partial}
						activeLeafId="active"
						onClose={close}
						onSelect={() => undefined}
					/>
				)}
			</HistoryLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open history" });
		await user.click(opener);
		let list = document.querySelector<HTMLElement>(".history-options");
		if (!list) throw new Error("missing history scroller");
		list.scrollTop = 10;
		view.rerender(
			<HistoryLauncher nodes={[]} loading={false}>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={branchFixture()}
						activeLeafId="active"
						onClose={close}
						onSelect={() => undefined}
					/>
				)}
			</HistoryLauncher>,
		);
		expect(list.scrollTop).toBe(80);
		await user.keyboard("{Escape}");
		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());

		view.rerender(
			<HistoryLauncher nodes={[]} loading={false}>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={partial}
						activeLeafId="active"
						onClose={close}
						onSelect={() => undefined}
					/>
				)}
			</HistoryLauncher>,
		);
		await user.click(opener);
		list = document.querySelector<HTMLElement>(".history-options");
		if (!list) throw new Error("missing history scroller");
		list.scrollTop = 5;
		view.rerender(
			<HistoryLauncher nodes={[]} loading={false}>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={branchFixture()}
						activeLeafId="active"
						onClose={close}
						onSelect={() => undefined}
					/>
				)}
			</HistoryLauncher>,
		);
		expect(list.scrollTop).toBe(75);
		rectSpy.mockRestore();
	});

	it("closes on an outside pointer interaction and restores opener focus", async () => {
		const user = userEvent.setup({ pointerEventsCheck: 0 });
		render(
			<HistoryLauncher nodes={[]} loading={false}>
				{(close) => (
					<CompactHistoryPickerDialog
						nodes={[]}
						activeLeafId={null}
						onClose={close}
						onSelect={vi.fn()}
					/>
				)}
			</HistoryLauncher>,
		);
		const opener = screen.getByRole("button", { name: "Open history" });
		await user.click(opener);
		await screen.findByRole("dialog");
		await user.click(document.querySelector(".dialog-overlay") as HTMLElement);

		await waitFor(() => expect(screen.queryByRole("dialog")).toBeNull());
		expect(document.activeElement).toBe(opener);
	});

	it("keeps the compact disclosure visible with a coarse-pointer-sized target", async () => {
		const stylesPath = process.cwd().endsWith("packages/web")
			? resolve("src/styles.css")
			: resolve("packages/web/src/styles.css");
		const styles = await readFile(stylesPath, "utf8");
		const compactStart = styles.indexOf("@media (max-width: 760px)");
		const compactEnd = styles.indexOf("@media (max-width: 430px)", compactStart);
		const compactStyles = styles.slice(compactStart, compactEnd);
		const toggleRule = cssRule(compactStyles, ".branch-toggle");

		expect(compactStart).toBeGreaterThanOrEqual(0);
		expect(compactEnd).toBeGreaterThan(compactStart);
		expect(toggleRule).toContain("display: inline-flex");
		expect(toggleRule).not.toContain("display: none");
		expect(pixelDeclaration(toggleRule, "width")).toBeGreaterThanOrEqual(44);
		expect(pixelDeclaration(toggleRule, "height")).toBeGreaterThanOrEqual(44);
	});
});

function HistoryLauncher({
	children,
}: {
	nodes: TranscriptTreeNode[];
	loading: boolean;
	children: (close: () => void, fallbackRef: RefObject<HTMLElement | null>) => ReactNode;
}) {
	const [open, setOpen] = useState(false);
	const fallbackRef = useRef<HTMLButtonElement>(null);
	return (
		<>
			<button ref={fallbackRef} type="button" onClick={() => setOpen(true)}>Open history</button>
			{open ? children(() => setOpen(false), fallbackRef) : null}
		</>
	);
}

function rect(top: number, bottom: number): DOMRect {
	return {
		x: 0,
		y: top,
		top,
		bottom,
		left: 0,
		right: 100,
		width: 100,
		height: bottom - top,
		toJSON: () => ({}),
	};
}
