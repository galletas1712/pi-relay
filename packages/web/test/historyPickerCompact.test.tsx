// @vitest-environment jsdom

import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeAll, describe, expect, it, vi } from "vitest";
import {
	HistoryTargetPickerDialog,
	historyTargetOptions,
} from "../src/historyPickerCompact.tsx";
import type { HistoryTargetsResult } from "../src/types.ts";

beforeAll(() => {
	class ResizeObserver {
		observe() {}
		unobserve() {}
		disconnect() {}
	}
	vi.stubGlobal("ResizeObserver", ResizeObserver);
	HTMLElement.prototype.hasPointerCapture ??= () => false;
	HTMLElement.prototype.setPointerCapture ??= () => {};
	HTMLElement.prototype.releasePointerCapture ??= () => {};
});

afterEach(cleanup);

const baseTime = Date.UTC(2026, 0, 1, 12, 0, 0);

describe("server-projected history targets", () => {
	it("keeps the history layout class and renders newest targets first", async () => {
		const onLoadMore = vi.fn();
		const onSelect = vi.fn();
		const targets = historyTargetOptions(historyTargetsPage());
		render(
			<HistoryTargetPickerDialog
				targets={targets}
				mode="switch"
				loading={false}
				submitting={false}
				error={null}
				hasMore
				onLoadMore={onLoadMore}
				onClose={() => undefined}
				onSelect={onSelect}
			/>,
		);

		const dialog = screen.getByRole("dialog", { name: "Switch branch" });
		expect(dialog.classList.contains("history-dialog")).toBe(true);
		expect(dialog.querySelector(".history-dialog-head")).toBeTruthy();
		expect(dialog.querySelector(".history-options")).toBeTruthy();
		const buttons = screen.getAllByRole("button", { name: /Switch to User message/ });
		expect(buttons.map((button) => button.textContent)).toEqual([
			expect.stringContaining("latest message"),
			expect.stringContaining("oldest message"),
		]);
		expect(screen.getByText("active branch", { exact: false })).toBeTruthy();
		expect(screen.getByText("alternate history", { exact: false })).toBeTruthy();
		await userEvent.click(screen.getByRole("button", { name: "Load older messages" }));
		expect(onLoadMore).toHaveBeenCalledOnce();
		await userEvent.click(buttons[0]!);
		expect(onSelect).toHaveBeenCalledWith(targets[0]);
	});

	it("locks dismissal and selection while submitting", () => {
		const onClose = vi.fn();
		const onSelect = vi.fn();
		render(
			<HistoryTargetPickerDialog
				targets={historyTargetOptions(historyTargetsPage())}
				mode="fork"
				loading={false}
				submitting
				error={null}
				hasMore
				onLoadMore={vi.fn()}
				onClose={onClose}
				onSelect={onSelect}
			/>,
		);

		const dialog = screen.getByRole("dialog", { name: "Fork session" });
		expect((screen.getByRole("button", { name: "close picker" }) as HTMLButtonElement).disabled).toBe(true);
		expect((screen.getAllByRole("button", {
			name: /Fork from User message/,
		})[0] as HTMLButtonElement).disabled).toBe(true);
		fireEvent.keyDown(dialog, { key: "Escape" });
		fireEvent.pointerDown(document.querySelector(".dialog-overlay")!);
		expect(screen.getByRole("dialog", { name: "Fork session" })).toBeTruthy();
		expect(onClose).not.toHaveBeenCalled();
		expect(onSelect).not.toHaveBeenCalled();
	});
});

function historyTargetsPage(): HistoryTargetsResult {
	return {
		session_id: "session-1",
		active_leaf_id: "active-finish",
		session_revision: 4,
		transcript_revision: 9,
		before_sequence: null,
		next_before_sequence: 1,
		has_more: true,
		targets: [
			{
				entry_id: "latest-user",
				target_leaf_id: "previous-finish",
				timestamp_ms: baseTime + 2,
				turn_id: 20,
				is_on_active_branch: true,
				preview: "latest message",
			},
			{
				entry_id: "older-user",
				target_leaf_id: null,
				timestamp_ms: baseTime + 1,
				turn_id: 1,
				is_on_active_branch: false,
				preview: "oldest message",
			},
		],
	};
}
