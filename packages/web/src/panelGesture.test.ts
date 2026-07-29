// @vitest-environment jsdom

import { describe, expect, it } from "vitest";
import {
	PANEL_GESTURE_SWIPE_THRESHOLD_PX,
	PanelGestureController,
	type PanelGesturePanelState,
	type PanelGesturePointer,
} from "./panelGesture.ts";

const compact = "compact" as const;
const closed: PanelGesturePanelState = { sidebarOpen: false, rightOpen: false };

function pointer(
	pointerId: number,
	clientX: number,
	clientY = 100,
	target: EventTarget | null = document.createElement("main"),
): PanelGesturePointer {
	return {
		pointerId,
		pointerType: "touch",
		isPrimary: true,
		clientX,
		clientY,
		target,
	};
}

function swipe(
	controller: PanelGestureController,
	start: PanelGesturePointer,
	end: PanelGesturePointer,
	state = closed,
) {
	expect(controller.start(start, compact)).toBe(true);
	expect(controller.move(end).preventDefault).toBe(true);
	return controller.end(end, compact, state);
}

describe("PanelGestureController", () => {
	it("opens the left or right panel from a closed compact layout", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(swipe(controller, pointer(1, 200), pointer(1, 260))).toBe("open-sidebar");
		expect(swipe(controller, pointer(2, 200), pointer(2, 140))).toBe("open-inspector");
	});

	it("only closes an open drawer in its outward direction", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(
			swipe(controller, pointer(1, 200), pointer(1, 140), { sidebarOpen: true, rightOpen: false }),
		).toBe("close-sidebar");
		expect(
			swipe(controller, pointer(2, 200), pointer(2, 260), { sidebarOpen: false, rightOpen: true }),
		).toBe("close-inspector");
		expect(
			swipe(controller, pointer(3, 200), pointer(3, 260), { sidebarOpen: true, rightOpen: false }),
		).toBeNull();
		expect(
			swipe(controller, pointer(4, 200), pointer(4, 140), { sidebarOpen: false, rightOpen: true }),
		).toBeNull();
	});

	it("does not track medium or wide layouts", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), "medium")).toBe(false);
		expect(controller.start(pointer(2, 200), "wide")).toBe(false);
	});

	it("rejects starts in the outer viewport edge guard", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 31), compact)).toBe(false);
		expect(controller.start(pointer(2, 469), compact)).toBe(false);
		expect(controller.start(pointer(3, 32), compact)).toBe(true);
		controller.reset();
	});

	it("rejects mouse and secondary pointers", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start({ ...pointer(1, 200), pointerType: "mouse" }, compact)).toBe(false);
		expect(controller.start({ ...pointer(2, 200), isPrimary: false }, compact)).toBe(false);
	});

	it("rejects interactive and explicitly ignored targets", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		for (const target of [
			document.createElement("button"),
			document.createElement("a"),
			document.createElement("input"),
			document.createElement("textarea"),
			document.createElement("select"),
			document.createElement("label"),
		]) {
			expect(controller.start(pointer(1, 200, 100, target), compact)).toBe(false);
		}
		const editable = document.createElement("div");
		editable.contentEditable = "true";
		expect(controller.start(pointer(1, 200, 100, editable), compact)).toBe(false);
		const ignored = document.createElement("div");
		ignored.dataset.panelGestureIgnore = "";
		expect(controller.start(pointer(1, 200, 100, ignored), compact)).toBe(false);
	});

	it("rejects starts on the mobile top bar and resize handle", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		for (const className of ["mobile-topbar", "sidebar-resize-handle"]) {
			const target = document.createElement("div");
			target.className = className;
			expect(controller.start(pointer(1, 200, 100, target), compact)).toBe(false);
		}
	});

	it("does not claim horizontally scrollable transcript content", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		const markdown = document.createElement("div");
		markdown.className = "assistant-markdown";
		const pre = document.createElement("pre");
		const code = document.createElement("code");
		pre.append(code);
		markdown.append(pre);
		expect(controller.start(pointer(1, 200, 100, code), compact)).toBe(false);
	});

	it("cancels vertical-dominant movement without preventing scrolling", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), compact)).toBe(true);
		expect(controller.move(pointer(1, 212, 140)).preventDefault).toBe(false);
		expect(controller.end(pointer(1, 260, 140), compact, closed)).toBeNull();
	});

	it("does not cancel a diagonal gesture that turns into vertical scrolling", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), compact)).toBe(true);
		expect(controller.move(pointer(1, 211, 109)).preventDefault).toBe(false);
		expect(controller.move(pointer(1, 212, 140)).preventDefault).toBe(false);
		expect(controller.end(pointer(1, 260, 140), compact, closed)).toBeNull();
	});

	it("does not open a panel for a near-diagonal swipe endpoint", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), compact)).toBe(true);
		expect(controller.move(pointer(1, 213)).preventDefault).toBe(false);
		expect(controller.end(pointer(1, 260, 155), compact, closed)).toBeNull();
	});

	it("commits intent at the short threshold but waits for the swipe threshold to act", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), compact)).toBe(true);
		expect(controller.move(pointer(1, 213)).preventDefault).toBe(false);
		expect(controller.move(pointer(1, 220)).preventDefault).toBe(false);
		expect(controller.end(pointer(1, 200 + PANEL_GESTURE_SWIPE_THRESHOLD_PX - 1), compact, closed)).toBeNull();
		expect(swipe(controller, pointer(2, 200), pointer(2, 200 + PANEL_GESTURE_SWIPE_THRESHOLD_PX))).toBe(
			"open-sidebar",
		);
	});

	it("does not produce an action until pointer end", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), compact)).toBe(true);
		expect(controller.move(pointer(1, 260)).preventDefault).toBe(true);
		expect(controller.activePointerId).toBe(1);
		expect(controller.end(pointer(1, 260), compact, closed)).toBe("open-sidebar");
		expect(controller.activePointerId).toBeNull();
	});

	it("handles cancellation and stale pointer ids without clearing a newer gesture", () => {
		const controller = new PanelGestureController({ viewportWidth: () => 500 });
		expect(controller.start(pointer(1, 200), compact)).toBe(true);
		expect(controller.end(pointer(99, 260), compact, closed)).toBeNull();
		expect(controller.activePointerId).toBe(1);
		expect(controller.cancel(99)).toBe(false);
		expect(controller.activePointerId).toBe(1);
		expect(controller.cancel(1)).toBe(true);
		expect(controller.activePointerId).toBeNull();
		expect(controller.end(pointer(1, 260), compact, closed)).toBeNull();

		expect(controller.start(pointer(2, 200), compact)).toBe(true);
		expect(controller.cancel()).toBe(true);
		expect(controller.end(pointer(2, 260), compact, closed)).toBeNull();
	});
});
